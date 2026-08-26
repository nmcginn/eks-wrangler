//! Everything this tool knows about AWS, which is deliberately not much.
//!
//! `eks` has no AWS SDK and holds no AWS credential. Authentication is the
//! kubeconfig `exec` block's job — it runs `aws eks get-token`, and `kube`
//! resolves it (see [`crate::k8s::client`]). What this module adds is the one
//! question that block cannot answer for itself: *will it work?* An IAM
//! Identity Center session that expired overnight makes that helper fail, and
//! the only thing standing between the user and a working cluster is an `aws
//! sso login` they have to go and type somewhere else.
//!
//! So the shape here is narrow on purpose:
//!
//! - [`profile`] works out which AWS profile the context authenticates as.
//! - [`config`] reads the few `~/.aws/config` keys that say whether that
//!   profile uses Identity Center, and which session.
//! - [`sso`] reads the AWS CLI's own token cache to see whether that session is
//!   still alive.
//! - [`login`] runs `aws sso login` when it is not.
//!
//! The first three are file reads and pure functions — no network, no
//! subprocess, nothing that can stall a first paint — which is what lets the
//! check happen *before* connecting rather than in reaction to a `401` the user
//! already waited for. [`decide`] is the whole policy, as one pure function
//! over the session, the flag, and whether there is a human at the terminal, so
//! "never open a browser at somebody who is piping this into a file" is a test
//! rather than a hope.

pub mod config;
pub mod login;
pub mod profile;
pub mod sso;

use k8s_openapi::jiff::Timestamp;

use crate::format;
use sso::Session;

/// When `eks` may log you in to IAM Identity Center on its own.
///
/// A `clap::ValueEnum` on the domain type for the reason `--color` and `--sort`
/// are (decision 28): a value this does not take is rejected with the ones it
/// does listed, before anything connects. The spellings match `--color`'s,
/// because a second vocabulary for the same three-way choice is one more thing
/// to remember.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum LoginMode {
    /// Offer, and wait for an answer, when there is a terminal to ask at. The
    /// default: a browser is never opened without a yes.
    #[default]
    Auto,
    /// Log in without asking. For someone who has decided that a stale session
    /// should simply be refreshed.
    Always,
    /// Never log in. Exactly the behaviour this tool had before the flag
    /// existed: a stale session is a message telling you what to run.
    Never,
}

/// What to do about a profile's Identity Center session before connecting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Nothing to do. The session is good, the profile does not use Identity
    /// Center at all, or nobody asked us to log anyone in.
    Proceed,
    /// Put the question to the user, and log in if they say yes.
    Ask { question: String, argv: Vec<String> },
    /// Log in without asking, saying so on the way past.
    Run {
        announcement: String,
        argv: Vec<String>,
    },
}

impl Action {
    /// The command this action would run, if it would run one.
    #[must_use]
    pub fn argv(&self) -> Option<&[String]> {
        match self {
            Self::Proceed => None,
            Self::Ask { argv, .. } | Self::Run { argv, .. } => Some(argv),
        }
    }
}

/// Decide what to do before connecting, given what the token cache says.
///
/// `interactive` means there is a human to answer: both stdin and stderr are
/// terminals. It is passed in rather than asked here so the whole policy stays
/// a pure function — every row of the table below is a test.
///
/// | session | `never` | `auto`, terminal | `auto`, piped | `always` |
/// |---|---|---|---|---|
/// | valid | proceed | proceed | proceed | proceed |
/// | expired | proceed | ask | proceed | run |
/// | missing | proceed | ask | proceed | run |
/// | not Identity Center | proceed | proceed | proceed | proceed |
///
/// The two `proceed`s in the `auto`, piped column are the important ones. A
/// listing being redirected into a file has nobody watching to answer a
/// question, and a tool that opened a browser there — or worse, sat waiting for
/// a keystroke nobody would type — would be a thing people work around rather
/// than use. Proceeding means they get the message this tool always gave, which
/// tells them what to run.
#[must_use]
pub fn decide(
    session: &Session,
    mode: LoginMode,
    interactive: bool,
    cluster: &str,
    profile: &str,
    now: Timestamp,
) -> Action {
    let reason = match session {
        // Nothing is wrong, or nothing here could put it right.
        Session::Valid { .. } | Session::NotSso => return Action::Proceed,
        Session::Expired { expires_at } => expiry_reason(cluster, profile, *expires_at, now),
        Session::Missing => format!(
            "{cluster} needs a login: profile {profile:?} has no IAM Identity Center session cached."
        ),
    };

    offer(&reason, mode, interactive, profile)
}

