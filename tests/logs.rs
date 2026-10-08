//! `eks logs`, end to end: the real binary against a stand-in API server and
//! a stand-in AWS CLI.
//!
//! The unit tests prove each decision on its own: which pod and container a
//! prefix means, how a Container Insights record reads, what each failure
//! says. What they cannot prove is the wiring that decides between the two
//! sources: that a running pod is read from the cluster and CloudWatch is
//! never asked, that a pod the cluster does not have is looked for in the
//! right group with the right pattern, and that every line from there is
//! labelled. A reviewer has neither a cluster nor Container Insights, so this
//! file plays both: a socket on localhost for the API server, and a shell
//! script named `aws`, first on `PATH`, that records each call and answers
//! from fixture files.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// --- The API server ----------------------------------------------------------

/// Every request target the stand-in API server was sent.
type Requests = Arc<Mutex<Vec<String>>>;

/// Start the stand-in API server on a thread of its own.
fn serve() -> (String, Requests) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Requests::default();
    let seen = Arc::clone(&requests);

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
                tokio::spawn(connection(socket, Arc::clone(&seen)));
            }
        });
    });

    (url, requests)
}

async fn connection(mut socket: TcpStream, requests: Requests) {
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
        requests.lock().unwrap().push(target.clone());

        let (status, kind, body) = route(&target);
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: {kind}\r\ncontent-length: {}\r\n\r\n{body}",
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

/// The running pods: `api-live-1` has restarted twice, `fresh-1` never has.
fn shop_pods() -> Vec<String> {
    vec![pod("api-live-1", "shop", 2), pod("fresh-1", "shop", 0)]
}

const JSON: &str = "application/json";

fn route(target: &str) -> (&'static str, &'static str, String) {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    match path {
        "/api/v1/namespaces/shop/pods" => ("200 OK", JSON, list(&shop_pods())),
        "/api/v1/pods" => {
            let mut all = shop_pods();
            all.push(pod("billing-1", "payments", 0));
            ("200 OK", JSON, list(&all))
        }
        "/api/v1/namespaces/shop/pods/api-live-1/log" => {
            let body = if query.contains("previous=true") {
                "before the crash\n"
            } else {
                "line one\nline two\n"
            };
            ("200 OK", "text/plain", body.to_owned())
        }
        _ => (
            "404 Not Found",
            JSON,
            r#"{"kind":"Status","apiVersion":"v1","status":"Failure","reason":"NotFound","code":404}"#
                .to_owned(),
        ),
    }
}

fn list(items: &[String]) -> String {
    format!(
        r#"{{"kind":"PodList","apiVersion":"v1","metadata":{{"resourceVersion":"1"}},"items":[{}]}}"#,
        items.join(",")
    )
}

fn pod(name: &str, namespace: &str, restarts: u32) -> String {
    format!(
        r#"{{"metadata":{{"name":"{name}","namespace":"{namespace}","creationTimestamp":"2026-10-03T11:00:00Z"}},
            "spec":{{"nodeName":"node-1","containers":[{{"name":"app","image":"app:1"}}]}},
            "status":{{"phase":"Running","containerStatuses":[{{"name":"app","ready":true,"restartCount":{restarts},
              "image":"app:1","imageID":"","state":{{"running":{{"startedAt":"2026-10-03T11:00:00Z"}}}}}}]}}}}"#
    )
}

// --- The AWS CLI -------------------------------------------------------------

/// The stand-in. Each call is appended to `calls`, one line of arguments.
/// `filter-log-events` answers `poll-<n>.json` for the n-th first page, or
/// `page-<token>.json` for a later one, or fails with the matching `.err` on
/// stderr. Anything missing is an empty page.
const FAKE_AWS: &str = r#"#!/bin/sh
dir="$FAKE_AWS_DIR"
printf '%s\n' "$*" >> "$dir/calls"
case "$1 $2" in
  "logs filter-log-events")
    token=""; previous=""
    for word in "$@"; do
      [ "$previous" = "--starting-token" ] && token="$word"
      previous="$word"
    done
    if [ -n "$token" ]; then
      name="page-$token"
    else
      n=$(cat "$dir/polls" 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" > "$dir/polls"
      name="poll-$n"
    fi ;;
  *) echo "usage: aws [options] <command> <subcommand>" >&2; exit 252 ;;
