//! `eks control-plane-logs`, end to end: the real binary against a stand-in
//! AWS CLI.
//!
//! The unit tests prove each decision on its own — which streams a type owns,
//! what an audit line says, what a refusal means. What they cannot prove is
//! the wiring: that the binary reads the cluster, region, and profile out of
//! a kubeconfig, asks `describe-cluster` before reading anything, follows the
//! CLI's paging tokens, prints in time order, and turns each failure into the
//! sentence it deserves. A reviewer has no cluster with logging switched on,
//! so this file plays the AWS CLI: a shell script named `aws`, first on
//! `PATH`, that records every call and answers from fixture files.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The stand-in. Each call is appended to `calls`, one line of arguments.
/// It answers `<name>.json` from its directory, or fails with `<name>.err`
/// on stderr, where `<name>` is `describe`, `streams`, `page-<token>` for a
/// later page, or `poll-<n>` for the n-th first page — the window, then each
/// `--follow` poll. `<name>.sleep` delays the answer. `expired` makes every
/// call fail as an expired Identity Center session until `aws sso login`
/// has run.
const FAKE_AWS: &str = r#"#!/bin/sh
dir="$FAKE_AWS_DIR"
printf '%s\n' "$*" >> "$dir/calls"
case "$1 $2" in
  "--version "*) cat "$dir/version" 2>/dev/null; exit 0 ;;
  "sso login") touch "$dir/logged-in"; echo "Successfully logged into Start URL"; exit 0 ;;
  "eks describe-cluster") name=describe ;;
  "logs describe-log-streams") name=streams ;;
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
if [ -f "$dir/expired" ] && [ ! -f "$dir/logged-in" ]; then
  echo "Error when retrieving token from sso: Token has expired and refresh failed" >&2
  exit 255
fi
[ -f "$dir/$name.sleep" ] && sleep "$(cat "$dir/$name.sleep")"
if [ -f "$dir/$name.err" ]; then cat "$dir/$name.err" >&2; exit 254; fi
if [ -f "$dir/$name.json" ]; then cat "$dir/$name.json"; else echo '{"events": []}'; fi
"#;

const ARN: &str = "arn:aws:eks:us-east-1:111122223333:cluster/prod";

const AUDIT_ON: &str = r#"{"cluster": {"name": "prod", "logging": {"clusterLogging": [
  {"types": ["api", "audit"], "enabled": true},
  {"types": ["authenticator", "controllerManager", "scheduler"], "enabled": false}]}}}"#;

const ALL_OFF: &str = r#"{"cluster": {"name": "prod", "logging": {"clusterLogging": [
  {"types": ["api", "audit", "authenticator", "controllerManager", "scheduler"], "enabled": false}]}}}"#;

/// An audit event, as CloudWatch hands it back inside `filter-log-events`.
fn audit(timestamp: i64, id: &str, verb: &str, name: &str, code: u16) -> String {
    let message = format!(
        r#"{{"kind":"Event","apiVersion":"audit.k8s.io/v1","stage":"ResponseComplete","verb":"{verb}","user":{{"username":"arn:aws:sts::111122223333:assumed-role/Admin/alice"}},"objectRef":{{"resource":"pods","namespace":"shop","name":"{name}"}},"responseStatus":{{"code":{code}}}}}"#
    );
    format!(
        r#"{{"logStreamName": "kube-apiserver-audit-0a", "timestamp": {timestamp}, "message": {}, "ingestionTime": {timestamp}, "eventId": "{id}"}}"#,
        serde_json::to_string(&message).unwrap()
    )
}

