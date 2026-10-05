//! `eks port-forward`, end to end: the real binary against a stand-in API
//! server.
//!
//! The unit tests prove each decision — which pod, which port, when to move.
//! What only the whole binary can show is that bytes really cross: that a
//! connection to the printed URL reaches the pod, that two connections are
//! two streams, that a rollout behind a service is followed without the
//! client noticing, and that Ctrl-C ends it cleanly. The stand-in answers
//! the few requests `eks port-forward` makes and speaks the WebSocket
//! port-forward protocol a kubelet does, with each "pod" an echo server
//! that says who it is.
//!
//! The stand-in shares nothing with `tests/exec.rs` on purpose: each file
//! is one command's fixture, readable top to bottom.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpStream as StdStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;

/// What the stand-in cluster looks like, shared with the test driving it.
#[derive(Default)]
struct Cluster {
    /// Once set, pod `api-7d9f-aaaaa` has been replaced by `api-7d9f-bbbbb`.
    rolled: AtomicBool,
    /// How many port-forward streams were opened.
    streams: AtomicUsize,
}

/// Start the stand-in on a thread of its own; return its URL and its state.
fn serve() -> (String, Arc<Cluster>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let cluster = Arc::new(Cluster::default());
    let shared = Arc::clone(&cluster);

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
                tokio::spawn(connection(socket, Arc::clone(&shared)));
            }
        });
    });

    (url, cluster)
}

async fn connection(mut socket: TcpStream, cluster: Arc<Cluster>) {
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
        let body = read_body(&mut socket, &head).await;

        if head.to_ascii_lowercase().contains("upgrade: websocket") {
            forward(socket, &head, &target, &cluster).await;
            return;
        }

        let (status, body) = route(&target, &body, &cluster);
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );
        if socket.write_all(response.as_bytes()).await.is_err() {
            return;
        }
    }
}

async fn read_head(socket: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.push(socket.read_u8().await.ok()?);
    }
    String::from_utf8(head).ok()
}

/// A request's body, for the one `POST` this command makes.
async fn read_body(socket: &mut TcpStream, head: &str) -> String {
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0; length];
    socket.read_exact(&mut body).await.unwrap();
    String::from_utf8(body).unwrap()
}

const POD_A: &str = "api-7d9f-aaaaa";
const POD_B: &str = "api-7d9f-bbbbb";

/// The pods behind `app=api`: one before the rollout, its successor after.
fn api_pods(cluster: &Cluster) -> Vec<String> {
    if cluster.rolled.load(Ordering::SeqCst) {
        vec![pod(POD_B, "api", &[("http", 8080)])]
    } else {
        vec![pod(POD_A, "api", &[("http", 8080)])]
    }
}

fn other_pods() -> Vec<String> {
    vec![
        pod("single-1", "single", &[("http", 38080)]),
        pod("busy-1", "busy", &[("http", 38081)]),
        pod("multi-1", "multi", &[("http", 8080), ("metrics", 9090)]),
        pod("refuse-1", "refuse", &[("http", 8080)]),
    ]
}