esac
if [ -f "$dir/$name.err" ]; then cat "$dir/$name.err" >&2; exit 254; fi
if [ -f "$dir/$name.json" ]; then cat "$dir/$name.json"; else echo '{"events": []}'; fi
"#;

const ARN: &str = "arn:aws:eks:us-east-1:111122223333:cluster/prod";
const GROUP: &str = "/aws/containerinsights/prod/application";

/// 2026-10-07T06:21:02Z, and the seconds after it.
const T0: i64 = 1_791_354_062_000;

/// A Container Insights line, as `filter-log-events` hands it back.
fn line(at: i64, pod: &str, container: &str, stream: &str, log: &str) -> String {
    let message = serde_json::json!({
        "time": "2026-10-07T06:21:02.123456789Z",
        "stream": stream,
        "log": format!("{log}\n"),
        "kubernetes": {
            "pod_name": pod,
            "namespace_name": "shop",
            "container_name": container,
            "docker_id": "3f2a",
            "host": "ip-10-0-1-2.ec2.internal"
        }
    })
    .to_string();
    serde_json::json!({
        "logStreamName": format!("ip-10-0-1-2.ec2.internal-application.var.log.containers.{pod}_shop_{container}-3f2a.log"),
        "timestamp": at,
        "message": message,
        "ingestionTime": at,
        "eventId": format!("{at}-{log}"),
    })
    .to_string()
}

fn page(events: &[String], next: Option<&str>) -> String {
    let next = next.map_or_else(String::new, |token| format!(r#", "NextToken": "{token}""#));
    format!(
        r#"{{"events": [{}], "searchedLogStreams": []{next}}}"#,
        events.join(",")
    )
}

struct World {
    home: tempfile::TempDir,
    bin: PathBuf,
    dir: PathBuf,
    requests: Requests,
}

impl World {
    fn new() -> Self {
        let (url, requests) = serve();
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join("bin");
        let dir = home.path().join("aws-state");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        install(&bin.join("aws"), FAKE_AWS);
        std::fs::write(
            home.path().join("kubeconfig"),
            format!(
                "apiVersion: v1\nkind: Config\ncurrent-context: {ARN}\n\
                 clusters:\n- name: {ARN}\n  cluster:\n    server: {url}\n\
                 users:\n- name: {ARN}\n  user:\n    token: fake\n\
                 contexts:\n- name: {ARN}\n  context:\n    cluster: {ARN}\n    user: {ARN}\n    namespace: shop\n"
            ),
        )
        .unwrap();
        Self {
            home,
            bin,
            dir,
            requests,
        }
    }

    fn answer(&self, name: &str, body: &str) {
        std::fs::write(self.dir.join(name), body).unwrap();
    }

    fn config(&self, toml: &str) {
        let dir = self.home.path().join(".config").join("eks");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), toml).unwrap();
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(ToOwned::to_owned)
            .collect()
    }

    /// The log requests the API server saw.
    fn log_requests(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|target| target.contains("/log"))
            .cloned()
            .collect()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_eks"));
        command
            .args(["--login", "never", "--timeout", "10s"])
            .args(args)
            .env("KUBECONFIG", self.home.path().join("kubeconfig"))
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path())
            .env("FAKE_AWS_DIR", &self.dir)
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()))
            .env("NO_COLOR", "1")
            .env_remove("AWS_PROFILE")
            .env_remove("AWS_CONFIG_FILE")
            // The "cluster" is a socket on this machine; a proxy from the
            // environment would be asked for it instead.
            .env_remove("HTTPS_PROXY")
            .env_remove("https_proxy")
            .env_remove("HTTP_PROXY")
            .env_remove("http_proxy")
            .env_remove("ALL_PROXY")
            .env_remove("all_proxy")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn eks(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
}