fn page(events: &[String], next: Option<&str>) -> String {
    let next = next.map_or_else(String::new, |token| format!(r#", "NextToken": "{token}""#));
    format!(
        r#"{{"events": [{}], "searchedLogStreams": []{next}}}"#,
        events.join(",")
    )
}

/// 2026-10-07T06:21:02Z, and the seconds after it.
const T0: i64 = 1_791_354_062_000;

struct World {
    home: tempfile::TempDir,
    bin: PathBuf,
    dir: PathBuf,
}

impl World {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join("bin");
        let dir = home.path().join("aws-state");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        install(&bin.join("aws"), FAKE_AWS);
        let world = Self { home, bin, dir };
        world.kubeconfig(ARN, "prod-admin");
        world
    }

    /// A context as `aws eks update-kubeconfig --profile prod-admin` writes it.
    fn kubeconfig(&self, context: &str, profile: &str) {
        let config = format!(
            r"apiVersion: v1
kind: Config
current-context: {context}
clusters:
- name: {context}
  cluster:
    server: https://ABCDEF.gr7.us-east-1.eks.amazonaws.com
contexts:
- name: {context}
  context:
    cluster: {context}
    user: {context}
users:
- name: {context}
  user:
    exec:
      apiVersion: client.authentication.k8s.io/v1beta1
      command: aws
      args: [--region, us-east-1, eks, get-token, --cluster-name, prod, --output, json, --profile, {profile}]
"
        );
        std::fs::write(self.home.path().join("kubeconfig"), config).unwrap();
    }

    fn answer(&self, name: &str, body: &str) {
        std::fs::write(self.dir.join(name), body).unwrap();
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(ToOwned::to_owned)
            .collect()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_eks"));
        if !args.contains(&"--timeout") {
            command.args(["--timeout", "10s"]);
        }
        command
            .args(args)
            .env("KUBECONFIG", self.home.path().join("kubeconfig"))
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path())
            .env("FAKE_AWS_DIR", &self.dir)
            .env("PATH", path_with(&self.bin))
            .env("NO_COLOR", "1")
            .env_remove("AWS_PROFILE")
            .env_remove("AWS_CONFIG_FILE")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn eks(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
}

/// Write an executable script.
///
/// Through a child `sh` rather than `std::fs::write`: these tests run on
/// parallel threads, and a file this process holds open for writing is
/// inherited by whatever another thread forks at that moment, so executing
/// it would fail with `ETXTBSY`.
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

/// `bin` first, then just enough of the system for `sh`, `cat`, and `sleep`.
fn path_with(bin: &Path) -> String {
    format!("{}:/usr/bin:/bin", bin.display())
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// The value after `flag` in a recorded call.
fn arg<'a>(call: &'a str, flag: &str) -> Option<&'a str> {
    let mut words = call.split(' ');
    words.find(|word| *word == flag)?;
    words.next()
}

#[test]
fn audit_events_print_one_line_each_in_time_order_across_pages() {
    let world = World::new();
    world.answer("describe.json", AUDIT_ON);
    world.answer(
        "poll-1.json",
        &page(
            &[
                audit(T0 + 2_000, "b", "delete", "api-7f9c", 200),
                audit(T0, "a", "get", "api-7f9c", 200),
            ],
            Some("t2"),
        ),
    );
    world.answer(
        "page-t2.json",
        &page(&[audit(T0 + 1_000, "c", "patch", "api-7f9c", 403)], None),
    );

    let output = world.eks(&["control-plane-logs", "--type", "audit"]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "2026-10-07T06:21:02Z  Admin/alice  get  pods shop/api-7f9c  200\n\
         2026-10-07T06:21:03Z  Admin/alice  patch  pods shop/api-7f9c  403\n\
         2026-10-07T06:21:04Z  Admin/alice  delete  pods shop/api-7f9c  200\n"
    );

    let calls = world.calls();
    assert_eq!(calls.len(), 3, "{calls:#?}");
    assert!(
        calls[0].starts_with(
            "eks describe-cluster --name prod --region us-east-1 --profile prod-admin"
        ),
        "{}",
        calls[0]
    );
    assert!(
        calls[1].starts_with("logs filter-log-events --log-group-name /aws/eks/prod/cluster"),
        "{}",
        calls[1]
    );
    assert_eq!(
        arg(&calls[1], "--log-stream-name-prefix"),
        Some("kube-apiserver-audit-")
    );
    assert_eq!(arg(&calls[1], "--profile"), Some("prod-admin"));
    assert_eq!(arg(&calls[1], "--starting-token"), None);
    assert_eq!(arg(&calls[2], "--starting-token"), Some("t2"));

    // `--since` defaults to an hour.
    let start: i64 = arg(&calls[1], "--start-time").unwrap().parse().unwrap();
    let expected = now_ms() - 3_600_000;
    assert!((expected - start).abs() < 60_000, "{start} vs {expected}");
}

#[test]
fn a_type_that_is_off_is_reported_with_the_command_that_would_switch_it_on() {
    let world = World::new();
    world.answer("describe.json", ALL_OFF);

    let output = world.eks(&["control-plane-logs", "--type", "audit"]);

    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(
        message.contains("prod (us-east-1) does not send its audit log to CloudWatch"),
        "{message}"
    );
    assert!(
        message.contains("No control-plane log type is switched on."),
        "{message}"
    );
    assert!(message.contains("CloudWatch charges"), "{message}");
    assert!(
        message.contains(
            r#"aws eks update-cluster-config --region us-east-1 --name prod --profile prod-admin --logging '{"clusterLogging":[{"types":["audit"],"enabled":true}]}'"#
        ),
        "{message}"
    );
    // Nothing was read, and nothing was switched on.
    assert_eq!(world.calls().len(), 1, "{:#?}", world.calls());
    assert_eq!(stdout(&output), "");
}

#[test]
fn the_api_server_log_is_read_from_its_own_streams_and_never_the_audit_log() {
    let world = World::new();
    world.answer("describe.json", AUDIT_ON);
    world.answer(
        "streams.json",
        r#"{"logStreams": [
            {"logStreamName": "kube-apiserver-audit-a1b2", "creationTime": 1},
            {"logStreamName": "kube-apiserver-a1b2", "creationTime": 1},
            {"logStreamName": "authenticator-a1b2", "creationTime": 1}]}"#,
    );
    world.answer(
        "poll-1.json",
        &page(
            &[format!(
                r#"{{"logStreamName": "kube-apiserver-a1b2", "timestamp": {T0}, "message": "I1007 06:21:02.000000 10 controller.go:615] quota admission added evaluator\n", "eventId": "x"}}"#
            )],
            None,
        ),
    );

    let output = world.eks(&["control-plane-logs", "-t", "api"]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "2026-10-07T06:21:02Z  I1007 06:21:02.000000 10 controller.go:615] quota admission added evaluator\n"
    );
    let calls = world.calls();
    let filter = calls
        .iter()
        .find(|call| call.starts_with("logs filter-log-events"))
        .unwrap();
    assert!(
        filter.contains("--log-stream-names kube-apiserver-a1b2 "),
        "{filter}"
    );
    assert!(!filter.contains("audit"), "{filter}");
    assert!(
        calls
            .iter()
            .any(|call| call.starts_with("logs describe-log-streams"))
    );
}

