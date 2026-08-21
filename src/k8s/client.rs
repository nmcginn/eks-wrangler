//! Building a Kubernetes client, and translating its failures into English.
//!
//! Two jobs live here, and the second one is the interesting one.
//!
//! Building a client is *nearly* mechanical: read the same kubeconfig files the
//! rest of the tool reads, pick a context, hand it to `kube`. No network
//! traffic happens here, so nothing on this path can stall a first paint. It is
//! not free of side effects, though, and the side effect is the reason this
//! module is more than twenty lines: `kube` resolves the auth layer eagerly, so
//! a context with an `exec` block runs `aws eks get-token` inside
//! `Client::try_from` rather than on the first request. That is why building a
//! client can fail with a credential error, and why [`explain`] is used on both
//! paths.
//!
//! It runs it with a *blocking* `std::process::Command`, on whatever thread
//! asked. A credential helper that never comes back — an expired SSO session
//! that wants a browser login, a laptop that has lost its route to the SSO
//! endpoint — therefore blocks the thread rather than the future, and a
//! `tokio::time::timeout` wrapped around this function would never fire. So the
//! build goes onto a blocking task, where the timeout has something to interrupt:
//! [`connect`] takes the same [`Budget`] the requests after it take, and spends
//! it on the helper too.
//!
//! Abandoning a blocking task does not stop it — nothing here can kill a
//! subprocess `kube` owns — so the other half of that timeout is
//! [`crate::commands::block_on`], which shuts the runtime down instead of
//! dropping it. Dropping one waits for its blocking tasks, and waiting for this
//! one is the exact hang the budget was written to end.
//!
//! Translating failures is the job that earns its keep. An EKS cluster whose
//! SSO session expired answers with `401 Unauthorized`, and `kube` reports that
//! faithfully as `ApiError: ... (Status { code: 401 ... })`. That is a correct
//! sentence about HTTP and a useless one for the person at the keyboard, whose
//! actual problem is that they need to run `aws sso login`. [`explain`] is
//! where that translation happens, and it is a pure function so every message
//! is asserted on in tests rather than provoked from a cluster.
//! [`stalled_helper`] is the same idea for the failure above, which has no
//! `kube::Error` behind it to classify.

use std::path::PathBuf;
use std::time::Duration;

use kube::config::{AuthInfo, KubeConfigOptions, Kubeconfig};
use kube::{Client, Config};

use crate::cluster::ClusterView;
use crate::format;
use crate::k8s::page::{self, Budget};

/// Failures from building a client, before any resource is requested.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no kubeconfig found (looked at {})", format_paths(.searched))]
    NotFound { searched: Vec<PathBuf> },

    #[error("{path} could not be read as a kubeconfig: {message}")]
    Read { path: PathBuf, message: String },

    #[error(
        "kubeconfig context {context:?} could not be used: {message}\n\
         Check that the cluster and user entries it names still exist."
    )]
    Context { context: String, message: String },

    /// A failure that already carries a user-facing explanation, from
    /// [`explain`].
    #[error("{0}")]
    Cluster(String),

    /// The context's credential helper was still running when the budget ran
    /// out. Carries its explanation from [`stalled_helper`], which is a
    /// separate wording because there is no `kube::Error` behind this one to
    /// classify — nothing was asked of the cluster yet.
    #[error("{0}")]
    HelperStalled(String),

    /// The thread building the client stopped without answering. Only a panic
    /// inside `kube` can do this, so there is no advice to give beyond what it
    /// said on its way down.
    #[error(
        "building a client for {cluster} stopped unexpectedly: {message}\n\
         That is a bug in eks or in the kube crate rather than something you did; \
         please report it with the output of `eks -vv`."
    )]
    Interrupted { cluster: String, message: String },
}

