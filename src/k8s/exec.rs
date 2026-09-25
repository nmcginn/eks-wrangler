//! Running a kubeconfig's credential helper ourselves.
//!
//! An EKS context does not store a token. It stores an `exec` block — usually
//! `aws eks get-token --cluster-name prod` — and the Kubernetes client
//! authentication protocol says what to do with it: run the command with the
//! environment the block names, tell it about the run in `KUBERNETES_EXEC_INFO`,
//! and read one `ExecCredential` document off its standard output. That
//! document carries either a bearer token or a client certificate, and
//! optionally the moment it stops being good.
//!
//! `kube` can do this for us, and until decision 110 it did. It does it with a
//! blocking `std::process::Command` on a thread nobody can interrupt, so a
//! helper that hangs could only be *abandoned* — left running, holding the
//! terminal's stdin, after the shell had its prompt back. And it does it more
//! than once: building one client ran the helper for the TLS identity, again
//! for the auth layer, and a third time to ask when the identity expires. Each
//! run of `aws eks get-token` is a Python interpreter starting up.
//!
//! Here the helper is a `tokio` child with `kill_on_drop` set. Whatever stops
//! waiting for it — a `--timeout`, a Ctrl-C, a dashboard closing — drops the
//! future that owns it, and dropping it kills it. It runs once per credential,
//! and the credential is held by [`crate::k8s::auth`], which is what runs it
//! again when the token nears its `expirationTimestamp`.
//!
//! What this module does *not* do is log anybody in. `aws sso login` stays a
//! shell-out in [`crate::aws::login`], and nothing here reads or writes the AWS
//! CLI's token cache (decision 74, narrowed by 110).
//!
//! [`parse`], [`due`], [`interactive`] and [`exec_info`] are pure, so every
//! shape of document a helper might print is a fixture. [`run`] is the only
//! function with a process in it.

use std::fmt;
use std::io;
use std::process::{ExitStatus, Stdio};

use k8s_openapi::jiff::{SignedDuration, Timestamp};
use kube::config::{AuthInfo, ExecConfig, ExecInteractiveMode};
use serde::Deserialize;

use crate::k8s::client;

/// How long before its stated expiry a credential is treated as already gone.
///
/// The same minute [`crate::aws::sso`] allows a cached SSO token, and the same
/// minute `kube` allowed the credentials it held, for the same reason: a token
/// with forty seconds left is good for this page and dead for the next, and
/// the request that finds that out has already been sent.
pub const SKEW: SignedDuration = SignedDuration::from_secs(60);

/// What an `ExecCredential` authenticates with.
///
/// `Debug` is written by hand so that a credential in a `tracing` line or a
/// failed assertion prints as `Token(..)` rather than as the token.
#[derive(Clone, PartialEq, Eq)]
pub enum Secret {
    /// A bearer token, sent on every request.
    Token(String),
    /// A client certificate and its key, both PEM, presented during the TLS
    /// handshake.
    Certificate { certificate: String, key: String },
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Token(_) => f.write_str("Token(..)"),
            Self::Certificate { .. } => f.write_str("Certificate(..)"),
        }
    }
}

/// One answer from a credential helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credential {
    pub secret: Secret,
    /// When the helper said this stops working, if it said. `aws eks
    /// get-token` always says: fifteen minutes after it was minted.
    pub expires: Option<Timestamp>,
}

/// Why running a credential helper did not produce a credential.
///
/// Every variant carries the command, spelled by [`client::helper_command`],
/// because each of them ends in the same advice: run it yourself and look.
/// The user-facing wording is [`client::explain`]'s job, not `Display`'s —
/// `Display` is for logs.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The command could not be started at all: not installed, not on `PATH`,
    /// or an `exec` block with no command in it.
    #[error("could not start `{command}`: {source}")]
    Start { command: String, source: io::Error },

    /// It ran and exited with a failure.
    #[error("`{command}` exited with {status}{}", stderr_suffix(.stderr))]
    Failed {
        command: String,
        status: ExitStatus,
        /// What it said on its way out, when it was not talking to the user
        /// directly — see [`interactive`].
        stderr: String,
    },

    /// It succeeded and printed something that is not a credential.
    #[error("`{command}` did not print a credential: {reason}")]
    Unreadable { command: String, reason: String },

    /// It was still running when the budget ran out, and has been killed.
    #[error("`{command}` was still running after {limit:?}")]
    Stalled {
        command: String,
        limit: std::time::Duration,
    },
}