#[test]
fn json_prints_each_event_whole_on_its_own_line() {
    let world = World::new();
    world.answer("describe.json", AUDIT_ON);
    world.answer(
        "poll-1.json",
        &page(&[audit(T0, "a", "delete", "api", 200)], None),
    );

    let output = world.eks(&["control-plane-logs", "--json"]);

    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert_eq!(text.lines().count(), 1, "{text}");
    let value: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(value["type"], "audit");
    assert_eq!(value["time"], "2026-10-07T06:21:02.000Z");
    assert_eq!(value["message"]["verb"], "delete");
    assert_eq!(value["message"]["objectRef"]["name"], "api");
}

#[test]
fn grep_is_sent_as_a_quoted_pattern_and_checked_again_here() {
    let world = World::new();
    world.answer("describe.json", AUDIT_ON);
    // The server's pattern language is not ours: anything it lets through
    // that does not contain the text as written is dropped.
    world.answer(
        "poll-1.json",
        &page(
            &[
                audit(T0, "a", "delete", "api-7f9c", 200),
                audit(T0 + 1, "b", "delete", "worker-1", 200),
            ],
            None,
        ),
    );

    let output = world.eks(&["control-plane-logs", "--grep", "api-7f9c", "--since", "2d"]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).lines().count(), 1, "{}", stdout(&output));
    let calls = world.calls();
    assert_eq!(arg(&calls[1], "--filter-pattern"), Some("\"api-7f9c\""));
    let start: i64 = arg(&calls[1], "--start-time").unwrap().parse().unwrap();
    assert!((now_ms() - 2 * 86_400_000 - start).abs() < 60_000);
}

