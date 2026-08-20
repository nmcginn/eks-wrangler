//! Building a Kubernetes client, and translating its failures into English.
//!
//! Two jobs live here, and the second one is the interesting one.
//!
//! Building a client is mechanical: read the same kubeconfig files the rest of
//! the tool reads, pick a context, hand it to `kube`. No network traffic
//! happens here, so nothing on this path can stall a first paint. It is *not*
//! free of side effects, though: `kube` resolves the auth layer eagerly, so a
//! context with an `exec` block runs `aws eks get-token` while the client is
//! being built rather than on the first request. That is why building a client
//! can fail with a credential error, and why [`explain`] is used on both paths.
//!
//! One thing that follows from that eager resolution is worth stating plainly,
//! because it is a limit on what `--timeout` can promise: `kube` runs the exec
//! plugin with a *blocking* `std::process::Command`, so a credential helper
//! that hangs hangs the thread rather than the future, and no timeout wrapped
//! around this function would ever fire. [`Budget`] therefore covers requests
//! to the cluster — see [`crate::k8s::page`] — and the flag's help says so.
//!
//! Translating failures is the job that earns its keep. An EKS cluster whose
//! SSO session expired answers with `401 Unauthorized`, and `kube` reports that
//! faithfully as `ApiError: ... (Status { code: 401 ... })`. That is a correct
//! sentence about HTTP and a useless one for the person at the keyboard, whose
//! actual problem is that they need to run `aws sso login`. [`explain`] is
//! where that translation happens, and it is a pure function so every message
//! is asserted on in tests rather than provoked from a cluster.

use std::path::PathBuf;
use std::time::Duration;

use kube::config::{KubeConfigOptions, Kubeconfig};
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

/// Build a client for one cluster.
///
/// `paths` is the same list the rest of the tool reads, so `--kubeconfig` and a
/// multi-file `KUBECONFIG` behave identically here and in `eks contexts`.
/// Missing files are skipped rather than fatal, matching `kubectl`.
///
/// No network request is made, but the context's credential helper does run —
/// see the module documentation — so failures here are translated with
/// [`explain`] and name the cluster the way the user does.
pub async fn connect(paths: &[PathBuf], cluster: &ClusterView) -> Result<Client, Error> {
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

    Client::try_from(config).map_err(|source| {
        tracing::debug!(%source, "building a client failed");
        Error::Cluster(explain(&source.into(), &cluster.label()))
    })
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::io;

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
        let dir = tempfile::tempdir().unwrap();
        let paths = vec![write_kubeconfig(dir.path(), MISSING_HELPER)];

        // `Client` is not `Debug`, so unwrap the result by hand.
        let Err(error) = connect(&paths, &view("prod")).await else {
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
    async fn connecting_to_a_context_the_kubeconfig_does_not_have_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let paths = vec![write_kubeconfig(dir.path(), MISSING_HELPER)];

        let Err(error) = connect(&paths, &view("staging")).await else {
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
}
