//! `eks exec`, end to end: the real binary against a stand-in API server.
//!
//! The unit tests prove each decision on its own — which pod a prefix means,
//! what a status says, how bytes are carried. What they cannot prove is that
//! the pieces are wired together: that the binary really pipes its stdin
//! into the session, ends with the remote command's exit code, and prints
//! the sentence each failure deserves. A reviewer has no cluster to try it
//! on, so this file plays one — a socket on localhost that answers the few
//! requests `eks exec` makes, and speaks the `v5.channel.k8s.io` WebSocket
//! protocol a kubelet's exec stream does, with a handful of scripted
//! commands behind it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;

/// Start the stand-in API server on a thread of its own and return its URL.
fn serve() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let listener = TcpListener::from_std(listener).unwrap();
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(connection(socket));
            }
        });
    });

    url
}

/// One client connection: plain requests answered in turn until one asks to
/// upgrade, which then becomes an exec stream.
async fn connection(mut socket: TcpStream) {
    loop {
        let Some(head) = read_head(&mut socket).await else {
            return;
        };
        let target = head
            .lines()
            .next()
            .and_then(|line| line.split(' ').nth(1))
            .unwrap_or_default()
            .to_owned();
        let lower = head.to_ascii_lowercase();

        if lower.contains("upgrade: websocket") {
            exec(socket, &head, &target).await;
            return;
        }

        let (status, body) = route(&target);
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );
        if socket.write_all(response.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// Read up to the blank line ending a request's head. Byte by byte, so
/// nothing after it — the first WebSocket frame — is swallowed here.
async fn read_head(socket: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let byte = socket.read_u8().await.ok()?;
        head.push(byte);
    }
    String::from_utf8(head).ok()
}

/// What each plain request is answered with.
fn route(target: &str) -> (&'static str, String) {
    let path = target.split('?').next().unwrap_or_default();
    match path {
        "/api/v1/namespaces/default/pods" => (
            "200 OK",
            list(
                "PodList",
                &[
                    pod("api-7d9f-xk2", "default", "Running"),
                    pod("web-1", "default", "Running"),
                    pod("web-2", "default", "Running"),
                    pod("pending-1", "default", "Pending"),
                    pod("locked-1", "default", "Running"),
                ],
            ),
        ),
        "/api/v1/pods" => (
            "200 OK",
            list(
                "PodList",
                &[
                    pod("api-7d9f-xk2", "default", "Running"),
                    pod("shopfront-1", "shop", "Running"),
                ],
            ),
        ),
        "/api/v1/namespaces/default/events" => (
            "200 OK",
            list(
                "EventList",
                &[r#"{"metadata":{"name":"e1","namespace":"default"},
                     "involvedObject":{"kind":"Pod","name":"pending-1","namespace":"default"},
                     "reason":"FailedScheduling","type":"Warning","count":3,
                     "message":"0/3 nodes are available: 3 Insufficient cpu.",
                     "lastTimestamp":"2026-10-03T11:59:00Z"}"#
                    .to_owned()],
            ),
        ),
        _ => (
            "404 Not Found",
            r#"{"kind":"Status","apiVersion":"v1","status":"Failure","reason":"NotFound","code":404}"#
                .to_owned(),
        ),
    }
}

fn list(kind: &str, items: &[String]) -> String {
    format!(
        r#"{{"kind":"{kind}","apiVersion":"v1","metadata":{{"resourceVersion":"1"}},"items":[{}]}}"#,
        items.join(",")
    )
}

fn pod(name: &str, namespace: &str, phase: &str) -> String {
    let state = if phase == "Running" {
        r#"{"running":{"startedAt":"2026-10-03T11:00:00Z"}}"#
    } else {
        r#"{"waiting":{"reason":"ContainerCreating"}}"#
    };
    format!(
        r#"{{"metadata":{{"name":"{name}","namespace":"{namespace}","creationTimestamp":"2026-10-03T11:00:00Z"}},
            "spec":{{"nodeName":"node-1","containers":[{{"name":"app","image":"app:1"}}]}},
            "status":{{"phase":"{phase}","containerStatuses":[{{"name":"app","ready":true,"restartCount":0,
              "image":"app:1","imageID":"","state":{state}}}]}}}}"#
    )
}