fn format_paths(paths: &[PathBuf]) -> String {
    if paths.is_empty() {
        return "<nothing>".to_owned();
    }
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Build a client for one cluster, giving up if the credential helper does not
/// come back inside `budget`.
///
/// `paths` is the same list the rest of the tool reads, so `--kubeconfig` and a
/// multi-file `KUBECONFIG` behave identically here and in `eks contexts`.
/// Missing files are skipped rather than fatal, matching `kubectl`.
///
/// No network request is made, but the context's credential helper does run —
/// see the module documentation — so failures here are translated with
/// [`explain`] and name the cluster the way the user does.
///
/// `budget` is `--timeout`, the same value each request after this one is given.
/// It is spent per step rather than shared: a helper that takes twenty seconds
/// to refresh an SSO token has not used up the listing's time, any more than one
/// page of that listing uses up the next page's.
pub async fn connect(
    paths: &[PathBuf],
    cluster: &ClusterView,
    budget: Budget,
) -> Result<Client, Error> {
    let kubeconfig = read_merged(paths)?;

    let options = KubeConfigOptions {
        context: Some(cluster.context_name.clone()),
        cluster: None,
        user: None,
    };

    let config = Config::from_custom_kubeconfig(kubeconfig, &options)
        .await
        .map_err(|source| Error::Context {
            context: cluster.context_name.clone(),
            message: source.to_string(),
        })?;

    let label = cluster.label();
    // Read before `config` moves onto the blocking task, because the message
    // for a helper that never answers has to name the command to run by hand.
    let helper = helper_command(&config.auth_info);

    // The one blocking call in the tool, and the reason it is on a blocking
    // task: `Client::try_from` resolves the auth layer, which runs the
    // kubeconfig's exec plugin with `std::process::Command::output`. On this
    // thread that would block the timer below along with everything else.
    let task = tokio::task::spawn_blocking(move || Client::try_from(config));

    let finished = match budget.limit() {
        // `--timeout 0`: the user asked to wait, so wait.
        None => task.await,
        Some(limit) => {
            // Dropping the join handle abandons the task; it does not stop it,
            // and nothing here can kill a subprocess `kube` owns. Declining to
            // wait for it is `commands::block_on`'s half of this.
            let Ok(finished) = tokio::time::timeout(limit, task).await else {
                tracing::debug!(?limit, "the credential helper outlived the budget");
                return Err(Error::HelperStalled(stalled_helper(
                    &label,
                    helper.as_deref(),
                    limit,
                )));
            };
            finished
        }
    };

    finished
        .map_err(|source| Error::Interrupted {
            cluster: label.clone(),
            message: source.to_string(),
        })?
        .map_err(|source| {
            tracing::debug!(%source, "building a client failed");
            Error::Cluster(explain(&source.into(), &label))
        })
}

/// The command a context runs to get its credentials, spelled the way a user
/// could paste it into a shell, or `None` for a context that authenticates
/// some other way — a bare token, a client certificate, an in-cluster service
/// account.
///
/// The `exec` block's environment comes out in front of it, as `NAME=value`
/// assignments, because the point of printing the line is that running it
/// reproduces what just hung. An EKS entry that sets `AWS_PROFILE` and is
/// pasted without it runs against whatever profile the shell already had, which
/// may well answer instantly — sending the user off to look for a problem
/// somewhere else.
///
/// Pure over the `AuthInfo` the kubeconfig produced, so the wording around a
/// stalled helper is tested against a fixture rather than provoked from a real
/// `aws eks get-token`.
#[must_use]
pub fn helper_command(auth: &AuthInfo) -> Option<String> {
    let exec = auth.exec.as_ref()?;
    let command = exec.command.as_deref()?;

    let mut line = String::new();

    // The same two keys `kube` reads, and the same silence about an entry
    // carrying anything else: a variable it will not pass is not one to print.
    for variable in exec.env.iter().flatten() {
        if let (Some(name), Some(value)) = (variable.get("name"), variable.get("value")) {
            // The name is written bare. A shell will not accept a quoted one on
            // the left of `=`, and a name that would need quoting is not a
            // variable name.
            line.push_str(name);
            line.push('=');
            line.push_str(&shell_word(value));
            line.push(' ');
        }
    }

    line.push_str(&shell_word(command));
    for argument in exec.args.iter().flatten() {
        line.push(' ');
        line.push_str(&shell_word(argument));
    }
    Some(line)
}

/// One word of a command line, quoted if a shell would need it quoted.
///
/// The point of printing the helper's command is that the user can run it
/// themselves, and an EKS `exec` block routinely carries an argument with a
/// space in it — a role ARN with a path, a profile named after a team. A line
/// they have to repair before it runs is worse than no line.
fn shell_word(word: &str) -> String {
    // The conservative set: anything a POSIX shell leaves alone unquoted.
    let plain = !word.is_empty()
        && word.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '=' | '@' | ',')
        });

    if plain {
        word.to_owned()
    } else {
        // The only way to get a single quote inside single quotes: end the
        // string, escape one, start again.
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// Read and merge every kubeconfig file that exists, in precedence order.
fn read_merged(paths: &[PathBuf]) -> Result<Kubeconfig, Error> {
    let mut merged: Option<Kubeconfig> = None;

    for path in paths {
        if !path.exists() {
            continue;
        }

        let parsed = Kubeconfig::read_from(path).map_err(|source| Error::Read {
            path: path.clone(),
            message: source.to_string(),
        })?;

        merged = Some(match merged {
            // `merge` keeps entries from `self` when names collide, which is the
            // precedence order kubectl documents: the first file wins.
            Some(existing) => existing.merge(parsed).map_err(|source| Error::Read {
                path: path.clone(),
                message: source.to_string(),
            })?,
            None => parsed,
        });
    }

    merged.ok_or_else(|| Error::NotFound {
        searched: paths.to_vec(),
    })
}

/// Why a request to a cluster failed, at the level of detail that decides what
/// advice the user gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// Credentials are absent, expired, or were refused by the API server.
    Credentials,
    /// The kubeconfig's credential helper could not even be started — usually
    /// the AWS CLI is not installed or not on `PATH`.
    HelperMissing,
    /// Authentication worked; authorisation did not.
    Forbidden,
    /// No answer from the API server at all.
    Unreachable,
    /// No answer within the time the user allowed, carried here because the
    /// advice has to name the budget it overran.
    Slow(Duration),
    /// A paged listing outlived the marker the cluster was keeping its place
    /// with.
    PageExpired,
    /// Anything we have no specific advice for.
    Other,
}