/// Decide what to do after the cluster has refused the credentials anyway.
///
/// The backstop behind [`decide`], and it exists because the token cache is not
/// the whole truth. A session can die between the check and the request; a
/// cached token can be revoked centrally while its `expiresAt` still reads
/// hours away; and a profile whose `~/.aws/config` this tool could not follow
/// looks like "no Identity Center here" right up until the API server says
/// otherwise. In each case the offer is worth making a second time.
///
/// `sso` is what [`config::AwsConfig::sso_for`] found: `None` means the profile
/// has no Identity Center login to refresh, and the refusal is about something
/// else — a wrong role, an access entry nobody has granted — where a login
/// would not help and offering one would be a wrong-but-plausible suggestion of
/// exactly the kind [`crate::k8s::client::explain`] avoids.
#[must_use]
pub fn after_refusal(
    sso: Option<&config::Sso>,
    mode: LoginMode,
    interactive: bool,
    cluster: &str,
    profile: &str,
) -> Action {
    if sso.is_none() {
        return Action::Proceed;
    }

    offer(
        &format!("{cluster} refused the credentials from profile {profile:?}."),
        mode,
        interactive,
        profile,
    )
}

/// Turn a reason into the ask or the announcement, according to the flag.
fn offer(reason: &str, mode: LoginMode, interactive: bool, profile: &str) -> Action {
    let argv = login::command(profile);
    let line = login::line(&argv);

    match mode {
        LoginMode::Always => Action::Run {
            announcement: format!("{reason}\nLogging in: `{line}`"),
            argv,
        },
        LoginMode::Auto if interactive => Action::Ask {
            question: format!("{reason}\nLog in now with `{line}`? [Y/n] "),
            argv,
        },
        // Either nobody is watching (`auto` with no terminal to ask at) or
        // nobody asked us to (`never`). Both proceed, and for one reason: the
        // credential message this tool has always printed already says what to
        // run, and it is about to be printed.
        LoginMode::Auto | LoginMode::Never => Action::Proceed,
    }
}

