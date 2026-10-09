//! Running the AWS CLI and reading what it says back (decision 123).
//!
//! `eks` calls AWS APIs beyond login by running `aws … --output json` as a
//! child process, not through an SDK. Three things live here:
//!
//! - [`Call`], one invocation: the argv, the environment the context's own
//!   credential helper would have run under, and the IAM action the call
//!   needs, so a refusal can name it even when the CLI's message does not.
//! - [`run`], the I/O: a `tokio` child with `kill_on_drop`, raced against
//!   `--timeout` the way [`crate::k8s::exec`] races the credential helper, so
//!   dropping the future — a timeout, a Ctrl-C — stops the process rather than
//!   abandoning it.
//! - [`Failure::classify`] and [`Failure::explain`], pure functions from the
//!   CLI's stderr to a sentence that says what to do next. The CLI's own
//!   messages are written for someone who already knows which IAM action an
//!   operation maps to; ours name it.
//!
//! The CLI is never allowed to talk to the terminal. Its stdin is `/dev/null`,
//! so a profile that wants an MFA code fails at once instead of waiting on a
//! prompt nobody can see, and its pager and auto-prompt are switched off,
//! because a CLI v2 that thinks it has a terminal opens `less` over the
//! output `eks` is trying to read.

use std::io;
use std::process::Stdio;
use std::time::Duration;

use crate::format;
use crate::k8s::client;
use crate::k8s::page::Budget;
use crate::launch;

/// Where to get the AWS CLI, for the message that says it is missing or too
/// old.
pub const INSTALL_URL: &str = launch::AWS_INSTALL_URL;

/// The oldest AWS CLI major version `eks` is written against.
pub const REQUIRED_MAJOR: u32 = 2;

/// Where a call was made from, which changes only the advice: a flag to pass
/// on the command line, or a key to press in the dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// `eks control-plane-logs` or `eks logs`.
    Command,
    /// The dashboard's control-plane pane.
    Dashboard,
}

/// One run of the AWS CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    /// The program, then its arguments. The program is `aws` everywhere but
    /// in tests, which point it at a stand-in.
    pub argv: Vec<String>,
    /// Layered over this process's own environment: the context's `exec`
    /// block entries, so the CLI reads the same `~/.aws/config` and profile
    /// the credential helper does.
    pub env: Vec<(String, String)>,
    /// The IAM action this call needs, `logs:FilterLogEvents`, for the
    /// message when it is refused.
    pub action: &'static str,
}