impl Failure {
    /// Classify a failed request.
    ///
    /// Deliberately coarse. Every arm has to lead to advice worth printing, and
    /// a wrong-but-plausible suggestion costs a user more time than an honest
    /// "here is the raw error".
    #[must_use]
    pub fn of(error: &page::Error) -> Self {
        match error {
            page::Error::TimedOut { limit } => Self::Slow(*limit),
            page::Error::Api(error) => Self::of_api(error),
        }
    }

    /// Classify a `kube` error, which is every failure that reached the cluster
    /// or tried to.
    fn of_api(error: &kube::Error) -> Self {
        match error {
            kube::Error::Api(status) => match status.code {
                401 => Self::Credentials,
                403 => Self::Forbidden,
                // Only a paged listing sends a continue token, and only an
                // expired one comes back as a `410`.
                410 => Self::PageExpired,
                _ => Self::Other,
            },
            // Every auth failure but one means "your credentials did not work".
            // The exception is the helper never running, which needs different
            // advice: install the tool, do not go looking for a fresh token.
            kube::Error::Auth(kube::client::AuthError::AuthExecStart(_)) => Self::HelperMissing,
            kube::Error::Auth(_) => Self::Credentials,
            // `Service` also carries middleware failures, but in practice the
            // connector is what fails: DNS, a refused connection, a timeout.
            kube::Error::Service(_) | kube::Error::HyperError(_) => Self::Unreachable,
            _ => Self::Other,
        }
    }
}

/// Turn a failed request into the message a user should see, naming the cluster
/// they were talking to and what to do next.
///
/// `cluster` is a human label such as `prod (us-east-1)`, not an ARN.
#[must_use]
pub fn explain(error: &page::Error, cluster: &str) -> String {
    match Failure::of(error) {
        Failure::Credentials => format!(
            "{cluster} rejected your credentials — they are missing or expired.\n\
             Refresh them and try again: `aws sso login`, or renew whichever AWS profile that context uses."
        ),
        Failure::HelperMissing => format!(
            "the credential helper for {cluster} could not be started.\n\
             Its kubeconfig entry runs a command — for EKS that is usually `aws` — which is not on your PATH. \
             Install the AWS CLI, or fix the `exec` block for that context."
        ),
        Failure::Forbidden => format!(
            "{cluster} knows who you are but will not let you list this resource.\n\
             Ask a cluster admin for an EKS access entry, or a role mapping in the aws-auth ConfigMap."
        ),
        Failure::Unreachable => format!(
            "could not reach the API server for {cluster}.\n\
             Check your network, and note that a private EKS endpoint only answers from inside its VPC or over a VPN."
        ),
        // The silent cousin of `Unreachable`, and the reason `--timeout` exists:
        // a private endpoint reached from outside its VPC does not refuse the
        // connection, it simply never answers. Same advice, plus the way out for
        // the other case — a cluster that is only slow.
        Failure::Slow(limit) => format!(
            "{cluster} did not answer within {}.\n\
             A private EKS endpoint only answers from inside its VPC or over a VPN. \
             If the cluster is merely busy, allow it longer: `--timeout {}`.",
            format::exact_duration(limit),
            Budget::of(limit.checked_mul(2).unwrap_or(limit)),
        ),
        Failure::PageExpired => format!(
            "{cluster} lost its place partway through this listing.\n\
             A listing too large for one response is read in pages, and the marker between them \
             expires after a few minutes. Run it again; if it keeps happening, ask for less at once."
        ),
        // No advice worth inventing, so show the real thing rather than a
        // reassuring guess.
        Failure::Other => format!("talking to {cluster} failed: {error}"),
    }
}