impl Error {
    /// The helper's command line, as [`client::helper_command`] spells it.
    #[must_use]
    pub fn command(&self) -> &str {
        match self {
            Self::Start { command, .. }
            | Self::Failed { command, .. }
            | Self::Unreadable { command, .. }
            | Self::Stalled { command, .. } => command,
        }
    }
}

fn stderr_suffix(stderr: &str) -> String {
    if stderr.is_empty() {
        String::new()
    } else {
        format!(": {stderr}")
    }
}

/// The `exec` block this context authenticates through, or `None` if it
/// authenticates some other way.
///
/// `kube`'s own precedence, kept exactly: an auth provider, then a username and
/// password, then an inline token, then a token file, and only then `exec`. A
/// kubeconfig with both a token and an `exec` block has always sent the token,
/// and running a helper it never used to run would be a change nobody asked
/// for.
#[must_use]
pub fn helper_of(auth: &AuthInfo) -> Option<&ExecConfig> {
    let otherwise = auth.auth_provider.is_some()
        || (auth.username.is_some() && auth.password.is_some())
        || auth.token.is_some()
        || auth.token_file.is_some();
    if otherwise { None } else { auth.exec.as_ref() }
}

/// Whether a credential is close enough to its expiry to fetch another.
///
/// A credential that never said when it expires is never due by the clock;
/// only a `401` retires it (see [`crate::k8s::auth`]).
#[must_use]
pub fn due(expires: Option<Timestamp>, now: Timestamp) -> bool {
    match expires {
        None => false,
        // Subtracting from the expiry rather than adding to `now`, so the one
        // arithmetic that could overflow is on a value the helper chose. An
        // expiry so close to the start of time that a minute before it does
        // not exist is certainly due.
        Some(at) => at
            .checked_sub(SKEW)
            .map_or(true, |refresh_at| now >= refresh_at),
    }
}

/// Whether the helper may talk to the user: inherit stdin to read an answer,
/// and stderr to ask the question.
///
/// client-go's reading of `interactiveMode`, which `kube`'s simplified to
/// "anything but `Never`". `IfAvailable` — and a block that says nothing, which
/// the protocol defines as `IfAvailable` — means *if stdin is a terminal*. A
/// listing piped in from a script has nobody at the other end, and a helper
/// handed that pipe as though it were a person waits on it.
#[must_use]
pub fn interactive(mode: Option<&ExecInteractiveMode>, stdin_is_terminal: bool) -> bool {
    match mode {
        Some(ExecInteractiveMode::Never) => false,
        Some(ExecInteractiveMode::Always) => true,
        Some(ExecInteractiveMode::IfAvailable) | None => stdin_is_terminal,
    }
}

/// The `KUBERNETES_EXEC_INFO` document the protocol hands a helper: which
/// version of the protocol this is, whether it may prompt, and — when the
/// block asks for it with `provideClusterInfo` — the cluster it is getting
/// credentials for.
pub fn exec_info(exec: &ExecConfig, interactive: bool) -> Result<String, serde_json::Error> {
    let mut spec = serde_json::Map::new();
    spec.insert("interactive".to_owned(), interactive.into());
    if exec.provide_cluster_info
        && let Some(cluster) = &exec.cluster
    {
        spec.insert("cluster".to_owned(), serde_json::to_value(cluster)?);
    }

    let mut info = serde_json::Map::new();
    if let Some(version) = &exec.api_version {
        info.insert("apiVersion".to_owned(), version.clone().into());
    }
    info.insert("kind".to_owned(), "ExecCredential".into());
    info.insert("spec".to_owned(), spec.into());
    serde_json::to_string(&info)
}

/// The part of an `ExecCredential` this tool reads.
#[derive(Deserialize)]
struct Document {
    status: Option<Status>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    expiration_timestamp: Option<String>,
    token: Option<String>,
    client_certificate_data: Option<String>,
    client_key_data: Option<String>,
}