impl Call {
    /// The command as somebody would type it, without the `--output json`
    /// that is ours rather than theirs.
    #[must_use]
    pub fn line(&self) -> String {
        let mut words: Vec<&str> = Vec::with_capacity(self.argv.len());
        let mut args = self.argv.iter();
        while let Some(arg) = args.next() {
            if arg == "--output" {
                args.next();
                continue;
            }
            words.push(arg);
        }
        words
            .iter()
            .map(|word| client::shell_word(word))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The first three words, `aws logs filter-log-events`: what the progress
    /// line and a timeout name.
    #[must_use]
    pub fn short(&self) -> String {
        self.argv
            .iter()
            .take(3)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Why a call produced no answer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The program could not be started at all. `why` is worked out at the
    /// moment it failed — see [`crate::launch`] — and boxed because it is the
    /// rare case, and every `Result` carrying this error would otherwise be
    /// sized for it.
    #[error("could not start `{program}`: {why}")]
    Start {
        program: String,
        why: Box<launch::NotStarted>,
    },

    /// `--timeout` ran out first, and the child was killed.
    #[error("`{command}` did not finish within {}", format::exact_duration(*limit))]
    TimedOut { command: String, limit: Duration },

    /// The CLI ran and exited unsuccessfully.
    #[error("`{command}` failed: {stderr}")]
    Failed {
        command: String,
        code: Option<i32>,
        stderr: String,
    },
}

/// Run one call to completion and hand back its stdout.
///
/// The child is killed if this future is dropped, which is what lets
/// `--timeout` and Ctrl-C stop it. The budget is spent per call, as it is
/// per page of a cluster listing: a long `--since` is several calls, and
/// should not be cut off for its length, only for one of them going quiet.
pub async fn run(call: &Call, budget: Budget) -> Result<Vec<u8>, Error> {
    let Some((program, rest)) = call.argv.split_first() else {
        return Err(Error::Start {
            program: String::new(),
            why: Box::new(launch::NotStarted::Other {
                program: String::new(),
                reason: "there is no command to run".to_owned(),
            }),
        });
    };

    let mut child = tokio::process::Command::new(program);
    child
        .args(rest)
        .envs(call.env.iter().map(|(name, value)| (name, value)))
        // A CLI v2 that believes it has a terminal pipes JSON through `less`
        // and may open its interactive prompt; neither is ours to show.
        .env("AWS_PAGER", "")
        .env("AWS_CLI_AUTO_PROMPT", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        // A process group of its own, so a profile whose credential process
        // opens `/dev/tty` to ask for something is refused by the terminal
        // instead of reading the user's keystrokes. The same reasoning as the
        // dashboard's credential helpers (decision 120).
        child.process_group(0);
    }

    let start = |source: io::Error| Error::Start {
        program: program.clone(),
        why: Box::new(launch::explain(program, &source)),
    };
    let waiting = async {
        child
            .spawn()
            .map_err(start)?
            .wait_with_output()
            .await
            .map_err(start)
    };

    let output = match budget.limit() {
        Some(limit) => {
            tokio::time::timeout(limit, waiting)
                .await
                .map_err(|_| Error::TimedOut {
                    command: call.short(),
                    limit,
                })??
        }
        None => waiting.await?,
    };

    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(Error::Failed {
            command: call.short(),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

/// What `aws --version` says, for the message about a CLI too old for the
/// arguments it was given. Asked only after a usage error, so a working CLI
/// never pays for a second start-up.
///
/// Read from stdout and stderr both: version 1 printed it on stderr.
pub async fn version(program: &str, budget: Budget) -> Option<String> {
    let mut child = tokio::process::Command::new(program);
    child
        .arg("--version")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = match budget.limit() {
        Some(limit) => tokio::time::timeout(limit, child.output()).await.ok()?,
        None => child.output().await,
    }
    .ok()?;
    [output.stdout, output.stderr]
        .iter()
        .map(|bytes| String::from_utf8_lossy(bytes).trim().to_owned())
        .find(|text| text.starts_with("aws-cli/"))
}

/// What a failed call means for the person running `eks`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// `aws` could not be started: not installed, not on the `PATH` this
    /// process has, or not runnable — [`crate::launch`] says which.
    Missing { why: launch::NotStarted },
    /// The CLI did not understand its own arguments: nearly always a CLI too
    /// old for them.
    Usage { detail: String },
    /// The caller is who AWS thinks, and may not do this.
    Denied {
        action: String,
        principal: Option<String>,
    },
    /// The credentials are gone or stale: an Identity Center session that
    /// ran out, an expired assumed-role token. A login can fix this one.
    Expired { detail: String },
    /// No credentials could be found at all.
    NoCredentials,
    /// The profile the context names is not in `~/.aws/config`.
    NoProfile { profile: String },
    /// The cluster or log group does not exist where we looked.
    NotFound { detail: String },
    /// AWS asked us to slow down and the CLI's own retries ran out.
    Throttled,
    /// No network path to the AWS endpoint.
    Unreachable { detail: String },
    /// `--timeout` ran out.
    TimedOut { limit: Duration },
    /// Anything else, as the CLI said it.
    Other { detail: String },
}

impl Failure {
    /// Read a failed call.
    ///
    /// `action` is the IAM action the call needed, used when the CLI's own
    /// message does not name one — a `403` from `eks:DescribeCluster` says
    /// "not authorized to perform: eks:DescribeCluster", but an SCP denial
    /// may say neither.
    #[must_use]
    pub fn of(error: &Error, action: &str) -> Self {
        match error {
            Error::Start { why, .. } => Self::Missing {
                why: why.as_ref().clone(),
            },
            Error::TimedOut { limit, .. } => Self::TimedOut { limit: *limit },
            Error::Failed { stderr, .. } => Self::classify(stderr, action),
        }
    }

    /// Read the CLI's stderr. Pure, so each message the CLI is known to print
    /// is a fixture.
    #[must_use]
    pub fn classify(stderr: &str, action: &str) -> Self {
        let detail = last_line(stderr);
        let code = error_code(stderr);

        if stderr.contains("aws: error: argument")
            || stderr.contains("Unknown options:")
            || stderr.contains("Invalid choice")
            || stderr.starts_with("usage: aws")
        {
            return Self::Usage { detail };
        }
        if stderr.contains("Unable to locate credentials") {
            return Self::NoCredentials;
        }
        if let Some(profile) = between(stderr, "The config profile (", ") could not be found") {
            return Self::NoProfile {
                profile: profile.to_owned(),
            };
        }
        let expired = matches!(
            code.as_deref(),
            Some(
                "ExpiredToken"
                    | "ExpiredTokenException"
                    | "UnrecognizedClientException"
                    | "InvalidClientTokenId"
            )
        ) || [
            "Token has expired",
            "SSO session associated with this profile has expired",
            "Error loading SSO Token",
            "Error when retrieving token from sso",
            "security token included in the request is expired",
            "security token included in the request is invalid",
        ]
        .iter()
        .any(|needle| stderr.contains(needle));
        if expired {
            return Self::Expired { detail };
        }
        if matches!(
            code.as_deref(),
            Some("AccessDeniedException" | "AccessDenied" | "UnauthorizedOperation")
        ) || stderr.contains("is not authorized to perform")
        {
            return Self::Denied {
                action: between(stderr, "not authorized to perform: ", " ").map_or_else(
                    || action.to_owned(),
                    |found| found.trim_end_matches(['.', ',']).to_owned(),
                ),
                principal: between(stderr, "User: ", " is not authorized").map(short_principal),
            };
        }
        if matches!(
            code.as_deref(),
            Some("ResourceNotFoundException" | "NotFoundException")
        ) {
            return Self::NotFound {
                detail: message_of(stderr).unwrap_or(detail),
            };
        }
        if matches!(
            code.as_deref(),
            Some("ThrottlingException" | "Throttling" | "TooManyRequestsException")
        ) {
            return Self::Throttled;
        }
        if stderr.contains("Could not connect to the endpoint URL")
            || stderr.contains("Connect timeout on endpoint URL")
        {
            return Self::Unreachable { detail };
        }
        Self::Other { detail }
    }

    /// Whether a fresh `aws sso login` could put this right.
    #[must_use]
    pub fn fixed_by_signing_in(&self) -> bool {
        matches!(self, Self::Expired { .. })
    }

    /// Whether this goes away on its own: worth trying again in a moment,
    /// as `--follow` does, rather than worth stopping for.
    #[must_use]
    pub fn passes(&self) -> bool {
        matches!(
            self,
            Self::Throttled | Self::Unreachable { .. } | Self::TimedOut { .. }
        )
    }

    /// The sentence for a person having a bad day. `call` is what failed,
    /// `profile` the profile it ran as (for advice that names one), and
    /// `found` the `aws --version` line when a usage error made it worth
    /// asking. `surface` decides whether the advice names a flag or a key.
    #[must_use]
    pub fn explain(
        &self,
        call: &Call,
        profile: &str,
        found: Option<&str>,
        surface: Surface,
    ) -> String {
        let command = call.short();
        match self {
            Self::Missing { why } => format!(
                "eks reads CloudWatch through the AWS CLI, and could not start it: {why}.\n{}",
                why.remedy(launch::Origin::Eks)
            ),
            Self::Usage { detail } => match found.and_then(major_version) {
                Some(major) if major < REQUIRED_MAJOR => format!(
                    "`{command}` did not accept the arguments eks gave it: your AWS CLI is \
                     {}, and eks needs version {REQUIRED_MAJOR} or later.\n\
                     Install it: {INSTALL_URL}",
                    found
                        .unwrap_or_default()
                        .split_whitespace()
                        .next()
                        .unwrap_or_default(),
                ),
                _ => format!(
                    "`{command}` did not accept the arguments eks gave it: {detail}\n\
                     This is a bug in eks if your AWS CLI is up to date; please report it \
                     with the output of `aws --version`."
                ),
            },
            Self::Denied { action, principal } => format!(
                "AWS refused `{command}`: {who} not allowed `{action}`.\n\
                 Ask whoever manages IAM for your account to grant `{action}` to the role \
                 profile {profile:?} signs in as.",
                who = principal
                    .as_deref()
                    .map_or_else(|| "you are".to_owned(), |name| format!("{name} is")),
            ),
            Self::Expired { detail } => format!(
                "AWS refused `{command}` because the credentials for profile {profile:?} \
                 have expired ({detail}).\n{}",
                match surface {
                    Surface::Command => format!(
                        "Sign in again, e.g. `aws sso login --profile {profile}`, and re-run this."
                    ),
                    Surface::Dashboard => {
                        "Press L to sign in again; the pane reads again once you have.".to_owned()
                    }
                }
            ),
            Self::NoCredentials => format!(
                "`{command}` found no AWS credentials for profile {profile:?}.\n\
                 Sign in, e.g. `aws sso login --profile {profile}`, or check the profile with \
                 `aws configure list --profile {profile}`."
            ),
            Self::NoProfile { profile } => format!(
                "the context's AWS profile {profile:?} is not in your AWS config.\n\
                 Add it with `aws configure sso --profile {profile}`, or check that \
                 `AWS_CONFIG_FILE` points where the context expects."
            ),
            Self::NotFound { detail } => format!("`{command}` found nothing: {detail}"),
            // The dashboard tries again by itself, and says so beside this.
            Self::Throttled => match surface {
                Surface::Command => format!(
                    "AWS is throttling `{command}`, and the AWS CLI's own retries ran out.\n\
                     Wait a minute and try again, or ask for a shorter `--since`."
                ),
                Surface::Dashboard => {
                    format!("AWS is throttling `{command}`, and the AWS CLI's own retries ran out.")
                }
            },
            Self::Unreachable { detail } => format!(
                "`{command}` could not reach AWS: {detail}\n\
                 Check your network, proxy, and VPN."
            ),
            Self::TimedOut { limit } => format!(
                "`{command}` did not finish within {} and was stopped.\n\
                 If AWS is just slow, {} with `--timeout {}`.",
                format::exact_duration(*limit),
                match surface {
                    Surface::Command => "allow longer",
                    Surface::Dashboard => "start eks",
                },
                format::exact_duration(limit.saturating_mul(4)),
            ),
            Self::Other { detail } if detail.is_empty() => {
                format!("`{command}` failed without saying why.")
            }
            Self::Other { detail } => format!("`{command}` failed: {detail}"),
        }
    }
}

/// The major version in `aws --version`'s line: `aws-cli/2.17.0 Python/…`.
#[must_use]
pub fn major_version(line: &str) -> Option<u32> {
    line.trim()
        .strip_prefix("aws-cli/")?
        .split('.')
        .next()?
        .parse()
        .ok()
}

/// `(AccessDeniedException)` out of `An error occurred (AccessDeniedException)
/// when calling …`.
fn error_code(stderr: &str) -> Option<String> {
    between(stderr, "An error occurred (", ")").map(ToOwned::to_owned)
}

/// What comes after `… operation: `, the service's own sentence.
fn message_of(stderr: &str) -> Option<String> {
    let (_, message) = stderr.split_once(" operation: ")?;
    let message = message.lines().next()?.trim();
    (!message.is_empty()).then(|| message.to_owned())
}

/// The text between the first `start` and the next `end` after it.
fn between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let (_, rest) = text.split_once(start)?;
    let (found, _) = rest.split_once(end).unwrap_or((rest, ""));
    let found = found.trim();
    (!found.is_empty()).then_some(found)
}

/// The last non-blank line, where every CLI puts the sentence that matters.
fn last_line(stderr: &str) -> String {
    stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned()
}

/// A principal ARN as the name a person would use: the role and session of
/// an assumed role, or the user or role name.
///
/// `arn:aws:sts::111122223333:assumed-role/Admin/alice` reads `Admin/alice`.
/// Anything that is not an IAM or STS ARN is returned as it was.
#[must_use]
pub fn short_principal(arn: &str) -> String {
    let mut parts = arn.splitn(6, ':');
    let (Some("arn"), Some(_), Some(service), Some(_), Some(_), Some(resource)) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return arn.to_owned();
    };
    if service != "iam" && service != "sts" {
        return arn.to_owned();
    }
    match resource.split_once('/') {
        Some((_, name)) if !name.is_empty() => name.to_owned(),
        _ => arn.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn call(argv: &[&str]) -> Call {
        Call {
            argv: argv.iter().map(|word| (*word).to_owned()).collect(),
            env: Vec::new(),
            action: "logs:FilterLogEvents",
        }
    }

    fn filter_call() -> Call {
        call(&[
            "aws",
            "logs",
            "filter-log-events",
            "--log-group-name",
            "/aws/eks/prod/cluster",
            "--filter-pattern",
            "\"it's\"",
            "--output",
            "json",
        ])
    }

    // --- the line a message prints ---------------------------------------

    #[test]
    fn a_call_is_printed_as_a_line_that_pastes_into_a_shell() {
        assert_eq!(
            filter_call().line(),
            r#"aws logs filter-log-events --log-group-name /aws/eks/prod/cluster --filter-pattern '"it'\''s"'"#
        );
    }

    #[test]
    fn a_call_is_named_by_its_first_three_words() {
        assert_eq!(filter_call().short(), "aws logs filter-log-events");
    }

    // --- classify, over what the CLI really prints -----------------------

    #[test]
    fn an_access_denial_names_the_action_and_who_was_refused() {
        let stderr = "\nAn error occurred (AccessDeniedException) when calling the FilterLogEvents \
             operation: User: arn:aws:sts::111122223333:assumed-role/ReadOnly/alice is not \
             authorized to perform: logs:FilterLogEvents on resource: \
             arn:aws:logs:us-east-1:111122223333:log-group:/aws/eks/prod/cluster:log-stream: \
             because no identity-based policy allows the logs:FilterLogEvents action\n";

        assert_eq!(
            Failure::classify(stderr, "logs:FilterLogEvents"),
            Failure::Denied {
                action: "logs:FilterLogEvents".to_owned(),
                principal: Some("ReadOnly/alice".to_owned()),
            }
        );
    }

    #[test]
    fn a_denial_that_names_no_action_is_given_the_one_the_call_needed() {
        let stderr = "An error occurred (AccessDeniedException) when calling the DescribeCluster \
                      operation: explicit deny in a service control policy";

        let failure = Failure::classify(stderr, "eks:DescribeCluster");

        assert_eq!(
            failure,
            Failure::Denied {
                action: "eks:DescribeCluster".to_owned(),
                principal: None,
            }
        );
        let message = failure.explain(&filter_call(), "prod", None, Surface::Command);
        assert!(message.contains("`eks:DescribeCluster`"), "{message}");
        assert!(message.contains("you are not allowed"), "{message}");
        assert!(message.contains("Ask whoever manages IAM"), "{message}");
    }

    #[test]
    fn every_way_the_cli_says_a_session_has_gone_is_a_login_problem() {
        for stderr in [
            "Error when retrieving token from sso: Token has expired and refresh failed",
            "The SSO session associated with this profile has expired or is otherwise invalid. \
             To refresh this SSO session run aws sso login with the corresponding profile.",
            "Error loading SSO Token: Token for corp does not exist",
            "An error occurred (ExpiredTokenException) when calling the FilterLogEvents \
             operation: The security token included in the request is expired",
            "An error occurred (UnrecognizedClientException) when calling the DescribeCluster \
             operation: The security token included in the request is invalid.",
        ] {
            let failure = Failure::classify(stderr, "logs:FilterLogEvents");
            assert!(failure.fixed_by_signing_in(), "{stderr} -> {failure:?}");
        }
    }

    #[test]
    fn a_denial_is_not_mistaken_for_an_expired_session() {
        // A login cannot fix a missing permission, and offering one would send
        // somebody to fix a thing that is not broken.
        let failure = Failure::classify(
            "An error occurred (AccessDeniedException) when calling the FilterLogEvents \
             operation: User: x is not authorized to perform: logs:FilterLogEvents",
            "logs:FilterLogEvents",
        );
        assert!(!failure.fixed_by_signing_in());
    }

    #[test]
    fn no_credentials_and_an_unknown_profile_are_told_apart() {
        assert_eq!(
            Failure::classify(
                "Unable to locate credentials. You can configure credentials by running \"aws configure\".",
                "eks:DescribeCluster"
            ),
            Failure::NoCredentials
        );
        assert_eq!(
            Failure::classify(
                "The config profile (prod-admin) could not be found",
                "eks:DescribeCluster"
            ),
            Failure::NoProfile {
                profile: "prod-admin".to_owned()
            }
        );
    }

    #[test]
    fn a_missing_log_group_keeps_the_service_s_own_sentence() {
        assert_eq!(
            Failure::classify(
                "An error occurred (ResourceNotFoundException) when calling the FilterLogEvents \
                 operation: The specified log group does not exist.",
                "logs:FilterLogEvents"
            ),
            Failure::NotFound {
                detail: "The specified log group does not exist.".to_owned()
            }
        );
    }

    #[test]
    fn an_argument_the_cli_does_not_know_is_a_usage_error() {
        for stderr in [
            "usage: aws [options] <command> <subcommand>\naws: error: argument operation: Invalid choice, valid choices are:",
            "Unknown options: --max-items, 5000",
        ] {
            assert!(
                matches!(
                    Failure::classify(stderr, "logs:FilterLogEvents"),
                    Failure::Usage { .. }
                ),
                "{stderr}"
            );
        }
    }

    #[test]
    fn throttling_and_an_unreachable_endpoint_are_their_own_failures() {
        assert_eq!(
            Failure::classify(
                "An error occurred (ThrottlingException) when calling the FilterLogEvents operation (reached max retries: 2): Rate exceeded",
                "logs:FilterLogEvents"
            ),
            Failure::Throttled
        );
        assert!(matches!(
            Failure::classify(
                "Could not connect to the endpoint URL: \"https://logs.us-east-1.amazonaws.com/\"",
                "logs:FilterLogEvents"
            ),
            Failure::Unreachable { .. }
        ));
    }

    #[test]
    fn anything_else_is_reported_in_the_cli_s_own_last_line() {
        let failure = Failure::classify("\nsomething odd\n  the real reason  \n\n", "x:Y");
        assert_eq!(
            failure,
            Failure::Other {
                detail: "the real reason".to_owned()
            }
        );
        assert_eq!(
            Failure::Other {
                detail: String::new()
            }
            .explain(&filter_call(), "prod", None, Surface::Command),
            "`aws logs filter-log-events` failed without saying why."
        );
    }

    #[test]
    fn only_throttling_the_network_and_a_timeout_pass_on_their_own() {
        let passing = [
            Failure::Throttled,
            Failure::Unreachable {
                detail: String::new(),
            },
            Failure::TimedOut {
                limit: Duration::from_secs(30),
            },
        ];
        for failure in passing {
            assert!(failure.passes(), "{failure:?}");
        }
        let lasting = [
            Failure::Missing { why: not_on_path() },
            Failure::Denied {
                action: "logs:FilterLogEvents".to_owned(),
                principal: None,
            },
            Failure::Expired {
                detail: String::new(),
            },
            Failure::NotFound {
                detail: String::new(),
            },
            Failure::Other {
                detail: String::new(),
            },
        ];
        for failure in lasting {
            assert!(!failure.passes(), "{failure:?}");
        }
    }

    // --- explain ---------------------------------------------------------

    fn not_on_path() -> launch::NotStarted {
        launch::NotStarted::NotOnPath {
            program: "aws".to_owned(),
            directories: 4,
        }
    }

    #[test]
    fn a_missing_cli_says_which_version_to_install_and_where() {
        let message = Failure::Missing { why: not_on_path() }.explain(
            &filter_call(),
            "prod",
            None,
            Surface::Command,
        );
        assert!(
            message.contains(
                "`aws` is not in any of the 4 directories on the PATH eks was started with"
            ),
            "{message}"
        );
        assert!(message.contains("version 2"), "{message}");
        assert!(message.contains(INSTALL_URL), "{message}");
    }

    #[test]
    fn a_cli_the_shell_finds_and_eks_does_not_is_fixed_on_path_not_in_a_kubeconfig() {
        // eks chose to run `aws` here, not the kubeconfig, so the advice is
        // about the environment eks starts in.
        let message = Failure::Missing { why: not_on_path() }.explain(
            &filter_call(),
            "prod",
            None,
            Surface::Command,
        );
        assert!(message.contains("`type aws`"), "{message}");
        assert!(message.contains("on the PATH eks starts with"), "{message}");
        assert!(!message.contains("command:"), "{message}");
    }

    #[test]
    fn a_usage_error_from_a_version_one_cli_asks_for_version_two() {
        let failure = Failure::Usage {
            detail: "Unknown options: --max-items".to_owned(),
        };

        let message = failure.explain(
            &filter_call(),
            "prod",
            Some("aws-cli/1.29.62 Python/3.11.4 Linux/6.1 botocore/1.31.62"),
            Surface::Command,
        );

        assert!(message.contains("aws-cli/1.29.62"), "{message}");
        assert!(message.contains("version 2 or later"), "{message}");
        assert!(message.contains(INSTALL_URL), "{message}");
    }

    #[test]
    fn a_usage_error_from_a_current_cli_is_owned_as_a_bug() {
        let failure = Failure::Usage {
            detail: "Unknown options: --frobnicate".to_owned(),
        };

        let message = failure.explain(
            &filter_call(),
            "prod",
            Some("aws-cli/2.17.0 Python/3.11"),
            Surface::Command,
        );

        assert!(message.contains("--frobnicate"), "{message}");
        assert!(message.contains("bug in eks"), "{message}");
    }

    #[test]
    fn an_expired_session_says_how_to_sign_in_as_the_right_profile() {
        let message = Failure::Expired {
            detail: "Token has expired and refresh failed".to_owned(),
        }
        .explain(&filter_call(), "prod-admin", None, Surface::Command);
        assert!(
            message.contains("aws sso login --profile prod-admin"),
            "{message}"
        );
    }

    #[test]
    fn a_timeout_suggests_a_longer_one_it_can_parse() {
        let message = Failure::TimedOut {
            limit: Duration::from_secs(30),
        }
        .explain(&filter_call(), "prod", None, Surface::Command);
        assert!(message.contains("within 30s"), "{message}");
        assert!(message.contains("--timeout 2m"), "{message}");
    }

    #[test]
    fn in_the_dashboard_an_expired_session_points_at_l_not_at_re_running() {
        let failure = Failure::Expired {
            detail: "Token has expired and refresh failed".to_owned(),
        };

        let message = failure.explain(&filter_call(), "prod-admin", None, Surface::Dashboard);

        assert!(message.contains("profile \"prod-admin\""), "{message}");
        assert!(message.contains("Press L to sign in again"), "{message}");
        assert!(!message.contains("re-run"), "{message}");
    }

    #[test]
    fn in_the_dashboard_throttling_names_no_flag_the_pane_cannot_take() {
        let message = Failure::Throttled.explain(&filter_call(), "prod", None, Surface::Dashboard);

        assert!(message.contains("throttling"), "{message}");
        assert!(!message.contains("--since"), "{message}");
    }

    #[test]
    fn in_the_dashboard_a_timeout_says_to_start_eks_with_a_longer_one() {
        let message = Failure::TimedOut {
            limit: Duration::from_secs(30),
        }
        .explain(&filter_call(), "prod", None, Surface::Dashboard);

        assert!(
            message.contains("start eks with `--timeout 2m`"),
            "{message}"
        );
    }

    #[test]
    fn advice_that_names_no_flag_or_key_is_the_same_on_both_surfaces() {
        let failure = Failure::Denied {
            action: "logs:FilterLogEvents".to_owned(),
            principal: None,
        };
        assert_eq!(
            failure.explain(&filter_call(), "prod", None, Surface::Command),
            failure.explain(&filter_call(), "prod", None, Surface::Dashboard),
        );
    }

    #[test]
    fn the_cli_version_is_read_from_its_own_banner() {
        assert_eq!(
            major_version("aws-cli/2.17.0 Python/3.11.8 Darwin/23.4.0 exe/x86_64"),
            Some(2)
        );
        assert_eq!(major_version("aws-cli/1.18.69 Python/2.7.18"), Some(1));
        assert_eq!(major_version("something else"), None);
        assert_eq!(major_version(""), None);
    }

    #[test]
    fn principals_read_as_names_not_arns() {
        assert_eq!(
            short_principal("arn:aws:sts::111122223333:assumed-role/Admin/alice"),
            "Admin/alice"
        );
        assert_eq!(short_principal("arn:aws:iam::111122223333:user/bob"), "bob");
        assert_eq!(short_principal("kubernetes-admin"), "kubernetes-admin");
        assert_eq!(
            short_principal("arn:aws:s3:::bucket/key"),
            "arn:aws:s3:::bucket/key"
        );
    }

    // --- run, against real processes -------------------------------------

    #[tokio::test]
    async fn a_call_that_succeeds_hands_back_its_stdout() {
        let out = run(
            &call(&["sh", "-c", "echo '{\"ok\": true}'"]),
            Budget::default(),
        )
        .await
        .unwrap();
        assert_eq!(String::from_utf8(out).unwrap().trim(), "{\"ok\": true}");
    }

    #[tokio::test]
    async fn the_call_runs_with_the_context_s_environment_and_no_pager() {
        let mut call = call(&["sh", "-c", "printf '%s|%s' \"$AWS_PROFILE\" \"$AWS_PAGER\""]);
        call.env = vec![("AWS_PROFILE".to_owned(), "prod-admin".to_owned())];

        let out = run(&call, Budget::default()).await.unwrap();

        assert_eq!(String::from_utf8(out).unwrap(), "prod-admin|");
    }

    #[tokio::test]
    async fn a_call_that_fails_keeps_what_it_said_on_stderr() {
        let error = run(
            &call(&[
                "sh",
                "-c",
                "echo 'Unable to locate credentials.' >&2; exit 253",
            ]),
            Budget::default(),
        )
        .await
        .unwrap_err();

        let Error::Failed { code, stderr, .. } = &error else {
            panic!("{error:?}");
        };
        assert_eq!(*code, Some(253));
        assert_eq!(
            Failure::of(&error, "eks:DescribeCluster"),
            Failure::NoCredentials
        );
        assert!(stderr.contains("Unable to locate credentials"));
    }

    #[tokio::test]
    async fn a_program_that_is_not_installed_is_reported_as_missing() {
        let error = run(
            &call(&["eks-test-no-such-aws-cli", "logs"]),
            Budget::default(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                Failure::of(&error, "x:Y"),
                Failure::Missing {
                    why: launch::NotStarted::NotOnPath { .. }
                }
            ),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn a_call_that_outlives_the_budget_is_stopped() {
        let started = std::time::Instant::now();

        let error = run(
            &call(&["sh", "-c", "sleep 30"]),
            Budget::of(Duration::from_millis(200)),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, Error::TimedOut { .. }), "{error:?}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn the_cli_never_reads_the_terminal() {
        // `/dev/null`: a profile that asks for an MFA code gets an end of file
        // at once rather than waiting on a prompt nobody can see.
        let out = run(&call(&["sh", "-c", "cat; echo done"]), Budget::default())
            .await
            .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "done\n");
    }

    /// Write an executable script.
    ///
    /// Through a child `sh` rather than `std::fs::write`: a file this process
    /// holds open for writing is inherited by any child another test thread
    /// forks at that moment, and executing it then fails with `ETXTBSY`.
    #[cfg(unix)]
    fn install(path: &std::path::Path, script: &str) {
        use std::io::Write as _;
        let mut child = std::process::Command::new("sh")
            .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
            .arg(path)
            .stdin(std::process::Stdio::piped())
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

    #[cfg(unix)]
    #[tokio::test]
    async fn the_version_is_read_from_whichever_stream_the_cli_printed_it_on() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("aws-v1");
        install(
            &old,
            "#!/bin/sh\necho 'aws-cli/1.18.69 Python/2.7.18' >&2\n",
        );
        let new = dir.path().join("aws-v2");
        install(&new, "#!/bin/sh\necho 'aws-cli/2.17.0 Python/3.11.8'\n");

        let found = version(old.to_str().unwrap(), Budget::default()).await;
        assert_eq!(found.as_deref(), Some("aws-cli/1.18.69 Python/2.7.18"));
        let found = version(new.to_str().unwrap(), Budget::default()).await;
        assert_eq!(found.as_deref(), Some("aws-cli/2.17.0 Python/3.11.8"));
        assert_eq!(
            version("eks-test-no-such-aws-cli", Budget::default()).await,
            None
        );
    }

    #[tokio::test]
    async fn an_empty_call_is_worded_rather_than_panicked_on() {
        let error = run(&call(&[]), Budget::default()).await.unwrap_err();
        assert!(error.to_string().contains("no command to run"));
    }
}