fn route(target: &str, body: &str, cluster: &Cluster) -> (&'static str, String) {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut pods = api_pods(cluster);
    pods.extend(other_pods());

    if path == "/apis/authorization.k8s.io/v1/selfsubjectaccessreviews" {
        let allowed = !body.contains(r#""namespace":"locked""#);
        return (
            "201 Created",
            format!(
                r#"{{"apiVersion":"authorization.k8s.io/v1","kind":"SelfSubjectAccessReview",
                    "metadata":{{}},"spec":{{}},"status":{{"allowed":{allowed}}}}}"#
            ),
        );
    }
    if let Some(name) = path.strip_prefix("/api/v1/namespaces/default/pods/")
        && let Some(found) = pods
            .iter()
            .find(|pod| pod.contains(&format!(r#""name":"{name}""#)))
    {
        return ("200 OK", found.clone());
    }
    match path {
        "/api/v1/namespaces/default/pods" => {
            let selected: Vec<String> = match query_value(query, "labelSelector").as_deref() {
                Some("app=api") => api_pods(cluster),
                Some(selector) => {
                    let app = selector.trim_start_matches("app=");
                    pods.into_iter()
                        .filter(|pod| pod.contains(&format!(r#""app":"{app}""#)))
                        .collect()
                }
                None => pods,
            };
            ("200 OK", list("PodList", &selected))
        }
        "/api/v1/namespaces/default/services" => (
            "200 OK",
            list(
                "ServiceList",
                &[
                    service("api", Some("api"), &[(Some("web"), 80, r#""http""#)]),
                    service(
                        "multi",
                        Some("multi"),
                        &[(Some("web"), 80, "8080"), (Some("metrics"), 9090, "9090")],
                    ),
                    service("orphan", None, &[(None, 80, "80")]),
                ],
            ),
        ),
        "/api/v1/services" => (
            "200 OK",
            list(
                "ServiceList",
                &[service_in(
                    "db",
                    "data",
                    Some("db"),
                    &[(None, 5432, "5432")],
                )],
            ),
        ),
        "/apis/apps/v1/namespaces/default/deployments" => (
            "200 OK",
            list(
                "DeploymentList",
                &[r#"{"metadata":{"name":"api","namespace":"default"},
                     "spec":{"selector":{"matchLabels":{"app":"api"}},
                             "template":{"metadata":{},"spec":{"containers":[]}}}}"#
                    .to_owned()],
            ),
        ),
        _ => not_found(),
    }
}

fn not_found() -> (&'static str, String) {
    (
        "404 Not Found",
        r#"{"kind":"Status","apiVersion":"v1","status":"Failure","reason":"NotFound","code":404}"#
            .to_owned(),
    )
}

fn list(kind: &str, items: &[String]) -> String {
    format!(
        r#"{{"kind":"{kind}","apiVersion":"v1","metadata":{{"resourceVersion":"1"}},"items":[{}]}}"#,
        items.join(",")
    )
}

/// A running, ready pod labelled `app=<app>` declaring `ports`. The `api`
/// pods belong to the `ReplicaSet` `api-7d9f`, as a `Deployment`'s would.
fn pod(name: &str, app: &str, ports: &[(&str, u16)]) -> String {
    let ports = ports
        .iter()
        .map(|(port_name, number)| format!(r#"{{"name":"{port_name}","containerPort":{number}}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let owner = if app == "api" {
        r#","ownerReferences":[{"apiVersion":"apps/v1","kind":"ReplicaSet","name":"api-7d9f","uid":"1","controller":true}]"#
    } else {
        ""
    };
    format!(
        r#"{{"metadata":{{"name":"{name}","namespace":"default","creationTimestamp":"2026-10-05T11:00:00Z",
              "labels":{{"app":"{app}","pod-template-hash":"7d9f"}}{owner}}},
            "spec":{{"nodeName":"node-1","containers":[{{"name":"app","image":"app:1","ports":[{ports}]}}]}},
            "status":{{"phase":"Running","conditions":[{{"type":"Ready","status":"True"}}],
              "containerStatuses":[{{"name":"app","ready":true,"restartCount":0,"image":"app:1","imageID":"",
                "state":{{"running":{{"startedAt":"2026-10-05T11:00:00Z"}}}}}}]}}}}"#
    )
}

fn service(name: &str, app: Option<&str>, ports: &[(Option<&str>, u16, &str)]) -> String {
    service_in(name, "default", app, ports)
}

fn service_in(
    name: &str,
    namespace: &str,
    app: Option<&str>,
    ports: &[(Option<&str>, u16, &str)],
) -> String {
    let selector = app
        .map(|app| format!(r#","selector":{{"app":"{app}"}}"#))
        .unwrap_or_default();
    let ports = ports
        .iter()
        .map(|(port_name, port, target)| {
            let port_name = port_name
                .map(|n| format!(r#""name":"{n}","#))
                .unwrap_or_default();
            format!(r#"{{{port_name}"port":{port},"targetPort":{target},"protocol":"TCP"}}"#)
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"{{"metadata":{{"name":"{name}","namespace":"{namespace}"}},
            "spec":{{"ports":[{ports}]{selector}}}}}"#
    )
}

fn query_value(query: &str, name: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.replace("%3D", "=").replace("%2C", ","))
}

/// A port-forward stream. The pod echoes each chunk back prefixed with its
/// own name and port; `refuse-1` has nothing listening, so it sends the
/// kubelet's error instead. A pod that has gone answers `404`, as an API
/// server does.
async fn forward(mut socket: TcpStream, head: &str, target: &str, cluster: &Cluster) {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let pod = path
        .strip_prefix("/api/v1/namespaces/default/pods/")
        .and_then(|rest| rest.strip_suffix("/portforward"))
        .unwrap_or_default()
        .to_owned();
    let exists = api_pods(cluster)
        .iter()
        .chain(&other_pods())
        .any(|p| p.contains(&format!(r#""name":"{pod}""#)));
    if !exists {
        let (line, body) = not_found();
        let _ = socket
            .write_all(
                format!(
                    "HTTP/1.1 {line}\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await;
        return;
    }
    cluster.streams.fetch_add(1, Ordering::SeqCst);

    let key = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("sec-websocket-key")
                .then_some(value.trim())
        })
        .unwrap_or_default();
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\nSec-WebSocket-Protocol: v4.channel.k8s.io\r\n\r\n",
        derive_accept_key(key.as_bytes())
    );
    socket.write_all(response.as_bytes()).await.unwrap();
    let mut ws = WebSocketStream::from_raw_socket(socket, Role::Server, None).await;

    let port: u16 = query_value(query, "ports").unwrap().parse().unwrap();
    let [low, high] = port.to_le_bytes();
    // Each channel opens with the port it carries: data on 0, errors on 1.
    ws.send(Message::binary(vec![0, low, high])).await.unwrap();
    ws.send(Message::binary(vec![1, low, high])).await.unwrap();

    if pod == "refuse-1" {
        let mut frame = vec![1];
        frame.extend_from_slice(
            format!(
                "error forwarding port {port} to pod 1f2e, uid : failed to connect to localhost:{port} \
                 inside namespace \"1f2e\", IPv4: dial tcp4 127.0.0.1:{port}: connect: connection refused"
            )
            .as_bytes(),
        );
        ws.send(Message::binary(frame)).await.unwrap();
        let _ = ws.close(None).await;
        while let Some(Ok(_)) = ws.next().await {}
        return;
    }

    while let Some(Ok(message)) = ws.next().await {
        let Message::Binary(frame) = message else {
            if message.is_close() {
                break;
            }
            continue;
        };
        if frame.first() == Some(&0) && frame.len() > 1 {
            let mut out = vec![0];
            out.extend_from_slice(format!("{pod}:{port} ").as_bytes());
            out.extend_from_slice(&frame[1..]);
            if ws.send(Message::binary(out)).await.is_err() {
                break;
            }
        }
    }
}

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

/// A running `eks port-forward`, killed when dropped.
struct Forward {
    child: Child,
    stdout: mpsc::Receiver<String>,
    stderr: Arc<Mutex<String>>,
    cluster: Arc<Cluster>,
    _home: tempfile::TempDir,
}

impl Forward {
    fn start(args: &[&str]) -> Self {
        let (url, cluster) = serve();
        let home = tempfile::tempdir().unwrap();
        let config = kubeconfig(home.path(), &url);
        let mut child = Command::new(env!("CARGO_BIN_EXE_eks"))
            .args(["--login", "never", "--timeout", "10s", "port-forward"])
            .args(args)
            .env("KUBECONFIG", &config)
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path())
            .env("NO_COLOR", "1")
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

        let (tx, stdout) = mpsc::channel();
        let out = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { return };
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&stderr);
        let mut err = child.stderr.take().unwrap();
        std::thread::spawn(move || {
            let mut buffer = [0; 1024];
            while let Ok(n) = err.read(&mut buffer) {
                if n == 0 {
                    return;
                }
                sink.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buffer[..n]));
            }
        });

        Self {
            child,
            stdout,
            stderr,
            cluster,
            _home: home,
        }
    }

    /// The next line on stdout: one forward's URL and where it goes.
    fn line(&self) -> String {
        self.stdout
            .recv_timeout(Duration::from_secs(20))
            .unwrap_or_else(|_| panic!("no forward line; stderr:\n{}", self.stderr()))
    }

    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    /// Wait until stderr contains `text`.
    fn wait_for(&self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !self.stderr().contains(text) {
            assert!(
                Instant::now() < deadline,
                "stderr never said {text:?}:\n{}",
                self.stderr()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Wait for the process to end on its own.
    fn exit_code(&mut self) -> Option<i32> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                // Let the reader thread drain the last of stderr.
                std::thread::sleep(Duration::from_millis(100));
                return status.code();
            }
            assert!(
                Instant::now() < deadline,
                "eks did not exit:\n{}",
                self.stderr()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Ctrl-C, as a terminal would send it.
    fn interrupt(&mut self) -> Option<i32> {
        let status = Command::new("kill")
            .args(["-INT", &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        self.exit_code()
    }
}

impl Drop for Forward {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The port in a forward line's URL.
fn port_of(line: &str) -> u16 {
    let url = line.split_whitespace().next().unwrap();
    url.rsplit(':').next().unwrap().parse().unwrap()
}

/// Send `text` down a fresh connection and read the pod's echo of it.
///
/// The write side is left open until the answer is in: closing it first
/// ends the forward's stream before the pod's reply has crossed, as it does
/// for `kubectl port-forward`. Real clients — browsers, `curl`, database
/// drivers — read their answer before they close.
fn ask(port: u16, text: &str) -> String {
    let mut stream = StdStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    stream.write_all(text.as_bytes()).unwrap();
    read_until(&mut stream, text)
}

fn read_until(stream: &mut StdStream, text: &str) -> String {
    let mut answer = Vec::new();
    let mut buffer = [0; 256];
    while !String::from_utf8_lossy(&answer).ends_with(text) {
        let n = stream.read(&mut buffer).unwrap_or(0);
        if n == 0 {
            break;
        }
        answer.extend_from_slice(&buffer[..n]);
    }
    String::from_utf8_lossy(&answer).into_owned()
}

#[test]
fn a_service_forward_prints_a_url_whose_connections_reach_its_pod() {
    let forward = Forward::start(&["svc/api", ":80"]);
    let line = forward.line();

    assert!(line.starts_with("http://127.0.0.1:"), "{line}");
    assert!(
        line.ends_with(&format!("→ svc/api port 80 (pod {POD_A} port 8080)")),
        "{line}"
    );
    assert_eq!(ask(port_of(&line), "ping"), format!("{POD_A}:8080 ping"));
}

#[test]
fn concurrent_connections_each_get_their_own_stream() {
    let forward = Forward::start(&["single", ":38080"]);
    let port = port_of(&forward.line());

    let mut first = StdStream::connect(("127.0.0.1", port)).unwrap();
    let mut second = StdStream::connect(("127.0.0.1", port)).unwrap();
    for stream in [&first, &second] {
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
    }
    first.write_all(b"one").unwrap();
    second.write_all(b"two").unwrap();

    assert_eq!(read_until(&mut second, "two"), "single-1:38080 two");
    assert_eq!(read_until(&mut first, "one"), "single-1:38080 one");
    assert_eq!(forward.cluster.streams.load(Ordering::SeqCst), 2);
}

#[test]
fn a_rollout_behind_a_service_is_followed_without_dropping_the_next_connection() {
    let forward = Forward::start(&["svc/api", ":80"]);
    let port = port_of(&forward.line());
    assert_eq!(ask(port, "before"), format!("{POD_A}:8080 before"));

    forward.cluster.rolled.store(true, Ordering::SeqCst);

    // The first connection after the rollout finds its pod gone, wakes the
    // watch, and is carried to the successor rather than dropped.
    assert_eq!(ask(port, "after"), format!("{POD_B}:8080 after"));
    forward.wait_for(&format!(
        "pod {POD_A} is gone; svc/api now forwards to pod {POD_B}."
    ));
}

#[test]
fn a_deployment_forward_follows_its_selector_too() {
    let forward = Forward::start(&["deploy/api", ":http"]);
    let line = forward.line();
    assert!(
        line.ends_with(&format!("→ deploy/api (pod {POD_A} port 8080)")),
        "{line}"
    );

    forward.cluster.rolled.store(true, Ordering::SeqCst);
    // Without any connection to prompt it, the watch loop notices by itself.
    forward.wait_for(&format!("deploy/api now forwards to pod {POD_B}."));
    assert_eq!(ask(port_of(&line), "hello"), format!("{POD_B}:8080 hello"));
}

#[test]
fn a_pod_named_directly_that_is_deleted_ends_the_command_and_names_its_deployment() {
    let mut forward = Forward::start(&[POD_A, ":8080"]);
    let line = forward.line();
    assert!(
        line.ends_with(&format!("→ pod {POD_A} port 8080")),
        "{line}"
    );

    forward.cluster.rolled.store(true, Ordering::SeqCst);

    assert_eq!(forward.exit_code(), Some(1));
    let text = forward.stderr();
    assert!(
        text.contains(&format!(
            "eks: pod {POD_A} was deleted, so there is nothing left to forward to."
        )),
        "{text}"
    );
    assert!(text.contains("`eks port-forward deploy/api`"), "{text}");
}

#[test]
fn ctrl_c_closes_the_listeners_and_exits_cleanly() {
    let mut forward = Forward::start(&["single", ":38080"]);
    let port = port_of(&forward.line());
    forward.wait_for("Forwarding until Ctrl-C.");

    assert_eq!(forward.interrupt(), Some(0));
    assert!(forward.stderr().contains("Stopped forwarding."));
    assert!(
        StdStream::connect(("127.0.0.1", port)).is_err(),
        "the listener outlived the command"
    );
}

#[test]
fn an_address_beyond_loopback_is_called_out_on_its_line() {
    let forward = Forward::start(&["single", ":38080", "--address", "0.0.0.0"]);
    let line = forward.line();

    assert!(line.starts_with("http://127.0.0.1:"), "{line}");
    assert!(
        line.ends_with("(open to other machines: listening on every interface)"),
        "{line}"
    );
}

#[test]
fn a_label_selector_narrows_the_pods_a_prefix_is_matched_against() {
    // `s` alone means `single-1`. Selecting `app=busy` leaves it out of the
    // candidates, so the same prefix now matches nothing.
    let mut forward = Forward::start(&["s", ":38080", "-l", "app=busy"]);

    assert_eq!(forward.exit_code(), Some(1));
    let text = forward.stderr();
    assert!(
        text.contains("no pod in namespace default is called \"s\""),
        "{text}"
    );
}

#[test]
fn the_only_declared_port_is_used_when_none_is_named() {
    let forward = Forward::start(&["single"]);
    let line = forward.line();
    // On its own number when that is free here, which in a test run it
    // nearly always is; the fallback is the next test's subject.
    assert!(line.contains("→ pod single-1 port 38080"), "{line}");
}

#[test]
fn a_taken_local_port_falls_back_to_a_free_one_and_says_why() {
    // Hold the port the pod declares, so the forward cannot have it.
    let held = std::net::TcpListener::bind(("127.0.0.1", 38081));
    let forward = Forward::start(&["busy"]);
    let line = forward.line();

    assert!(line.contains("→ pod busy-1 port 38081"), "{line}");
    if held.is_ok() {
        assert!(line.ends_with("(local port 38081 is in use)"), "{line}");
        assert_ne!(port_of(&line), 38081);
    }
}

#[test]
fn a_taken_local_port_that_was_asked_for_is_an_error_with_a_way_out() {
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port();
    let mut forward = Forward::start(&["single", &format!("{port}:http")]);

    assert_eq!(forward.exit_code(), Some(1));
    let text = forward.stderr();
    assert!(
        text.contains(&format!(
            "could not listen on 127.0.0.1:{port}: something else is already listening there."
        )),
        "{text}"
    );
    assert!(text.contains("`:http`"), "{text}");
}

#[test]
fn several_ports_and_no_terminal_lists_them_and_says_how_to_choose() {
    let mut forward = Forward::start(&["multi"]);

    assert_eq!(forward.exit_code(), Some(1));
    let text = forward.stderr();
    assert!(text.contains("pod multi-1 has 2 ports"), "{text}");
    assert!(text.contains("metrics"), "{text}");
    assert!(text.contains("CONTAINER"), "{text}");
    assert!(text.contains("`eks port-forward multi http`"), "{text}");
}

#[test]
fn a_service_with_several_ports_lists_where_each_goes() {
    let mut forward = Forward::start(&["svc/multi"]);

    assert_eq!(forward.exit_code(), Some(1));
    let text = forward.stderr();
    assert!(text.contains("service multi has 2 ports"), "{text}");
    assert!(text.contains("pod port 9090"), "{text}");
}

#[test]
fn a_refused_connection_says_nothing_is_listening_and_the_forward_carries_on() {
    let forward = Forward::start(&["refuse", ":8080"]);
    let port = port_of(&forward.line());

    let answer = ask(port, "ping");
    assert_eq!(answer, "");
    forward.wait_for(
        "pod refuse-1 refused a connection on port 8080: nothing in it is listening there.",
    );
    // Still listening: one refused connection is not the end of the forward.
    assert!(StdStream::connect(("127.0.0.1", port)).is_ok());
}

#[test]
fn a_service_without_a_selector_says_to_forward_to_a_pod() {
    let mut forward = Forward::start(&["svc/orphan", "80"]);

    assert_eq!(forward.exit_code(), Some(1));
    assert!(forward.stderr().contains("service orphan has no selector"));
}

#[test]
fn a_service_in_another_namespace_is_named_with_the_flag_to_reach_it() {
    let mut forward = Forward::start(&["svc/db", "5432"]);

    assert_eq!(forward.exit_code(), Some(1));
    let text = forward.stderr();
    assert!(
        text.contains("no service in namespace default is called \"db\""),
        "{text}"
    );
    assert!(text.contains("pass `-n data`"), "{text}");
}

#[test]
fn a_missing_permission_is_reported_before_anything_listens() {
    let mut forward = Forward::start(&["svc/api", "80", "-n", "locked"]);

    assert_eq!(forward.exit_code(), Some(1));
    let text = forward.stderr();
    assert!(
        text.contains("`create` verb on `pods/portforward`"),
        "{text}"
    );
    assert!(!text.contains("Forwarding until"), "{text}");
}

#[test]
fn a_malformed_port_is_refused_before_connecting() {
    let mut forward = Forward::start(&["svc/api", "80:"]);

    assert_eq!(forward.exit_code(), Some(1));
    assert!(forward.stderr().contains("`LOCAL:REMOTE`"));
    assert_eq!(forward.cluster.streams.load(Ordering::SeqCst), 0);
}

/// The question asked when several ports could be meant, answered at a
/// terminal. `script(1)` gives `eks` a pseudo-terminal for stdin and stderr,
/// which is what decides there is somebody to ask, and carries what is typed
/// into it.
#[cfg(target_os = "linux")]
#[test]
fn at_a_terminal_several_ports_are_offered_and_the_answer_is_forwarded() {
    if Command::new("script").arg("--version").output().is_err() {
        eprintln!("skipped: script(1) is not installed");
        return;
    }
    let (url, _cluster) = serve();
    let home = tempfile::tempdir().unwrap();
    let config = kubeconfig(home.path(), &url);
    let eks = format!(
        "'{}' --login never --timeout 10s port-forward multi",
        env!("CARGO_BIN_EXE_eks")
    );

    let mut child = Command::new("script")
        .args(["-qec", &eks, "/dev/null"])
        .env("KUBECONFIG", &config)
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env("NO_COLOR", "1")
        .env_remove("HTTPS_PROXY")
        .env_remove("https_proxy")
        .env_remove("HTTP_PROXY")
        .env_remove("http_proxy")
        .env_remove("ALL_PROXY")
        .env_remove("all_proxy")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let (tx, lines) = mpsc::channel();
    let out = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        let mut out = BufReader::new(out);
        let mut seen = String::new();
        let mut buffer = [0; 256];
        while let Ok(n) = out.read(&mut buffer) {
            if n == 0 {
                return;
            }
            seen.push_str(&String::from_utf8_lossy(&buffer[..n]));
            if tx.send(seen.clone()).is_err() {
                return;
            }
        }
    });
    let wait_for = |text: &str| -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut last = String::new();
        while Instant::now() < deadline {
            if let Ok(seen) = lines.recv_timeout(Duration::from_millis(200)) {
                last = seen;
            }
            if last.contains(text) {
                return last;
            }
        }
        panic!("never saw {text:?} in:\n{last}");
    };

    let asked = wait_for("Forward which one?");
    assert!(asked.contains("pod multi-1 has 2 ports"), "{asked}");
    stdin.write_all(b"metrics\r").unwrap();
    stdin.flush().unwrap();

    wait_for("→ pod multi-1 port 9090");
    let _ = child.kill();
    let _ = child.wait();
}