/// Read a credential out of what a helper printed.
///
/// JSON is what the protocol specifies. YAML is accepted as well, because
/// client-go decodes with a YAML-tolerant codec, `kube` followed it, and a
/// helper written against either has been working for somebody. The error
/// names what was wrong with it in terms of the document rather than the
/// parser, since the reader is a person looking at a helper's output.
pub fn parse(stdout: &[u8]) -> Result<Credential, String> {
    if stdout.iter().all(u8::is_ascii_whitespace) {
        return Err("it printed nothing".to_owned());
    }

    let status = match serde_json::from_slice::<Document>(stdout) {
        Ok(document) => document
            .status
            .ok_or_else(|| "its ExecCredential has no `status`".to_owned())?,
        // YAML reads nearly anything as *some* document — `Enter MFA code:` is
        // a map with one key — so it only counts when it finds a `status`.
        // Otherwise the JSON error is the truer description of what arrived.
        Err(json) => serde_yaml_ng::from_slice::<Document>(stdout)
            .ok()
            .and_then(|document| document.status)
            .ok_or_else(|| format!("what it printed is not an ExecCredential ({json})"))?,
    };

    let expires =
        match status.expiration_timestamp.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(text) => Some(text.parse::<Timestamp>().map_err(|_| {
                format!("its `expirationTimestamp` {text:?} is not an RFC 3339 time")
            })?),
        };

    let present = |field: Option<String>| field.filter(|value| !value.trim().is_empty());

    // A certificate wins over a token when both are present — `kube`'s order,
    // and client-go's — since a certificate is the one that cannot be sent
    // alongside the other as a header.
    let secret = match (
        present(status.client_certificate_data),
        present(status.client_key_data),
        present(status.token),
    ) {
        (Some(certificate), Some(key), _) => Secret::Certificate { certificate, key },
        (Some(_), None, _) => {
            return Err(
                "it sent a client certificate without the key that goes with it".to_owned(),
            );
        }
        (None, Some(_), _) => {
            return Err(
                "it sent a client key without the certificate that goes with it".to_owned(),
            );
        }
        (None, None, Some(token)) => Secret::Token(token),
        (None, None, None) => {
            return Err("its `status` carries neither a token nor a client certificate".to_owned());
        }
    };

    Ok(Credential { secret, expires })
}

/// Run the context's credential helper once, and read what it prints.
///
/// `auth` is the context's `AuthInfo`; callers check [`helper_of`] first. The
/// child is killed if this future is dropped before it exits, which is the
/// whole reason this exists: every caller races it against a budget, and
/// losing that race has to stop the helper rather than stop watching it.
///
/// No budget is taken here. The two callers spend theirs differently — see
/// [`crate::k8s::client::build`] and [`crate::k8s::auth`] — and a timeout is
/// a thing to put around a future, not to thread through it.
pub async fn run(auth: &AuthInfo) -> Result<Credential, Error> {
    let command = client::helper_command(auth).unwrap_or_default();
    let start = |source: io::Error| Error::Start {
        command: command.clone(),
        source,
    };

    let exec = auth
        .exec
        .as_ref()
        .ok_or_else(|| start(io::Error::other("the context has no `exec` block")))?;
    let program = exec
        .command
        .as_deref()
        .ok_or_else(|| start(io::Error::other("its `exec` block names no command")))?;

    let talks = interactive(
        exec.interactive_mode.as_ref(),
        std::io::IsTerminal::is_terminal(&io::stdin()),
    );
    let info = exec_info(exec, talks).map_err(|error| start(io::Error::other(error)))?;

    let mut child = tokio::process::Command::new(program);
    child
        .args(exec.args.iter().flatten())
        .envs(client::exec_env(auth))
        .env("KUBERNETES_EXEC_INFO", info)
        .stdout(Stdio::piped())
        // Held by the future below, so dropping that future is what kills it.
        .kill_on_drop(true);
    for name in exec.drop_env.iter().flatten() {
        child.env_remove(name);
    }
    if talks {
        child.stdin(Stdio::inherit()).stderr(Stdio::inherit());
    } else {
        // Nobody to answer, so nothing to read: a helper that asks anyway gets
        // an immediate end of file rather than a wait on a pipe.
        child.stdin(Stdio::null()).stderr(Stdio::piped());
    }

    let output = child
        .spawn()
        .map_err(start)?
        .wait_with_output()
        .await
        .map_err(start)?;

    if !output.status.success() {
        return Err(Error::Failed {
            command,
            status: output.status,
            stderr: last_line(&output.stderr),
        });
    }

    parse(&output.stdout).map_err(|reason| Error::Unreadable { command, reason })
}