/// An exec stream. The command in the query decides what the "container"
/// does; a pod whose name starts `locked-` refuses the upgrade the way an
/// API server does for a role without `create` on `pods/exec`.
async fn exec(mut socket: TcpStream, head: &str, target: &str) {
    if target.contains("/pods/locked-") {
        let body = r#"{"kind":"Status","status":"Failure","reason":"Forbidden","code":403}"#;
        let _ = socket
            .write_all(
                format!(
                    "HTTP/1.1 403 Forbidden\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await;
        return;
    }

    // Header names are case-insensitive; the key's value is not.
    let key = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("sec-websocket-key")
                .then_some(value.trim())
        })
        .unwrap_or_default();
    let accept = derive_accept_key(key.as_bytes());
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\nSec-WebSocket-Protocol: v5.channel.k8s.io\r\n\r\n"
    );
    socket.write_all(response.as_bytes()).await.unwrap();
    let mut ws = WebSocketStream::from_raw_socket(socket, Role::Server, None).await;

    let command = query_values(target, "command");
    let program = command.first().map(String::as_str).unwrap_or_default();
    match program {
        // Echo stdin back until the client says stdin is closed.
        "cat" => {
            while let Some(Ok(message)) = ws.next().await {
                let Message::Binary(frame) = message else {
                    continue;
                };
                match frame.first() {
                    Some(0) => {
                        let mut out = vec![1];
                        out.extend_from_slice(&frame[1..]);
                        ws.send(Message::binary(out)).await.unwrap();
                    }
                    Some(255) => break,
                    _ => {}
                }
            }
            status(&mut ws, r#"{"status":"Success"}"#).await;
        }
        "exit3" => {
            ws.send(Message::binary(b"\x01bye\n".to_vec()))
                .await
                .unwrap();
            ws.send(Message::binary(b"\x02oops\n".to_vec()))
                .await
                .unwrap();
            status(
                &mut ws,
                r#"{"status":"Failure","reason":"NonZeroExitCode","message":"exit code 3",
                    "details":{"causes":[{"reason":"ExitCode","message":"3"}]}}"#,
            )
            .await;
        }
        // Every shell `eks` tries is missing: a distroless image.
        "/bin/sh" | "/bin/bash" => {
            status(
                &mut ws,
                &format!(
                    r#"{{"status":"Failure","reason":"InternalError","message":"OCI runtime exec failed: exec: \"{program}\": stat {program}: no such file or directory: unknown"}}"#
                ),
            )
            .await;
        }
        _ => {
            status(
                &mut ws,
                &format!(
                    r#"{{"status":"Failure","reason":"InternalError","message":"exec: \"{program}\": executable file not found in $PATH: unknown"}}"#
                ),
            )
            .await;
        }
    }
    let _ = ws.close(None).await;
    // Keep reading until the client closes too, as an API server's stdin
    // reader does. Dropping the socket with the client's frames still unread
    // makes the kernel answer with a reset, which discards the status the
    // client has not read yet — a failure of this stand-in, not of `eks`.
    while let Some(Ok(_)) = ws.next().await {}
}

async fn status(ws: &mut WebSocketStream<TcpStream>, json: &str) {
    let mut frame = vec![3];
    frame.extend_from_slice(json.as_bytes());
    ws.send(Message::binary(frame)).await.unwrap();
}

/// Every value of `name` in a request target's query, percent-decoded.
fn query_values(target: &str, name: &str) -> Vec<String> {
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or_default();
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter(|(key, _)| *key == name)
        .map(|(_, value)| decode(value))
        .collect()
}

fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
                out.push(u8::from_str_radix(hex, 16).unwrap());
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap()
}

/// A kubeconfig whose one context points at `url` with a static token, so
/// no credential helper or AWS login is involved.
fn kubeconfig(dir: &Path, url: &str) -> std::path::PathBuf {
    let path = dir.join("config");
    std::fs::write(
        &path,
        format!(
            "apiVersion: v1\nkind: Config\ncurrent-context: test\n\
             clusters:\n- name: test\n  cluster:\n    server: {url}\n\
             users:\n- name: test\n  user:\n    token: fake\n\
             contexts:\n- name: test\n  context:\n    cluster: test\n    user: test\n    namespace: default\n"
        ),
    )
    .unwrap();
    path
}