/// How to describe a session that has run out, or is about to.
///
/// Both readings come out of one `expires_at` because [`sso::classify`] folds
/// the last minute of a token's life into `Expired` — it is not worth starting
/// a paged listing with — and a message saying a session "signed out 0s ago"
/// when it has forty seconds left would read as a bug rather than as caution.
fn expiry_reason(cluster: &str, profile: &str, expires_at: Timestamp, now: Timestamp) -> String {
    if expires_at > now {
        format!(
            "{cluster} needs a fresh login: profile {profile:?} signs out of IAM Identity Center in {}.",
            format::human_duration(expires_at.duration_since(now))
        )
    } else {
        format!(
            "{cluster} needs a fresh login: profile {profile:?} signed out of IAM Identity Center {} ago.",
            format::human_duration(now.duration_since(expires_at))
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    const CLUSTER: &str = "prod (us-east-1)";

    fn now() -> Timestamp {
        "2026-08-26T12:00:00Z".parse().unwrap()
    }

    /// A session that ran out overnight, which is what nearly every one of
    /// these is.
    fn expired() -> Session {
        Session::Expired {
            expires_at: "2026-08-26T04:00:00Z".parse().unwrap(),
        }
    }

    fn valid() -> Session {
        Session::Valid {
            expires_at: "2026-08-26T20:00:00Z".parse().unwrap(),
        }
    }

    fn decide_with(session: &Session, mode: LoginMode, interactive: bool) -> Action {
        decide(session, mode, interactive, CLUSTER, "prod", now())
    }

    fn a_session() -> config::Sso {
        config::Sso {
            start_url: "https://acme.awsapps.com/start".to_owned(),
            session: Some("corp".to_owned()),
        }
    }

    #[test]
    fn a_live_session_is_never_logged_in_again() {
        // `always` means "do not ask", not "log in every time".
        for mode in [LoginMode::Auto, LoginMode::Always, LoginMode::Never] {
            assert_eq!(
                decide_with(&valid(), mode, true),
                Action::Proceed,
                "{mode:?}"
            );
        }
    }

    #[test]
    fn a_profile_that_does_not_use_identity_centre_is_left_alone() {
        // Static keys, a `credential_process`, an instance role: there is
        // nothing to log in to, and today's advice stays the right advice.
        for mode in [LoginMode::Auto, LoginMode::Always, LoginMode::Never] {
            assert_eq!(
                decide_with(&Session::NotSso, mode, true),
                Action::Proceed,
                "{mode:?}"
            );
        }
    }

    #[test]
    fn an_expired_session_at_a_terminal_is_offered_rather_than_taken() {
        let Action::Ask { question, argv } = decide_with(&expired(), LoginMode::Auto, true) else {
            panic!("an expired session at a terminal should be asked about");
        };

        assert!(question.contains(CLUSTER), "{question}");
        assert!(question.contains("signed out"), "{question}");
        // `format::human_duration`, the same rounding every age in this tool
        // uses, rather than a second vocabulary for one message.
        assert!(question.contains("8h ago"), "{question}");
        assert!(
            question.contains("aws sso login --profile prod"),
            "{question}"
        );
        assert_eq!(argv, login::command("prod"));
    }

    #[test]
    fn an_expired_session_on_a_pipe_is_never_logged_in_for_you() {
        // The row that matters most: a listing being redirected into a file has
        // nobody to answer, and a browser opening there would be a thing people
        // work around rather than use.
        assert_eq!(
            decide_with(&expired(), LoginMode::Auto, false),
            Action::Proceed
        );
        assert_eq!(
            decide_with(&Session::Missing, LoginMode::Auto, false),
            Action::Proceed
        );
    }

    #[test]
    fn login_never_restores_the_behaviour_that_predates_the_flag() {
        for session in [expired(), Session::Missing, valid(), Session::NotSso] {
            assert_eq!(
                decide_with(&session, LoginMode::Never, true),
                Action::Proceed,
                "{session:?}"
            );
        }
    }

    #[test]
    fn login_always_runs_without_asking_and_says_so() {
        let Action::Run { announcement, argv } = decide_with(&expired(), LoginMode::Always, true)
        else {
            panic!("`--login always` should log in");
        };

        assert!(announcement.contains("Logging in"), "{announcement}");
        assert!(
            !announcement.contains('?'),
            "an announcement is not a question: {announcement}"
        );
        assert_eq!(argv, login::command("prod"));
    }

    #[test]
    fn login_always_does_not_need_a_terminal() {
        // Someone who typed the flag has decided; there is nothing to ask.
        assert!(matches!(
            decide_with(&expired(), LoginMode::Always, false),
            Action::Run { .. }
        ));
    }

    #[test]
    fn a_session_with_no_cached_token_says_so_rather_than_claiming_it_expired() {
        let Action::Ask { question, .. } = decide_with(&Session::Missing, LoginMode::Auto, true)
        else {
            panic!("a missing session at a terminal should be asked about");
        };

        assert!(
            question.contains("no IAM Identity Center session cached"),
            "{question}"
        );
        assert!(!question.contains("signed out"), "{question}");
    }

    #[test]
    fn a_session_inside_its_last_minute_reads_as_about_to_go_not_as_gone() {
        // `sso::classify` calls this expired deliberately; the wording has to
        // stay honest about a token that has not actually run out yet.
        let session = Session::Expired {
            expires_at: "2026-08-26T12:00:40Z".parse().unwrap(),
        };

        let Action::Ask { question, .. } = decide_with(&session, LoginMode::Auto, true) else {
            panic!("expected a question");
        };

        assert!(question.contains("signs out"), "{question}");
        assert!(question.contains("40s"), "{question}");
        assert!(!question.contains("ago"), "{question}");
    }

    #[test]
    fn a_refusal_from_a_cluster_offers_a_login_the_cache_said_was_unnecessary() {
        // A token revoked centrally still reads as live in the cache, so the
        // cluster's `401` is the only evidence there is.
        let Action::Ask { question, argv } =
            after_refusal(Some(&a_session()), LoginMode::Auto, true, CLUSTER, "prod")
        else {
            panic!("a refusal at a terminal should be asked about");
        };

        assert!(question.contains("refused the credentials"), "{question}");
        assert_eq!(argv, login::command("prod"));
    }

    #[test]
    fn a_refusal_for_a_profile_with_no_identity_centre_offers_nothing() {
        // Logging in would not help, and suggesting it confidently would send
        // somebody off to fix a thing that is not broken.
        assert_eq!(
            after_refusal(None, LoginMode::Always, true, CLUSTER, "prod"),
            Action::Proceed
        );
    }

    #[test]
    fn a_refusal_honours_the_flag_the_same_way_the_pre_flight_does() {
        assert_eq!(
            after_refusal(Some(&a_session()), LoginMode::Never, true, CLUSTER, "prod"),
            Action::Proceed
        );
        assert_eq!(
            after_refusal(Some(&a_session()), LoginMode::Auto, false, CLUSTER, "prod"),
            Action::Proceed
        );
    }

    #[test]
    fn every_action_that_would_run_something_says_what() {
        assert_eq!(Action::Proceed.argv(), None);
        assert_eq!(
            decide_with(&expired(), LoginMode::Auto, true).argv(),
            Some(login::command("prod").as_slice())
        );
    }
}