/// The last non-blank line of what a helper wrote to stderr — where every CLI
/// puts the sentence that says what went wrong.
fn last_line(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn exec_auth(yaml: &str) -> AuthInfo {
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    // --- parse --------------------------------------------------------------

    /// What `aws eks get-token` prints, give or take the token.
    const EKS: &str = r#"{"kind": "ExecCredential", "apiVersion": "client.authentication.k8s.io/v1beta1", "spec": {}, "status": {"expirationTimestamp": "2026-09-25T06:30:00Z", "token": "k8s-aws-v1.aHR0cHM6Ly9zdHM"}}"#;

    #[test]
    fn an_eks_token_is_read_with_its_expiry() {
        let credential = parse(EKS.as_bytes()).unwrap();

        assert_eq!(
            credential.secret,
            Secret::Token("k8s-aws-v1.aHR0cHM6Ly9zdHM".to_owned())
        );
        assert_eq!(credential.expires, Some(at("2026-09-25T06:30:00Z")));
    }

    #[test]
    fn a_token_with_no_expiry_is_read_as_one_that_never_says() {
        let credential = parse(br#"{"status": {"token": "abc"}}"#).unwrap();

        assert_eq!(credential.secret, Secret::Token("abc".to_owned()));
        assert_eq!(credential.expires, None);
    }

    #[test]
    fn a_client_certificate_is_read_as_a_certificate() {
        let credential = parse(
            br#"{"status": {"clientCertificateData": "-----BEGIN CERTIFICATE-----", "clientKeyData": "-----BEGIN PRIVATE KEY-----"}}"#,
        )
        .unwrap();

        assert_eq!(
            credential.secret,
            Secret::Certificate {
                certificate: "-----BEGIN CERTIFICATE-----".to_owned(),
                key: "-----BEGIN PRIVATE KEY-----".to_owned(),
            }
        );
    }

    #[test]
    fn a_certificate_wins_over_a_token_sent_beside_it() {
        let credential = parse(
            br#"{"status": {"token": "abc", "clientCertificateData": "cert", "clientKeyData": "key"}}"#,
        )
        .unwrap();

        assert!(matches!(credential.secret, Secret::Certificate { .. }));
    }

    #[test]
    fn a_helper_that_prints_yaml_is_understood_like_client_go_understands_it() {
        let credential =
            parse(b"status:\n  token: abc\n  expirationTimestamp: 2026-09-25T06:30:00Z\n").unwrap();

        assert_eq!(credential.secret, Secret::Token("abc".to_owned()));
        assert_eq!(credential.expires, Some(at("2026-09-25T06:30:00Z")));
    }

    #[test]
    fn every_unreadable_answer_says_what_was_wrong_with_it() {
        for (printed, expected) in [
            ("", "printed nothing"),
            ("  \n", "printed nothing"),
            ("Enter MFA code:", "not an ExecCredential"),
            (r#"{"kind": "ExecCredential"}"#, "no `status`"),
            (
                r#"{"status": {}}"#,
                "neither a token nor a client certificate",
            ),
            (
                r#"{"status": {"token": ""}}"#,
                "neither a token nor a client certificate",
            ),
            (
                r#"{"status": {"clientCertificateData": "c"}}"#,
                "without the key",
            ),
            (
                r#"{"status": {"clientKeyData": "k"}}"#,
                "without the certificate",
            ),
            (
                r#"{"status": {"token": "abc", "expirationTimestamp": "tomorrow"}}"#,
                "\"tomorrow\" is not an RFC 3339 time",
            ),
        ] {
            let reason = parse(printed.as_bytes()).expect_err(printed);
            assert!(reason.contains(expected), "{printed:?} → {reason}");
        }
    }

    #[test]
    fn a_credential_never_prints_its_secret_when_debugged() {
        let credential = parse(EKS.as_bytes()).unwrap();
        let debugged = format!("{credential:?}");

        assert!(!debugged.contains("aHR0cHM6Ly9zdHM"), "{debugged}");
        assert!(debugged.contains("Token(..)"), "{debugged}");

        let certificate = Secret::Certificate {
            certificate: "cert-body".to_owned(),
            key: "key-body".to_owned(),
        };
        let debugged = format!("{certificate:?}");
        assert!(!debugged.contains("key-body"), "{debugged}");
    }

    // --- due ----------------------------------------------------------------

    #[test]
    fn a_credential_with_minutes_left_is_not_due() {
        let now = at("2026-09-25T06:00:00Z");
        assert!(!due(Some(at("2026-09-25T06:14:00Z")), now));
    }

    #[test]
    fn a_credential_inside_its_last_minute_is_due() {
        let now = at("2026-09-25T06:00:00Z");
        assert!(due(Some(at("2026-09-25T06:00:59Z")), now));
        // Exactly a minute out is the first moment it counts as gone.
        assert!(due(Some(at("2026-09-25T06:01:00Z")), now));
        assert!(!due(Some(at("2026-09-25T06:01:01Z")), now));
    }

    #[test]
    fn a_credential_that_has_already_expired_is_due() {
        let now = at("2026-09-25T06:00:00Z");
        assert!(due(Some(at("2026-09-25T05:00:00Z")), now));
    }

    #[test]
    fn a_credential_that_never_said_when_it_expires_is_never_due_by_the_clock() {
        assert!(!due(None, at("2999-01-01T00:00:00Z")));
    }

    #[test]
    fn an_expiry_at_the_start_of_time_is_due_rather_than_an_overflow() {
        assert!(due(Some(Timestamp::MIN), Timestamp::MIN));
    }

    // --- interactive and exec_info -----------------------------------------

    #[test]
    fn a_helper_may_prompt_only_when_its_mode_and_the_terminal_both_allow() {
        use ExecInteractiveMode::{Always, IfAvailable, Never};

        assert!(!interactive(Some(&Never), true));
        assert!(interactive(Some(&Always), false));
        assert!(interactive(Some(&IfAvailable), true));
        assert!(!interactive(Some(&IfAvailable), false));
        // Unset is `IfAvailable` in the protocol.
        assert!(interactive(None, true));
        assert!(!interactive(None, false));
    }

    #[test]
    fn the_exec_info_tells_the_helper_its_protocol_and_whether_it_may_prompt() {
        let exec = ExecConfig {
            api_version: Some("client.authentication.k8s.io/v1beta1".to_owned()),
            ..Default::default()
        };

        let info: serde_json::Value =
            serde_json::from_str(&exec_info(&exec, false).unwrap()).unwrap();

        assert_eq!(info["apiVersion"], "client.authentication.k8s.io/v1beta1");
        assert_eq!(info["kind"], "ExecCredential");
        assert_eq!(info["spec"]["interactive"], false);
        assert!(info["spec"].get("cluster").is_none());
    }

    #[test]
    fn the_exec_info_carries_the_cluster_only_when_the_block_asks_for_it() {
        let cluster = kube::config::ExecAuthCluster {
            server: Some("https://prod.example".to_owned()),
            ..Default::default()
        };
        let mut exec = ExecConfig {
            cluster: Some(cluster),
            ..Default::default()
        };

        let without: serde_json::Value =
            serde_json::from_str(&exec_info(&exec, true).unwrap()).unwrap();
        assert!(without["spec"].get("cluster").is_none());

        exec.provide_cluster_info = true;
        let with: serde_json::Value =
            serde_json::from_str(&exec_info(&exec, true).unwrap()).unwrap();
        assert_eq!(with["spec"]["cluster"]["server"], "https://prod.example");
    }

    // --- helper_of ------------------------------------------------------------

    #[test]
    fn an_exec_block_is_the_helper_when_nothing_outranks_it() {
        let auth = exec_auth("exec:\n  command: aws\n");
        assert_eq!(
            helper_of(&auth).and_then(|exec| exec.command.as_deref()),
            Some("aws")
        );
    }

    #[test]
    fn an_inline_token_or_a_token_file_outranks_an_exec_block_as_it_does_in_kube() {
        for other in ["token: abc", "tokenFile: /var/run/token"] {
            let auth = exec_auth(&format!("{other}\nexec:\n  command: aws\n"));
            assert!(helper_of(&auth).is_none(), "{other}");
        }
    }

    #[test]
    fn a_context_with_no_exec_block_has_no_helper() {
        assert!(helper_of(&exec_auth("token: abc\n")).is_none());
        assert!(helper_of(&AuthInfo::default()).is_none());
    }

    // --- run, against real processes -----------------------------------------

    fn sh(script: &str) -> AuthInfo {
        exec_auth(&format!(
            "exec:\n  apiVersion: client.authentication.k8s.io/v1beta1\n  command: sh\n  args: ['-c', {script:?}]\n  interactiveMode: Never\n"
        ))
    }

    #[tokio::test]
    async fn a_helper_that_prints_a_credential_is_run_and_read() {
        let auth = sh(r#"echo '{"status": {"token": "from-sh"}}'"#);

        let credential = run(&auth).await.unwrap();

        assert_eq!(credential.secret, Secret::Token("from-sh".to_owned()));
    }

    #[tokio::test]
    async fn the_helper_is_given_its_environment_and_the_exec_info() {
        // Written to a file rather than echoed into the token, because the
        // exec info is JSON and would need escaping to survive inside more.
        let dir = tempfile::tempdir().unwrap();
        let seen = dir.path().join("seen");
        let auth = exec_auth(&format!(
            r#"
exec:
  apiVersion: client.authentication.k8s.io/v1beta1
  command: sh
  args: ['-c', 'printf "%s\n%s" "$AWS_PROFILE" "$KUBERNETES_EXEC_INFO" > "$0"; echo "{{\"status\": {{\"token\": \"t\"}}}}"', {seen:?}]
  env:
    - name: AWS_PROFILE
      value: prod-admin
  interactiveMode: Never
"#
        ));

        run(&auth).await.unwrap();

        let seen = std::fs::read_to_string(seen).unwrap();
        let (profile, info) = seen.split_once('\n').unwrap();
        assert_eq!(profile, "prod-admin");
        let info: serde_json::Value = serde_json::from_str(info).unwrap();
        assert_eq!(info["kind"], "ExecCredential");
        assert_eq!(info["apiVersion"], "client.authentication.k8s.io/v1beta1");
        assert_eq!(info["spec"]["interactive"], false);
    }

    #[tokio::test]
    async fn a_helper_that_fails_reports_the_last_thing_it_said() {
        let auth = sh("echo 'working...' >&2; echo 'Error loading SSO Token' >&2; exit 255");

        let error = run(&auth).await.expect_err("it exited 255");

        let Error::Failed { stderr, .. } = &error else {
            panic!("{error:?}");
        };
        assert_eq!(stderr, "Error loading SSO Token");
        assert!(error.command().starts_with("sh -c"), "{}", error.command());
    }

    #[tokio::test]
    async fn a_helper_that_prints_something_else_is_unreadable_and_named() {
        let auth = sh("echo 'hello'");

        let error = run(&auth).await.expect_err("hello is not a credential");

        assert!(matches!(error, Error::Unreadable { .. }), "{error:?}");
        assert!(error.to_string().contains("sh -c"), "{error}");
    }

    #[tokio::test]
    async fn a_helper_that_is_not_installed_cannot_be_started() {
        let auth = exec_auth(
            "exec:\n  command: eks-test-no-such-credential-helper\n  interactiveMode: Never\n",
        );

        let error = run(&auth).await.expect_err("it does not exist");

        assert!(matches!(error, Error::Start { .. }), "{error:?}");
        assert_eq!(error.command(), "eks-test-no-such-credential-helper");
    }

    #[tokio::test]
    async fn an_exec_block_with_no_command_cannot_be_started_rather_than_panicking() {
        let auth = exec_auth("exec:\n  args: ['get-token']\n");

        let error = run(&auth).await.expect_err("there is nothing to run");

        assert!(matches!(error, Error::Start { .. }), "{error:?}");
    }

    #[tokio::test]
    async fn a_helper_that_asks_a_question_with_nobody_there_gets_end_of_file() {
        // `read` on a closed stdin fails at once; on an inherited terminal it
        // would wait for somebody to type.
        let auth = sh(r#"read answer || echo '{"status": {"token": "nobody-answered"}}'"#);

        let credential = tokio::time::timeout(std::time::Duration::from_secs(10), run(&auth))
            .await
            .expect("a helper with no stdin cannot wait on it")
            .unwrap();

        assert_eq!(
            credential.secret,
            Secret::Token("nobody-answered".to_owned())
        );
    }
}