/// Run `eks` with `args` against a fresh stand-in server, feeding it `stdin`.
fn eks(args: &[&str], stdin: &[u8]) -> Output {
    let url = serve();
    let home = tempfile::tempdir().unwrap();
    let config = kubeconfig(home.path(), &url);

    let mut child = Command::new(env!("CARGO_BIN_EXE_eks"))
        .args(["--login", "never", "--timeout", "10s"])
        .args(args)
        .env("KUBECONFIG", &config)
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env("NO_COLOR", "1")
        // The "cluster" is a socket on this machine; a proxy from the
        // environment would be asked for it instead.
        .env_remove("HTTPS_PROXY")
        .env_remove("https_proxy")
        .env_remove("HTTP_PROXY")
        .env_remove("http_proxy")
        .env_remove("ALL_PROXY")
        .env_remove("all_proxy")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn piped_stdin_reaches_the_remote_command_and_its_output_comes_back() {
    let output = eks(&["exec", "api", "--", "cat"], b"hi\n");

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(output.stdout, b"hi\n");
}

#[test]
fn the_remote_commands_exit_code_becomes_eks_own() {
    let output = eks(&["exec", "api-7d9f-xk2", "--", "exit3"], b"");

    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert_eq!(output.stdout, b"bye\n");
    assert_eq!(stderr(&output), "oops\n");
}

#[test]
fn an_ambiguous_prefix_lists_its_candidates() {
    let output = eks(&["exec", "web", "--", "cat"], b"");
    let text = stderr(&output);

    assert_eq!(output.status.code(), Some(1));
    assert!(text.contains("2 pods start with \"web\""), "{text}");
    assert!(text.contains("default    web-1"), "{text}");
    assert!(text.contains("default    web-2"), "{text}");
}

#[test]
fn a_pod_in_another_namespace_is_named_with_the_flag_to_reach_it() {
    let output = eks(&["exec", "shopfront", "--", "cat"], b"");
    let text = stderr(&output);

    assert_eq!(output.status.code(), Some(1));
    assert!(text.contains("pass `-n shop`"), "{text}");
}

#[test]
fn a_pending_pod_prints_its_phase_and_events() {
    let output = eks(&["exec", "pending", "--", "cat"], b"");
    let text = stderr(&output);

    assert_eq!(output.status.code(), Some(1));
    assert!(text.contains("pod pending-1 is Pending"), "{text}");
    assert!(
        text.contains("FailedScheduling: 0/3 nodes are available: 3 Insufficient cpu. (x3)"),
        "{text}"
    );
}

#[test]
fn a_forbidden_exec_names_the_missing_rbac_verb() {
    let output = eks(&["exec", "locked", "--", "cat"], b"");
    let text = stderr(&output);

    assert_eq!(output.status.code(), Some(1));
    assert!(text.contains("`create` verb on `pods/exec`"), "{text}");
}

#[test]
fn an_image_without_a_shell_points_at_a_debug_container() {
    let output = eks(&["exec", "api"], b"");
    let text = stderr(&output);

    assert_eq!(output.status.code(), Some(1));
    assert!(text.contains("has no shell"), "{text}");
    assert!(
        text.contains("kubectl debug -it api-7d9f-xk2 -n default --image=busybox --target=app"),
        "{text}"
    );
}

#[test]
fn a_command_that_is_not_in_the_image_says_so() {
    let output = eks(&["exec", "api", "--", "htop"], b"");
    let text = stderr(&output);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        text.contains("htop is not in container app of pod api-7d9f-xk2"),
        "{text}"
    );
}

#[test]
fn a_container_the_pod_does_not_have_is_named_alongside_the_real_ones() {
    let output = eks(&["exec", "api", "-C", "sidecar", "--", "cat"], b"");
    let text = stderr(&output);

    assert_eq!(output.status.code(), Some(1));
    assert!(text.contains("no container called \"sidecar\""), "{text}");
    assert!(text.contains("Its containers are app"), "{text}");
}