/// Write an executable script through a child `sh`, so no file this process
/// holds open for writing is inherited by a fork on another test's thread
/// (which would make executing it fail with `ETXTBSY`).
fn install(path: &Path, script: &str) {
    let mut child = Command::new("sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
}

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

// --- A running pod -----------------------------------------------------------

#[test]
fn a_running_pod_is_read_from_the_cluster_unlabelled_and_cloudwatch_is_never_asked() {
    let world = World::new();

    let output = world.eks(&["logs", "api"]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "line one\nline two\n");
    assert_eq!(world.calls(), Vec::<String>::new());
    let requests = world.log_requests();
    assert_eq!(requests.len(), 1, "{requests:#?}");
    assert!(requests[0].contains("container=app"), "{}", requests[0]);
    assert!(!requests[0].contains("tailLines"), "{}", requests[0]);
}

#[test]
fn previous_and_since_reach_the_api_server() {
    let world = World::new();

    let output = world.eks(&["logs", "api-live", "--previous", "--since", "15m"]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "before the crash\n");
    let requests = world.log_requests();
    assert!(requests[0].contains("previous=true"), "{}", requests[0]);
    assert!(requests[0].contains("sinceSeconds=900"), "{}", requests[0]);
}

#[test]
fn previous_on_a_container_that_never_restarted_says_so_before_asking() {
    let world = World::new();

    let output = world.eks(&["logs", "fresh", "-p", "-f"]);

    assert_eq!(output.status.code(), Some(1));
    let text = stderr(&output);
    assert!(
        text.contains("--follow does nothing with --previous"),
        "{text}"
    );
    assert!(
        text.contains("app in pod fresh-1 has not restarted, so it has no previous log."),
        "{text}"
    );
    assert_eq!(world.log_requests(), Vec::<String>::new());
}

#[test]
fn kubectl_s_dash_c_for_the_container_is_pointed_at_capital_c() {
    let world = World::new();

    let output = world.eks(&["logs", "api", "-c", "app"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("the container is `--container` (`-C`)"),
        "{}",
        stderr(&output)
    );
}

// --- A pod that is gone ------------------------------------------------------

#[test]
fn a_pod_that_is_gone_is_read_from_container_insights_with_every_line_labelled() {
    let world = World::new();
    world.answer(
        "poll-1.json",
        &page(
            &[
                line(
                    T0 + 2_000,
                    "api-7d9f-xk2",
                    "app",
                    "stderr",
                    "panic: out of memory",
                ),
                line(T0, "api-7d9f-xk2", "app", "stdout", "listening on :8080"),
            ],
            Some("t2"),
        ),
    );
    world.answer(
        "page-t2.json",
        &page(
            &[line(
                T0 + 1_000,
                "api-7d9f-xk2",
                "app",
                "stdout",
                "GET /orders 200",
            )],
            None,
        ),
    );

    let output = world.eks(&["logs", "api-7d9f"]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "[cloudwatch 2026-10-07T06:21:02Z] listening on :8080\n\
         [cloudwatch 2026-10-07T06:21:03Z] GET /orders 200\n\
         [cloudwatch 2026-10-07T06:21:04Z stderr] panic: out of memory\n"
    );
    assert!(
        stderr(&output).contains(&format!(
            "No pod starting \"api-7d9f\" is running in namespace shop; api-7d9f-xk2 was. \
             Reading app's lines in the last 1h from CloudWatch, {GROUP}. Older lines need \
             `--since`, e.g. `--since 1d`."
        )),
        "{}",
        stderr(&output)
    );

    let calls = world.calls();
    assert_eq!(calls.len(), 2, "{calls:#?}");
    assert!(
        calls[0].starts_with(&format!(
            "logs filter-log-events --log-group-name {GROUP} --start-time "
        )),
        "{}",
        calls[0]
    );
    assert!(
        calls[0].contains(
            "--filter-pattern { $.kubernetes.namespace_name = \"shop\" && \
             $.kubernetes.pod_name = \"api-7d9f*\" }"
        ),
        "{}",
        calls[0]
    );
    assert!(calls[0].contains("--region us-east-1"), "{}", calls[0]);
    assert!(calls[1].contains("--starting-token t2"), "{}", calls[1]);
}

#[test]
fn a_cluster_without_container_insights_is_told_how_to_set_it_up() {
    let world = World::new();
    world.answer(
        "poll-1.err",
        "\nAn error occurred (ResourceNotFoundException) when calling the FilterLogEvents \
         operation: The specified log group does not exist.\n",
    );

    let output = world.eks(&["logs", "api-7d9f"]);

    assert_eq!(output.status.code(), Some(1));
    let text = stderr(&output);
    assert!(
        text.contains(&format!(
            "prod (us-east-1) does not send container logs to CloudWatch: there is no log group \
             {GROUP}"
        )),
        "{text}"
    );
    assert!(
        text.contains(
            "aws eks create-addon --cluster-name prod --addon-name \
             amazon-cloudwatch-observability --region us-east-1"
        ),
        "{text}"
    );
    assert_eq!(stdout(&output), "");
}

#[test]
fn the_config_file_names_another_group() {
    let world = World::new();
    world.config("log_group = \"/fluent-bit/{cluster}/pods\"\n");

    let output = world.eks(&["logs", "api-7d9f"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        world.calls()[0].contains("--log-group-name /fluent-bit/prod/pods "),
        "{:#?}",
        world.calls()
    );
}

#[test]
fn nothing_in_cloudwatch_but_a_running_pod_elsewhere_names_its_namespace() {
    let world = World::new();

    let output = world.eks(&["logs", "billing"]);

    assert_eq!(output.status.code(), Some(1));
    let text = stderr(&output);
    assert!(
        text.contains(
            "no pod in namespace shop is called \"billing\" or starts with it, and CloudWatch"
        ),
        "{text}"
    );
    assert!(text.contains("pass `-n payments`"), "{text}");
}

#[test]
fn several_gone_pods_are_listed_to_choose_from() {
    let world = World::new();
    world.answer(
        "poll-1.json",
        &page(
            &[
                line(T0, "api-7d9f-aaa", "app", "stdout", "x"),
                line(T0 + 1_000, "api-7d9f-bbb", "app", "stdout", "y"),
            ],
            None,
        ),
    );

    let output = world.eks(&["logs", "api-7d9f"]);

    assert_eq!(output.status.code(), Some(1));
    let text = stderr(&output);
    assert!(
        text.contains("CloudWatch has lines from 2 that did"),
        "{text}"
    );
    assert!(text.contains("api-7d9f-bbb"), "{text}");
    assert!(text.contains("e.g. `eks logs api-7d9f-bbb`"), "{text}");
    assert_eq!(stdout(&output), "");
}

#[test]
fn text_that_cannot_be_a_pod_name_is_never_sent_to_cloudwatch() {
    let world = World::new();

    let output = world.eks(&["logs", "Api\""]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("cannot be the start of a pod's name"),
        "{}",
        stderr(&output)
    );
    assert_eq!(world.calls(), Vec::<String>::new());
}

#[test]
fn follow_on_a_gone_pod_prints_lines_still_on_their_way() {
    let world = World::new();
    // Inside the window CloudWatch is asked for, as every real answer is:
    // the de-duplication keeps only what a later poll can return again.
    let now = now_ms();
    world.answer(
        "poll-1.json",
        &page(
            &[line(now - 2_000, "api-7d9f-xk2", "app", "stdout", "first")],
            None,
        ),
    );
    // The first poll sees the same line again, and one that arrived late.
    world.answer(
        "poll-2.json",
        &page(
            &[
                line(now - 2_000, "api-7d9f-xk2", "app", "stdout", "first"),
                line(now - 1_000, "api-7d9f-xk2", "app", "stdout", "last words"),
            ],
            None,
        ),
    );

    let mut child = world.command(&["logs", "api-7d9f", "-f"]).spawn().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut seen = Vec::new();
    while seen.len() < 2 && Instant::now() < deadline {
        match lines.next() {
            Some(Ok(line)) => seen.push(line),
            _ => break,
        }
    }
    child.kill().unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(seen.len(), 2, "{seen:#?}\n{}", stderr(&output));
    assert!(
        seen[0].starts_with("[cloudwatch ") && seen[0].ends_with("] first"),
        "{seen:#?}"
    );
    assert!(seen[1].ends_with("] last words"), "{seen:#?}");
    assert!(
        stderr(&output).contains("Following api-7d9f-xk2's app lines in CloudWatch"),
        "{}",
        stderr(&output)
    );
}