#[test]
fn an_empty_window_says_so_on_stderr_and_leaves_stdout_empty() {
    let world = World::new();
    world.answer("describe.json", AUDIT_ON);

    let output = world.eks(&["control-plane-logs", "--since", "15m", "--grep", "nope"]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    let note = stderr(&output);
    assert!(
        note.contains("No audit events from prod (us-east-1) in the last 15m containing \"nope\"."),
        "{note}"
    );
}

#[test]
fn a_missing_permission_names_the_action() {
    let world = World::new();
    world.answer("describe.json", AUDIT_ON);
    world.answer(
        "poll-1.err",
        "\nAn error occurred (AccessDeniedException) when calling the FilterLogEvents operation: \
         User: arn:aws:sts::111122223333:assumed-role/ReadOnly/alice is not authorized to perform: \
         logs:FilterLogEvents on resource: arn:aws:logs:us-east-1:111122223333:log-group:/aws/eks/prod/cluster:log-stream:\n",
    );

    let output = world.eks(&["control-plane-logs"]);

    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(
        message.contains("ReadOnly/alice is not allowed `logs:FilterLogEvents`"),
        "{message}"
    );
    assert!(message.contains("profile \"prod-admin\""), "{message}");
}

#[test]
fn a_missing_describe_permission_names_that_action_instead() {
    let world = World::new();
    world.answer(
        "describe.err",
        "An error occurred (AccessDeniedException) when calling the DescribeCluster operation: \
         User: arn:aws:iam::111122223333:user/bob is not authorized to perform: eks:DescribeCluster \
         on resource: arn:aws:eks:us-east-1:111122223333:cluster/prod",
    );

    let output = world.eks(&["control-plane-logs"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("bob is not allowed `eks:DescribeCluster`"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn no_aws_cli_on_the_path_says_what_to_install() {
    let world = World::new();
    std::fs::remove_file(world.bin.join("aws")).unwrap();

    let output = world.eks(&["control-plane-logs"]);

    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(
        message.contains("`aws` is not in any of the 3 directories on the PATH"),
        "{message}"
    );
    assert!(message.contains("`type aws`"), "{message}");
    assert!(message.contains("version 2"), "{message}");
}

#[test]
fn an_aws_cli_behind_a_tilde_in_path_is_found_and_the_spelling_blamed() {
    // `export PATH="~/bin:$PATH"`: bash runs `aws` from there at its prompt,
    // and no program bash starts can.
    let world = World::new();

    let output = world
        .command(&["control-plane-logs"])
        .env("PATH", "~/bin:/usr/bin:/bin")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(message.contains("`aws` is in `~/bin`"), "{message}");
    assert!(
        message.contains("write `$HOME/bin` instead of `~/bin`"),
        "{message}"
    );
    assert!(world.calls().is_empty(), "{:?}", world.calls());
}

#[test]
fn an_aws_cli_too_old_for_its_arguments_says_which_version_is_needed() {
    let world = World::new();
    world.answer("describe.err", "usage: aws [options] <command> <subcommand>\naws: error: argument operation: Invalid choice");
    world.answer(
        "version",
        "aws-cli/1.18.69 Python/2.7.18 Linux/5.4 botocore/1.17.0",
    );

    let output = world.eks(&["control-plane-logs"]);

    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(
        message.contains("your AWS CLI is aws-cli/1.18.69"),
        "{message}"
    );
    assert!(message.contains("version 2 or later"), "{message}");
    // The version was asked for only because the call failed.
    assert!(world.calls().iter().any(|call| call == "--version"));
}

#[test]
fn a_call_that_outlives_the_timeout_is_stopped_and_named() {
    let world = World::new();
    world.answer("describe.json", AUDIT_ON);
    world.answer("poll-1.sleep", "30");
    let started = Instant::now();

    let output = world.eks(&["--timeout", "1s", "control-plane-logs"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(started.elapsed() < Duration::from_secs(20));
    let message = stderr(&output);
    assert!(
        message.contains("`aws logs filter-log-events` did not finish within 1s"),
        "{message}"
    );
    assert!(message.contains("--timeout 4s"), "{message}");
}

#[test]
fn an_expired_session_is_offered_the_same_login_as_every_command() {
    let world = World::new();
    // A session the token cache still calls live, so the pre-flight has
    // nothing to say; the AWS CLI is what finds out it is not.
    std::fs::create_dir_all(world.home.path().join(".aws/sso/cache")).unwrap();
    std::fs::write(
        world.home.path().join(".aws/config"),
        "[profile prod-admin]\nsso_session = corp\nsso_account_id = 111122223333\nsso_role_name = Admin\n\n\
         [sso-session corp]\nsso_start_url = https://acme.awsapps.com/start\nsso_region = us-east-1\n",
    )
    .unwrap();
    std::fs::write(
        world.home.path().join(".aws/sso/cache/corp.json"),
        r#"{"startUrl": "https://acme.awsapps.com/start", "expiresAt": "2099-01-01T00:00:00Z"}"#,
    )
    .unwrap();
    world.answer("expired", "");
    world.answer("describe.json", AUDIT_ON);
    world.answer(
        "poll-1.json",
        &page(&[audit(T0, "a", "delete", "api", 200)], None),
    );

    let output = world.eks(&["--login", "always", "control-plane-logs"]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("AWS refused the credentials from profile \"prod-admin\""),
        "{}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("Logging in: `aws sso login --profile prod-admin`"),
        "{}",
        stderr(&output)
    );
    assert_eq!(stdout(&output).lines().count(), 1);
    let calls = world.calls();
    assert_eq!(calls[0].split(' ').next(), Some("eks"), "{calls:#?}");
    assert_eq!(calls[1], "sso login --profile prod-admin");
    assert!(calls[2].starts_with("eks describe-cluster"));
}

#[test]
fn an_expired_session_with_login_never_says_what_to_run() {
    let world = World::new();
    world.answer("expired", "");

    let output = world.eks(&["--login", "never", "control-plane-logs"]);

    assert_eq!(output.status.code(), Some(1));
    let message = stderr(&output);
    assert!(message.contains("have expired"), "{message}");
    assert!(
        message.contains("aws sso login --profile prod-admin"),
        "{message}"
    );
    assert!(
        !world
            .calls()
            .iter()
            .any(|call| call.starts_with("sso login"))
    );
}

#[test]
fn an_eksctl_context_needs_no_flags_either() {
    let world = World::new();
    let config = r"apiVersion: v1
kind: Config
current-context: alice@staging.eu-west-1.eksctl.io
clusters:
- name: staging.eu-west-1.eksctl.io
  cluster:
    server: https://0123ABCD.yl4.eu-west-1.eks.amazonaws.com
contexts:
- name: alice@staging.eu-west-1.eksctl.io
  context:
    cluster: staging.eu-west-1.eksctl.io
    user: alice@staging.eu-west-1.eksctl.io
users:
- name: alice@staging.eu-west-1.eksctl.io
  user:
    exec:
      apiVersion: client.authentication.k8s.io/v1beta1
      command: aws
      args: [eks, get-token, --output, json, --cluster-name, staging, --region, eu-west-1]
";
    std::fs::write(world.home.path().join("kubeconfig"), config).unwrap();
    world.answer("describe.json", AUDIT_ON);

    let output = world.eks(&["control-plane-logs"]);

    assert!(output.status.success(), "{}", stderr(&output));
    let calls = world.calls();
    assert!(
        calls[0]
            .starts_with("eks describe-cluster --name staging --region eu-west-1 --output json"),
        "{}",
        calls[0]
    );
    assert!(
        calls[1].contains("--log-group-name /aws/eks/staging/cluster"),
        "{}",
        calls[1]
    );
}

#[test]
fn follow_prints_new_events_and_drops_the_ones_it_has_already_printed() {
    let world = World::new();
    world.answer("describe.json", AUDIT_ON);
    world.answer(
        "poll-1.json",
        &page(&[audit(T0, "a", "get", "api", 200)], None),
    );
    // The poll overlaps the window: `a` comes back, and `b` is new.
    world.answer(
        "poll-2.json",
        &page(
            &[
                audit(T0, "a", "get", "api", 200),
                audit(T0 + 1_000, "b", "delete", "api", 200),
            ],
            None,
        ),
    );

    // `--since` is an instant, not the default hour: a window counted back
    // from the clock would pass `T0` an hour after it, and from then on the
    // repeat would be dropped by the window rather than by the tail.
    let mut child = world
        .command(&[
            "control-plane-logs",
            "--follow",
            "--since",
            "2026-10-07T06:00:00Z",
        ])
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let first = lines.next().unwrap().unwrap();
    let second = lines.next().unwrap().unwrap();
    child.kill().unwrap();
    child.wait().unwrap();

    assert_eq!(
        first,
        "2026-10-07T06:21:02Z  Admin/alice  get  pods shop/api  200"
    );
    assert_eq!(
        second,
        "2026-10-07T06:21:03Z  Admin/alice  delete  pods shop/api  200"
    );
    let calls = world.calls();
    let polls: Vec<i64> = calls
        .iter()
        .filter(|call| call.starts_with("logs filter-log-events"))
        .map(|call| arg(call, "--start-time").unwrap().parse().unwrap())
        .collect();
    // The first read starts at `--since`; the poll after it, thirty seconds
    // before the newest event printed.
    assert!(polls.len() >= 2, "{calls:#?}");
    assert_eq!(polls[0], 1_791_352_800_000, "2026-10-07T06:00:00Z");
    assert_eq!(polls[1], T0 - 30_000);
}

#[test]
fn a_reader_that_goes_away_ends_a_follow_quietly() {
    // `eks control-plane-logs -f | head -1`: once `head` has its line, the
    // next write meets a closed pipe, and that is the end, not an error.
    let world = World::new();
    world.answer("describe.json", AUDIT_ON);
    world.answer(
        "poll-1.json",
        &page(&[audit(T0, "a", "get", "api", 200)], None),
    );
    world.answer(
        "poll-2.json",
        &page(&[audit(T0 + 1_000, "b", "get", "api", 200)], None),
    );

    let mut child = world
        .command(&["control-plane-logs", "--follow"])
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    lines.next().unwrap().unwrap();
    drop(lines);

    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        !stderr(&output).contains("could not write"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn follow_rides_out_a_throttled_poll_and_says_so_once() {
    let world = World::new();
    world.answer("describe.json", AUDIT_ON);
    world.answer(
        "poll-1.json",
        &page(&[audit(T0, "a", "get", "api", 200)], None),
    );
    world.answer(
        "poll-2.err",
        "An error occurred (ThrottlingException) when calling the FilterLogEvents operation \
         (reached max retries: 2): Rate exceeded",
    );
    world.answer(
        "poll-3.json",
        &page(&[audit(T0 + 1_000, "b", "delete", "api", 200)], None),
    );

    let mut child = world
        .command(&["control-plane-logs", "--follow"])
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    lines.next().unwrap().unwrap();
    let after = lines.next().unwrap().unwrap();
    child.kill().unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(
        after,
        "2026-10-07T06:21:03Z  Admin/alice  delete  pods shop/api  200"
    );
    let notes = stderr(&output);
    assert_eq!(notes.matches("AWS is throttling").count(), 1, "{notes}");
    assert!(notes.contains("Trying again every 5s."), "{notes}");
    assert!(notes.contains("Reading again."), "{notes}");
}

// --- the dashboard's pane ---------------------------------------------------
//
// `C` in the dashboard reads through `spawn_dashboard`, in this process. The
// stand-in is found through the kubeconfig's own `exec` environment, which
// every `aws` call is given, so no test here touches this process's `PATH`.

mod dashboard {
    use std::sync::mpsc::{Receiver, RecvTimeoutError};

    use eks::aws::logs::LogType;
    use eks::commands::StreamHandle;
    use eks::commands::control_plane_logs::{Update, spawn_dashboard};
    use eks::k8s::page::Budget;
    use eks::kubeconfig::KubeConfig;

    use super::*;

    impl World {
        /// [`World::kubeconfig`], with the stand-in's `PATH` and state
        /// directory in the helper's environment.
        fn dashboard_kubeconfig(&self) -> PathBuf {
            let path = self.home.path().join("kubeconfig");
            let config = format!(
                r"apiVersion: v1
kind: Config
current-context: {ARN}
clusters:
- name: {ARN}
  cluster:
    server: https://ABCDEF.gr7.us-east-1.eks.amazonaws.com
contexts:
- name: {ARN}
  context:
    cluster: {ARN}
    user: {ARN}
users:
- name: {ARN}
  user:
    exec:
      apiVersion: client.authentication.k8s.io/v1beta1
      command: aws
      args: [--region, us-east-1, eks, get-token, --cluster-name, prod, --output, json, --profile, prod-admin]
      env:
      - name: PATH
        value: {path}
      - name: FAKE_AWS_DIR
        value: {dir}
",
                path = path_with(&self.bin),
                dir = self.dir.display(),
            );
            std::fs::write(&path, config).unwrap();
            path
        }

        /// What `C` starts, for `kind`.
        fn pane(&self, kind: LogType) -> (Receiver<Update>, StreamHandle) {
            let path = self.dashboard_kubeconfig();
            let config = KubeConfig::load_from(std::slice::from_ref(&path)).unwrap();
            spawn_dashboard(
                config,
                vec![path],
                ARN.to_owned(),
                kind,
                Budget::of(Duration::from_secs(10)),
            )
        }
    }

    /// The next update, waiting longer than one poll for it.
    fn next(rx: &Receiver<Update>) -> Update {
        rx.recv_timeout(Duration::from_secs(15))
            .expect("the pane heard nothing")
    }

    fn read(update: Update) -> (Vec<String>, Option<String>) {
        match update {
            Update::Read { lines, note } => (lines, note),
            other => panic!("expected a read, got {other:?}"),
        }
    }

    #[test]
    fn the_pane_reads_the_window_in_time_order_then_only_what_is_new() {
        // Inside the pane's last hour, as CloudWatch would only return: the
        // repeat in the second read is dropped because it was shown, not
        // because it fell out of the window.
        let t = now_ms() - 120_000;
        let world = World::new();
        world.answer("describe.json", AUDIT_ON);
        world.answer(
            "poll-1.json",
            &page(
                &[
                    audit(t + 1_000, "b", "delete", "api", 200),
                    audit(t, "a", "get", "api", 200),
                ],
                None,
            ),
        );
        world.answer(
            "poll-2.json",
            &page(
                &[
                    audit(t + 1_000, "b", "delete", "api", 200),
                    audit(t + 2_000, "c", "patch", "api", 409),
                ],
                None,
            ),
        );

        let (rx, _handle) = world.pane(LogType::Audit);
        let (first, note) = read(next(&rx));
        let (second, _) = read(next(&rx));

        assert_eq!(first.len(), 2, "{first:#?}");
        assert!(
            first[0].ends_with("  Admin/alice  get  pods shop/api  200"),
            "{first:#?}"
        );
        assert!(
            first[1].ends_with("  Admin/alice  delete  pods shop/api  200"),
            "{first:#?}"
        );
        assert_eq!(note, None);
        assert_eq!(second.len(), 1, "{second:#?}");
        assert!(
            second[0].ends_with("  Admin/alice  patch  pods shop/api  409"),
            "{second:#?}"
        );
    }

    #[test]
    fn an_empty_window_is_still_a_read_so_the_pane_stops_loading() {
        let world = World::new();
        world.answer("describe.json", AUDIT_ON);

        let (rx, _handle) = world.pane(LogType::Audit);

        assert_eq!(read(next(&rx)), (Vec::new(), None));
    }

    #[test]
    fn a_type_that_is_off_is_explained_with_the_pane_s_own_keys_and_nothing_is_read() {
        let world = World::new();
        world.answer("describe.json", AUDIT_ON);

        let (rx, _handle) = world.pane(LogType::Scheduler);

        let Update::Off(advice) = next(&rx) else {
            panic!("expected the type to be off");
        };
        assert!(
            advice.starts_with("prod (us-east-1) does not send its scheduler log to CloudWatch."),
            "{advice}"
        );
        assert!(advice.contains("press t to change type"), "{advice}");
        assert!(advice.contains("aws eks update-cluster-config"), "{advice}");
        assert!(advice.contains("press r to look again"), "{advice}");
        assert!(!advice.contains("--type"), "{advice}");
        assert!(
            !world
                .calls()
                .iter()
                .any(|call| call.starts_with("logs filter-log-events")),
            "{:#?}",
            world.calls()
        );
        // Nothing more is coming: the stream has ended.
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap_err(),
            RecvTimeoutError::Disconnected
        );
    }

    #[test]
    fn an_expired_session_stops_the_pane_offers_l_and_never_logs_in_by_itself() {
        let world = World::new();
        world.answer("describe.json", AUDIT_ON);
        world.answer("expired", "");

        let (rx, _handle) = world.pane(LogType::Audit);

        let Update::Failed(error) = next(&rx) else {
            panic!("expected the read to fail");
        };
        assert!(error.credentials, "{error:?}");
        assert!(
            error.message.contains("Press L to sign in again"),
            "{error:?}"
        );
        assert!(
            !world
                .calls()
                .iter()
                .any(|call| call.starts_with("sso login")),
            "{:#?}",
            world.calls()
        );
    }

    #[test]
    fn a_missing_permission_stops_the_pane_without_offering_l() {
        let world = World::new();
        world.answer(
            "describe.err",
            "An error occurred (AccessDeniedException) when calling the DescribeCluster \
             operation: User: arn:aws:sts::111122223333:assumed-role/ReadOnly/alice is not \
             authorized to perform: eks:DescribeCluster on resource: x",
        );

        let (rx, _handle) = world.pane(LogType::Audit);

        let Update::Failed(error) = next(&rx) else {
            panic!("expected the read to fail");
        };
        assert!(!error.credentials, "{error:?}");
        assert!(error.message.contains("`eks:DescribeCluster`"), "{error:?}");
    }

    #[test]
    fn a_throttled_first_call_is_waited_out_rather_than_ending_the_pane() {
        let world = World::new();
        world.answer("describe.json", AUDIT_ON);
        world.answer(
            "poll-1.err",
            "An error occurred (ThrottlingException) when calling the FilterLogEvents \
             operation (reached max retries: 2): Rate exceeded",
        );
        world.answer(
            "poll-2.json",
            &page(&[audit(T0, "a", "get", "api", 200)], None),
        );

        let (rx, _handle) = world.pane(LogType::Audit);

        let Update::Retrying(message) = next(&rx) else {
            panic!("expected the pane to wait the throttling out");
        };
        assert!(message.contains("AWS is throttling"), "{message}");
        assert!(message.contains("Trying again every 5s."), "{message}");
        assert!(!message.contains("--since"), "{message}");
        let (lines, _) = read(next(&rx));
        assert_eq!(
            lines,
            ["2026-10-07T06:21:02Z  Admin/alice  get  pods shop/api  200"]
        );
    }

    #[test]
    fn dropping_the_handle_ends_a_read_in_progress_at_once() {
        let world = World::new();
        world.answer("describe.json", AUDIT_ON);
        world.answer("poll-1.sleep", "30");

        let (rx, handle) = world.pane(LogType::Audit);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !world
            .calls()
            .iter()
            .any(|call| call.starts_with("logs filter-log-events"))
        {
            assert!(Instant::now() < deadline, "the read never started");
            std::thread::sleep(Duration::from_millis(20));
        }

        let dropped = Instant::now();
        drop(handle);

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(10)).unwrap_err(),
            RecvTimeoutError::Disconnected
        );
        assert!(dropped.elapsed() < Duration::from_secs(10));
        // And nothing polls after it.
        let calls = world.calls().len();
        std::thread::sleep(Duration::from_secs(6));
        assert_eq!(world.calls().len(), calls, "{:#?}", world.calls());
    }
}