/// The message for a credential helper that was still running when its budget
/// ran out.
///
/// Beside [`explain`] rather than inside it, and deliberately: nothing has been
/// asked of the cluster yet, so there is no `page::Error` to classify and no
/// `Failure` this could be. The advice is about the AWS CLI on this machine
/// rather than about a VPC, which is the whole reason the failure is worth
/// telling apart from `Failure::Slow`.
///
/// `helper` is [`helper_command`]'s answer: the command line to run by hand,
/// or `None` for a context with no `exec` block — which should not reach here,
/// since nothing else on this path blocks, but is worded rather than
/// `unwrap`ped.
#[must_use]
pub fn stalled_helper(cluster: &str, helper: Option<&str>, limit: Duration) -> String {
    // Naming the command is the actionable half: running it by hand is the only
    // way to see what it is stuck on, and it is deliberately not guessed at
    // here. `aws eks get-token` hangs for several unrelated reasons — a
    // blackholed IMDS address, an SSO endpoint with no route to it, a
    // `credential_process` of the user's own that prompts — and naming the
    // wrong one confidently would send them off to fix something that is fine.
    let named = match helper {
        Some(command) => format!("Its kubeconfig entry runs `{command}`, which has not come back"),
        None => "The command its kubeconfig entry runs has not come back".to_owned(),
    };

    format!(
        "getting credentials for {cluster} took longer than {}.\n\
         {named}. Run it yourself to see what it is waiting for — a machine that has lost its \
         route to AWS waits there rather than failing, and so does one prompting for a login. \
         Allow it longer with `--timeout {}`, or `--timeout 0` to wait for as long as it takes.",
        format::exact_duration(limit),
        Budget::of(limit.checked_mul(2).unwrap_or(limit)),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::io;
    use std::time::Instant;

    use kube::core::Status;

    use super::*;

    fn api_error(code: u16) -> page::Error {
        kube::Error::Api(
            Status::failure("denied", "Forbidden")
                .with_code(code)
                .boxed(),
        )
        .into()
    }

    /// A kubeconfig whose credential helper is a command that cannot exist, so
    /// building a client fails the way it would on a machine with no AWS CLI —
    /// without a cluster, credentials, or a network.
    const MISSING_HELPER: &str = r"
apiVersion: v1
kind: Config
current-context: prod
clusters:
  - name: prod
    cluster:
      server: https://127.0.0.1:6443
contexts:
  - name: prod
    context:
      cluster: prod
      user: prod
users:
  - name: prod
    user:
      exec:
        apiVersion: client.authentication.k8s.io/v1beta1
        command: eks-test-no-such-credential-helper
";

    /// A kubeconfig whose credential helper never comes back inside any budget
    /// a test would set — the shape of an `aws eks get-token` sitting on an SSO
    /// prompt, without an AWS CLI, an SSO endpoint, or a cluster.
    ///
    /// Thirty seconds rather than for ever: if the abandonment ever regresses,
    /// the test that uses this fails on its own assertion after half a minute
    /// instead of hanging CI until somebody cancels it.
    const SLOW_HELPER: &str = r"
apiVersion: v1
kind: Config
current-context: prod
clusters:
  - name: prod
    cluster:
      server: https://127.0.0.1:6443
contexts:
  - name: prod
    context:
      cluster: prod
      user: prod
users:
  - name: prod
    user:
      exec:
        apiVersion: client.authentication.k8s.io/v1beta1
        command: sleep
        args: ['30']
        interactiveMode: Never
";

    /// An `AuthInfo` carrying the exec block a kubeconfig would have parsed.
    fn exec_auth(command: Option<&str>, args: &[&str]) -> AuthInfo {
        AuthInfo {
            exec: Some(kube::config::ExecConfig {
                command: command.map(ToOwned::to_owned),
                args: Some(args.iter().map(|a| (*a).to_owned()).collect()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// The same, with the `env` list a kubeconfig spells as `name`/`value`
    /// pairs — which is how `kube` reads it too.
    fn exec_auth_with_env(command: &str, args: &[&str], env: &[(&str, &str)]) -> AuthInfo {
        let mut auth = exec_auth(Some(command), args);
        if let Some(exec) = auth.exec.as_mut() {
            exec.env = Some(
                env.iter()
                    .map(|(name, value)| {
                        [
                            ("name".to_owned(), (*name).to_owned()),
                            ("value".to_owned(), (*value).to_owned()),
                        ]
                        .into_iter()
                        .collect()
                    })
                    .collect(),
            );
        }
        auth
    }

    fn write_kubeconfig(dir: &std::path::Path, yaml: &str) -> PathBuf {
        let path = dir.join("config");
        std::fs::write(&path, yaml).unwrap();
        path
    }

    fn view(context_name: &str) -> ClusterView {
        ClusterView {
            context_name: context_name.to_owned(),
            display_name: "prod".to_owned(),
            region: Some("us-east-1".to_owned()),
            account_id: None,
            namespace: "default".to_owned(),
            is_current: true,
        }
    }

    #[test]
    fn an_expired_token_is_reported_as_a_credential_problem() {
        // EKS answers 401 once the SSO session behind `aws eks get-token` dies.
        let message = explain(&api_error(401), "prod (us-east-1)");

        assert_eq!(Failure::of(&api_error(401)), Failure::Credentials);
        assert!(message.contains("prod (us-east-1)"), "{message}");
        assert!(message.contains("aws sso login"), "{message}");
        assert!(
            !message.contains("401"),
            "raw HTTP status leaked: {message}"
        );
    }

    #[test]
    fn a_failed_credential_helper_is_a_credential_problem_too() {
        // `aws eks get-token` ran and exited non-zero — an expired SSO cache is
        // the common cause.
        let error = page::Error::from(kube::Error::Auth(kube::client::AuthError::ExecPluginFailed));

        assert_eq!(Failure::of(&error), Failure::Credentials);
        assert!(explain(&error, "prod").contains("aws sso login"));
    }

    #[test]
    fn a_missing_credential_helper_suggests_installing_it_not_logging_in() {
        let error = page::Error::from(kube::Error::Auth(kube::client::AuthError::AuthExecStart(
            io::Error::new(io::ErrorKind::NotFound, "no such file or directory"),
        )));

        assert_eq!(Failure::of(&error), Failure::HelperMissing);
        let message = explain(&error, "prod");
        assert!(message.contains("AWS CLI"), "{message}");
        assert!(!message.contains("aws sso login"), "{message}");
    }

    #[test]
    fn a_forbidden_response_talks_about_rbac_not_about_logging_in() {
        let message = explain(&api_error(403), "staging (eu-west-1)");

        assert_eq!(Failure::of(&api_error(403)), Failure::Forbidden);
        assert!(message.contains("staging (eu-west-1)"), "{message}");
        assert!(message.contains("access entry"), "{message}");
        assert!(!message.contains("aws sso login"), "{message}");
    }

    #[test]
    fn a_connection_failure_mentions_the_private_endpoint_trap() {
        let error = page::Error::from(kube::Error::Service(Box::new(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "connection refused",
        ))));

        assert_eq!(Failure::of(&error), Failure::Unreachable);
        let message = explain(&error, "prod (us-east-1)");
        assert!(message.contains("could not reach"), "{message}");
        assert!(message.contains("VPN"), "{message}");
    }

    #[test]
    fn a_request_that_ran_out_of_time_names_the_budget_and_a_bigger_one() {
        // The failure `--timeout` exists for, and the one where "check your
        // network" on its own is only half the advice: the other half is that
        // the cluster may simply be slower than the budget allowed.
        let error = page::Error::TimedOut {
            limit: Duration::from_secs(30),
        };
        let message = explain(&error, "prod (us-east-1)");

        assert_eq!(Failure::of(&error), Failure::Slow(Duration::from_secs(30)));
        assert!(message.contains("prod (us-east-1)"), "{message}");
        assert!(message.contains("within 30s"), "{message}");
        assert!(message.contains("--timeout 1m"), "{message}");
        assert!(message.contains("VPN"), "{message}");
    }

    #[test]
    fn the_suggested_budget_is_one_a_user_could_type() {
        // The doubling goes through `Budget`'s own spelling, so the advice can
        // never suggest a value the flag would reject.
        let doubled = |seconds| {
            explain(
                &page::Error::TimedOut {
                    limit: Duration::from_secs(seconds),
                },
                "prod",
            )
        };

        assert!(doubled(45).contains("--timeout 90s"), "{}", doubled(45));
        assert!(doubled(30).contains("--timeout 1m"), "{}", doubled(30));
    }

    #[test]
    fn an_expired_page_marker_says_to_run_it_again_not_to_log_in() {
        // A `410` only ever reaches us partway through a paged listing, and it
        // is not the user's fault or their credentials'.
        let message = explain(&api_error(410), "prod (us-east-1)");

        assert_eq!(Failure::of(&api_error(410)), Failure::PageExpired);
        assert!(message.contains("prod (us-east-1)"), "{message}");
        assert!(message.contains("Run it again"), "{message}");
        assert!(!message.contains("aws sso login"), "{message}");
        assert!(
            !message.contains("410"),
            "raw HTTP status leaked: {message}"
        );
    }

    #[test]
    fn an_unclassified_failure_shows_the_underlying_error() {
        // Better an ugly truth than a confident guess: a 500 from the API
        // server has no advice we could honestly give.
        let error = api_error(500);
        let message = explain(&error, "prod");

        assert_eq!(Failure::of(&error), Failure::Other);
        assert!(message.contains("prod"), "{message}");
        assert!(message.contains("denied"), "{message}");
    }

    #[test]
    fn every_message_names_the_cluster_it_is_about() {
        let errors = [
            api_error(401),
            api_error(403),
            api_error(410),
            api_error(500),
            kube::Error::Auth(kube::client::AuthError::MissingCommand).into(),
            kube::Error::Service(Box::new(io::Error::other("boom"))).into(),
            page::Error::TimedOut {
                limit: Duration::from_secs(30),
            },
        ];

        for error in &errors {
            let message = explain(error, "prod (us-east-1)");
            assert!(
                message.contains("prod (us-east-1)"),
                "{:?} produced a message with no cluster in it: {message}",
                Failure::of(error)
            );
        }
    }

    #[tokio::test]
    async fn a_context_with_no_usable_credential_helper_gets_the_friendly_message() {
        // Not a unit test of `explain` but of the path a user actually walks:
        // `kube` runs the exec plugin while building the client, so this is
        // where a laptop without the AWS CLI finds out.
        //
        // Under `--timeout 0`, which is also the only test of that branch: an
        // unlimited budget must still await the blocking task rather than skip
        // it, and a helper that cannot start still has to reach `explain`.
        let dir = tempfile::tempdir().unwrap();
        let paths = vec![write_kubeconfig(dir.path(), MISSING_HELPER)];

        // `Client` is not `Debug`, so unwrap the result by hand.
        let Err(error) = connect(&paths, &view("prod"), Budget::unlimited()).await else {
            panic!("a helper that does not exist cannot be run");
        };

        let message = error.to_string();
        assert!(message.contains("prod (us-east-1)"), "{message}");
        assert!(message.contains("AWS CLI"), "{message}");
        assert!(
            !message.contains("os error"),
            "raw io error leaked: {message}"
        );
    }

    #[tokio::test]
    async fn a_helper_that_answers_inside_the_budget_is_not_reported_as_stalled() {
        // The other side of the timeout, and the one a wrong `match` arm would
        // break silently: this helper fails immediately — it does not exist —
        // so the budget has nothing to expire on, and the message must be the
        // one about the AWS CLI rather than the one about waiting.
        let dir = tempfile::tempdir().unwrap();
        let paths = vec![write_kubeconfig(dir.path(), MISSING_HELPER)];

        let Err(error) = connect(&paths, &view("prod"), Budget::of(Duration::from_secs(30))).await
        else {
            panic!("a helper that does not exist cannot be run");
        };

        let message = error.to_string();
        assert!(message.contains("AWS CLI"), "{message}");
        assert!(
            !message.contains("took longer than"),
            "a helper that failed at once was reported as slow: {message}"
        );
    }

    #[tokio::test]
    async fn connecting_to_a_context_the_kubeconfig_does_not_have_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let paths = vec![write_kubeconfig(dir.path(), MISSING_HELPER)];

        let Err(error) = connect(&paths, &view("staging"), Budget::default()).await else {
            panic!("there is no staging context in that file");
        };

        let message = error.to_string();
        assert!(message.contains("\"staging\""), "{message}");
        assert!(message.contains("cluster and user entries"), "{message}");
    }

    #[test]
    fn connecting_with_no_kubeconfig_files_says_where_it_looked() {
        let missing = vec![
            PathBuf::from("/nope/one/config"),
            PathBuf::from("/nope/two/config"),
        ];

        let error = read_merged(&missing).expect_err("a config that is not there cannot be read");

        let message = error.to_string();
        assert!(message.contains("/nope/one/config"), "{message}");
        assert!(message.contains("/nope/two/config"), "{message}");
    }

    #[test]
    fn the_helper_command_is_spelled_the_way_it_could_be_typed() {
        let auth = exec_auth(
            Some("aws"),
            &[
                "--region",
                "us-east-1",
                "eks",
                "get-token",
                "--cluster-name",
                "prod",
            ],
        );

        assert_eq!(
            helper_command(&auth).as_deref(),
            Some("aws --region us-east-1 eks get-token --cluster-name prod")
        );
    }

    #[test]
    fn a_helper_argument_with_a_space_in_it_is_quoted() {
        // An EKS exec block routinely carries one: a role ARN with a path, or a
        // profile named after a team. Printing it bare would give the user a
        // line that breaks into two words when they paste it.
        let auth = exec_auth(
            Some("aws"),
            &["--profile", "prod admin", "eks", "get-token"],
        );

        assert_eq!(
            helper_command(&auth).as_deref(),
            Some("aws --profile 'prod admin' eks get-token")
        );
    }

    #[test]
    fn a_helper_argument_with_a_quote_in_it_still_pastes() {
        // The awkward one: a single quote cannot be escaped inside single
        // quotes, so the word has to be closed, the quote escaped, and the word
        // reopened. Getting this wrong produces a line that hangs a shell.
        let auth = exec_auth(Some("aws"), &["--profile", "o'brien"]);

        assert_eq!(
            helper_command(&auth).as_deref(),
            Some(r"aws --profile 'o'\''brien'")
        );
    }

    #[test]
    fn the_helper_environment_comes_out_in_front_of_the_command() {
        // Without it the pasted line runs against whatever profile the shell
        // already had, which may answer at once — and then the user is looking
        // for a problem somewhere that does not have one.
        let auth = exec_auth_with_env(
            "aws",
            &["eks", "get-token"],
            &[("AWS_PROFILE", "prod admin"), ("AWS_REGION", "us-east-1")],
        );

        assert_eq!(
            helper_command(&auth).as_deref(),
            Some("AWS_PROFILE='prod admin' AWS_REGION=us-east-1 aws eks get-token")
        );
    }

    #[test]
    fn a_helper_environment_entry_missing_a_name_or_a_value_is_skipped() {
        // `kube` drops those entries rather than passing them, so printing one
        // would put a variable in the line that the helper never had.
        let mut auth = exec_auth(Some("aws"), &["eks", "get-token"]);
        if let Some(exec) = auth.exec.as_mut() {
            exec.env = Some(vec![
                [("name".to_owned(), "AWS_PROFILE".to_owned())]
                    .into_iter()
                    .collect(),
                [("value".to_owned(), "orphaned".to_owned())]
                    .into_iter()
                    .collect(),
            ]);
        }

        assert_eq!(helper_command(&auth).as_deref(), Some("aws eks get-token"));
    }

    #[test]
    fn an_empty_helper_argument_survives_as_an_empty_argument() {
        // A shell splits on whitespace, so an empty word printed bare would
        // vanish and the pasted line would run with one argument fewer than the
        // one that hung.
        let auth = exec_auth(Some("aws"), &["--profile", "", "eks"]);

        assert_eq!(
            helper_command(&auth).as_deref(),
            Some("aws --profile '' eks")
        );
    }

    #[test]
    fn a_helper_with_no_arguments_is_just_its_command() {
        let auth = exec_auth(Some("get-token.sh"), &[]);

        assert_eq!(helper_command(&auth).as_deref(), Some("get-token.sh"));
    }

    #[test]
    fn a_context_that_runs_no_helper_has_no_command_to_name() {
        // A bare token, a client certificate, an in-cluster service account:
        // three ways to authenticate with nothing to run and nothing to print.
        assert_eq!(helper_command(&AuthInfo::default()), None);

        // And the malformed case: an `exec` block with no `command` in it,
        // which `kube` rejects later with `MissingCommand`.
        assert_eq!(helper_command(&exec_auth(None, &["get-token"])), None);
    }

    #[test]
    fn a_stalled_helper_names_the_command_and_a_bigger_budget() {
        let message = stalled_helper(
            "prod (us-east-1)",
            Some("aws eks get-token --cluster-name prod"),
            Duration::from_secs(5),
        );

        assert!(message.contains("prod (us-east-1)"), "{message}");
        assert!(
            message.contains("`aws eks get-token --cluster-name prod`"),
            "{message}"
        );
        assert!(message.contains("longer than 5s"), "{message}");
        assert!(message.contains("--timeout 10s"), "{message}");
        assert!(message.contains("--timeout 0"), "{message}");
    }

    #[test]
    fn a_stalled_helper_is_not_given_the_clusters_advice() {
        // The distinction the failure exists for: `Failure::Slow` is a cluster
        // that went quiet, and its advice is about private endpoints, VPCs, and
        // VPNs. None of that is true of a subprocess on the user's own machine,
        // and sending them to check a VPN over it would waste their afternoon.
        let message = stalled_helper("prod", Some("aws eks get-token"), Duration::from_secs(30));

        assert!(!message.contains("VPN"), "{message}");
        assert!(!message.contains("VPC"), "{message}");
        assert!(!message.contains("API server"), "{message}");
        assert!(message.contains("Run it yourself"), "{message}");
    }

    #[test]
    fn a_stalled_helper_with_no_command_to_name_still_reads_as_a_sentence() {
        // Should not arise — nothing else on that path blocks — but a message
        // with a hole in it is worse than a vaguer one, and `unwrap` is denied.
        let message = stalled_helper("prod", None, Duration::from_secs(30));

        assert!(message.contains("prod"), "{message}");
        assert!(message.contains("has not come back"), "{message}");
        assert!(
            !message.contains("``"),
            "empty command left a hole: {message}"
        );
    }

    #[test]
    fn the_suggested_budget_after_a_stall_is_one_a_user_could_type() {
        // The same round trip `explain` depends on: the doubling is spelled
        // through `Budget`, so the advice cannot name a value the flag rejects.
        let doubled = |millis| stalled_helper("prod", Some("aws"), Duration::from_millis(millis));

        assert!(doubled(250).contains("--timeout 500ms"), "{}", doubled(250));
        assert!(
            doubled(30_000).contains("--timeout 1m"),
            "{}",
            doubled(30_000)
        );
    }

    #[test]
    fn an_interrupted_build_says_it_is_a_bug_rather_than_the_users_fault() {
        // Only a panic inside `kube` reaches this, so it is provoked by
        // construction rather than by making a dependency fall over.
        let error = Error::Interrupted {
            cluster: "prod (us-east-1)".to_owned(),
            message: "task panicked".to_owned(),
        };

        let message = error.to_string();
        assert!(message.contains("prod (us-east-1)"), "{message}");
        assert!(message.contains("bug"), "{message}");
        assert!(message.contains("eks -vv"), "{message}");
    }

    #[test]
    fn a_credential_helper_that_never_answers_is_given_up_on_and_left_behind() {
        // The acceptance criterion, end to end and without a cluster: a helper
        // that will not exit for thirty seconds, a budget of a quarter of a
        // second, and a command that has to be back long before either.
        //
        // Through `commands::block_on` on purpose, because the second half of
        // the fix lives there: `connect` can only abandon the blocking task,
        // and dropping the runtime would wait out the full thirty seconds at
        // the door. A plain `#[tokio::test]` here would pass the timeout and
        // still hang.
        let dir = tempfile::tempdir().unwrap();
        let paths = vec![write_kubeconfig(dir.path(), SLOW_HELPER)];

        let started = Instant::now();
        let message = crate::commands::block_on(async move {
            match connect(
                &paths,
                &view("prod"),
                Budget::of(Duration::from_millis(250)),
            )
            .await
            {
                // `Client` is not `Debug`, so say it rather than unwrap it.
                Ok(_) => Ok(String::new()),
                Err(error) => Ok(error.to_string()),
            }
        })
        .unwrap();
        let elapsed = started.elapsed();

        assert!(
            !message.is_empty(),
            "a helper that sleeps for thirty seconds cannot have returned a token"
        );
        assert!(message.contains("prod (us-east-1)"), "{message}");
        assert!(message.contains("`sleep 30`"), "{message}");
        assert!(message.contains("--timeout 500ms"), "{message}");
        assert!(
            !message.contains("VPN"),
            "the cluster is blameless here: {message}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "waited {elapsed:?} for a helper nothing should have waited for"
        );
    }
}
