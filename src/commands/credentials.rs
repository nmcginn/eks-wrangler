//! Connecting to a cluster, logging in first if its AWS session has gone.
//!
//! This is the seam between [`crate::aws`], which knows how to tell a live IAM
//! Identity Center session from a dead one, and [`crate::k8s::client`], which
//! knows how to build a client and how to explain it when that fails. Every
//! command that talks to a cluster goes through [`connect`] rather than through
//! `k8s::connect` directly, so the offer to log in is made in one place and
//! worded once.
//!
//! Two chances are taken at it, and they answer different questions.
//!
//! The **pre-flight** happens before the credential helper runs at all. It is
//! two file reads — `~/.aws/config` and the AWS CLI's token cache — so it costs
//! no network, no subprocess, and nothing that could stall a first paint, and
//! it catches the case that is nearly all of them: a session that expired
//! overnight. Catching it here means the user answers one question instead of
//! waiting out a doomed request and reading an error afterwards.
//!
//! The **retry** happens after the cluster has refused anyway. The cache is not
//! the whole truth — a token can be revoked centrally while its `expiresAt`
//! still reads hours away, a session can die in the seconds between the check
//! and the request, and a `~/.aws/config` this tool could not follow looks like
//! "no Identity Center here" until the API server says otherwise. It is taken
//! at most once, so a cluster that refuses a freshly minted token is an error
//! rather than a loop. [`connect`] takes it itself, right after `client::build`
//! refuses; the dashboard's `L` key takes the same retry later, through
//! [`retry_login`], once a background fetch has refused instead.
//!
//! The decisions are all next door in [`crate::aws::decide`] and
//! [`crate::aws::after_refusal`], as pure functions. What lives here is the I/O
//! they cannot do: reading the files, asking the question, running the login.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{Result, bail};
use k8s_openapi::jiff::Timestamp;
use kube::{Client, Config};

use crate::aws::{self, Action, LoginMode};
use crate::cluster::ClusterView;
use crate::k8s::client;
use crate::k8s::page::Budget;

/// Build a client for one cluster, offering a login first if its AWS profile
/// needs one.
///
/// A drop-in for [`client::connect`] that takes the extra `login` flag. With
/// [`LoginMode::Never`] it behaves exactly as that function does, down to the
/// error text — which is the promise `--login never` makes.
pub async fn connect(
    paths: &[PathBuf],
    cluster: &ClusterView,
    budget: Budget,
    login: LoginMode,
) -> Result<Client> {
    let config = client::resolve(paths, cluster).await?;
    let label = cluster.label();

    // Nothing below this line runs a subprocess or opens a socket unless the
    // user has said yes to one, so a `--login never` command reaches
    // `client::build` having done no more work than it used to.
    let context = Context::of(&config, login);
    let before = context.act(&context.before(&label))?;

    match client::build(config.clone(), &label, budget).await {
        Ok(built) => Ok(built),
        // One retry, and only when the pre-flight neither logged in nor put the
        // question to anybody. A cluster that refuses a token minted seconds
        // ago is telling us something a second login will not change — and a
        // user who has just answered "no" to this exact question does not want
        // to be asked it again a moment later, which is the whole reason
        // `Declined` is a separate outcome from `NothingToDo`.
        Err(error) if worth_retrying(before, &error) => {
            if context.act(&context.after_refusal(&label))? == Outcome::LoggedIn {
                Ok(client::build(config, &label, budget).await?)
            } else {
                Err(error.into())
            }
        }
        Err(error) => Err(error.into()),
    }
}

/// What [`Context::act`] did about an [`Action`].
///
/// Three outcomes rather than a `bool`, because the two that did not log
/// anybody in mean opposite things to the retry: nothing was asked, so asking
/// now is new information — or the user has already said no, and asking the
/// same question twice in one command is how a tool teaches people to reach
/// for `--login never`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// There was nothing to offer, or nobody to offer it to.
    NothingToDo,
    /// A login ran and succeeded.
    LoggedIn,
    /// The offer was made and turned down.
    Declined,
}

/// Run only the pre-flight: offer a login if this cluster's AWS session has
/// gone, and return without connecting.
///
/// For the dashboard, which cannot do this the way a one-shot command does. Its
/// fetches happen on background threads that have no business owning the
/// terminal, so the question is put *here* — before `ui::run` opens the
/// alternate screen, while stdin and stderr are still ordinary — and every
/// fetcher after it is built with [`LoginMode::Never`]. A session that dies
/// later is [`retry_login`]'s problem, not this function's: the cache this
/// reads is exactly what a later refusal proves stale, so this is never the
/// right check for `L` to repeat.
pub async fn preflight(paths: &[PathBuf], cluster: &ClusterView, login: LoginMode) -> Result<()> {
    let config = client::resolve(paths, cluster).await?;
    let context = Context::of(&config, login);
    context.act(&context.before(&cluster.label()))?;
    Ok(())
}

/// What the `L` key runs: the **retry**'s question, not the pre-flight's.
///
/// `L` only appears once a fetch has already been refused for a credentials
/// reason (`App::credentials_lost`), so calling [`preflight`] here would ask
/// the wrong question: its `before` reads the token cache and proceeds
/// whenever the cache still calls the session valid — which is nearly always
/// true right after a dashboard's own refusal, since a token revoked
/// centrally still reads as live in the cache until something tries to use
/// it, and something already did. Consulting the cache a second time would
/// leave `L` doing nothing, silently, in exactly the case it exists for.
///
/// This runs `Context::after_refusal` instead — the same question
/// [`connect`]'s own retry asks after a one-shot command's request is
/// refused — which does not look at the cache at all: it logs in whenever
/// the profile has an Identity Center session to refresh, because the
/// refusal that put `L` on screen is already the evidence the cache's own
/// answer was wrong.
///
/// Always [`LoginMode::Always`], not a flag the caller passes in: pressing
/// `L` *is* the yes, and asking a second question over a dashboard that has
/// just given the screen back would be asking it twice. Fixed here rather
/// than left to `main.rs` to remember, so `Context::act` never has a question
/// to ask and `Outcome::Declined` never happens.
///
/// `Outcome::NothingToDo` is turned into an error here rather than being
/// swallowed the way [`preflight`] swallows it. A pre-flight finding nothing
/// to do is the ordinary case — most sessions are fine — so proceeding
/// quietly is correct there. `L` is different: it is only ever pressed
/// because `is_credentials` already classified a real refusal, so
/// `NothingToDo` here means the profile behind it has no Identity Center
/// session for `aws sso login` to refresh at all — a static-key or
/// instance-role profile whose credentials went bad some other way — and
/// that is worth a sentence on screen instead of a silent flicker with
/// nothing for the user to act on.
pub async fn retry_login(paths: &[PathBuf], cluster: &ClusterView) -> Result<()> {
    let config = client::resolve(paths, cluster).await?;
    let context = Context::of(&config, LoginMode::Always);
    match context.act(&context.after_refusal(&cluster.label()))? {
        Outcome::LoggedIn => Ok(()),
        Outcome::NothingToDo => bail!(
            "profile {:?} has no IAM Identity Center session to refresh; \
             its credentials need fixing some other way.",
            context.profile
        ),
        // `LoginMode::Always` never produces `Action::Ask`, so `Context::act`
        // never has a question to decline. Worded rather than `unreachable!`,
        // as everything in this crate is: a defensive branch should say what
        // is wrong if it is ever proven wrong, not stop the terminal cold.
        Outcome::Declined => bail!("logging in to AWS did not run"),
    }
}

/// Whether a failed build is worth offering a login for a second time.
///
/// The rule the retry turns on, pulled out of [`connect`] so its whole table is
/// a test rather than one `if` in the middle of an I/O path.
fn worth_retrying(before: Outcome, error: &client::Error) -> bool {
    before == Outcome::NothingToDo && is_credentials(error)
}

/// Whether a failed build is the kind a fresh login could put right.
///
/// Only [`client::Failure::Credentials`]. A `403` means the cluster knows
/// exactly who you are and will not let you in, which logging in again cannot
/// change; an unreachable endpoint is about a VPC; and a stalled helper has not
/// failed yet. Offering a login for any of those would be the
/// wrong-but-plausible suggestion `k8s::client::explain` is careful to avoid.
fn is_credentials(error: &client::Error) -> bool {
    matches!(
        error,
        client::Error::Cluster {
            failure: client::Failure::Credentials,
            ..
        }
    )
}

/// The same question, asked of an error that has already been boxed up by
/// `anyhow` on its way out of a command.
///
/// The dashboard's fetches run on background threads and come back as finished
/// [`anyhow::Error`]s, long past the point where the typed failure was in
/// hand — so this recovers it rather than re-reading the sentence. A failure
/// with no [`client::Error`] under it is not a credential problem: everything
/// that classifies one goes through [`client::Error::explained`] or
/// [`client::build`].
#[must_use]
pub fn refused_credentials(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<client::Error>()
        .is_some_and(is_credentials)
}

/// Everything the two decisions need, read once.
///
/// Gathered into a struct because the retry asks the same questions the
/// pre-flight did, and re-reading `~/.aws/config` between two attempts a second
/// apart would be work for nothing — and could give two different answers to
/// one command.
struct Context {
    mode: LoginMode,
    profile: String,
    sso: Option<aws::config::Sso>,
    session: aws::sso::Session,
    /// The `exec` block's own environment, passed to the login so it reads the
    /// same `~/.aws/config` the token fetch will.
    env: Vec<(String, String)>,
}

impl Context {
    /// Read what the local AWS configuration says about this context's profile.
    fn of(config: &Config, mode: LoginMode) -> Self {
        let env: Vec<(String, String)> = client::exec_env(&config.auth_info)
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect();

        // The environment the credential helper will actually run under: the
        // `exec` block's own entries layered over this process's, which is the
        // order `kube` hands them to the child. Everything below reads through
        // it, so the file we decide against is the file the token comes from.
        let layered = |name: &str| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
                .or_else(|| std::env::var(name).ok())
        };

        let profile = aws::profile::profile_for(&config.auth_info, &layered);

        // `--login never` will not act on any of this, so it is not read.
        // Skipping it keeps the flag's promise literal: the same files touched,
        // in the same order, as before the flag existed.
        if matches!(mode, LoginMode::Never) {
            return Self {
                mode,
                profile,
                sso: None,
                session: aws::sso::Session::NotSso,
                env,
            };
        }

        let sso = aws::config::AwsConfig::path_from(&layered)
            .map(|path| aws::config::AwsConfig::load_from(&path))
            .and_then(|config| config.sso_for(&profile));

        let session = match (&sso, aws::sso::cache_dir()) {
            (Some(sso), Some(dir)) => {
                aws::sso::classify(&aws::sso::read_cache(&dir), sso, Timestamp::now())
            }
            // No Identity Center behind this profile, or no home directory to
            // find a cache in. Either way there is nothing here to refresh.
            _ => aws::sso::Session::NotSso,
        };

        tracing::debug!(
            profile,
            ?session,
            "checked the AWS session for this context"
        );

        Self {
            mode,
            profile,
            sso,
            session,
            env,
        }
    }

    fn before(&self, label: &str) -> Action {
        aws::decide(
            &self.session,
            self.mode,
            interactive(),
            label,
            &self.profile,
            Timestamp::now(),
        )
    }

    fn after_refusal(&self, label: &str) -> Action {
        aws::after_refusal(
            self.sso.as_ref(),
            self.mode,
            interactive(),
            label,
            &self.profile,
        )
    }

    /// Carry out an action, returning whether a login actually ran.
    ///
    /// Everything user-facing here goes to **stderr**, never stdout: `eks nodes
    /// | column -t` has to keep printing a table and nothing else, and a
    /// question about logging in is not part of the listing.
    fn act(&self, action: &Action) -> Result<Outcome> {
        let (argv, prompt, ask) = match action {
            Action::Proceed => return Ok(Outcome::NothingToDo),
            Action::Ask { question, argv } => (argv, question, true),
            Action::Run { announcement, argv } => (argv, announcement, false),
        };

        let mut stderr = std::io::stderr();
        write!(stderr, "{prompt}")?;
        if !ask {
            writeln!(stderr)?;
        }
        stderr.flush()?;

        if ask {
            let mut answer = String::new();
            std::io::stdin().lock().read_line(&mut answer)?;
            if !accepted(&answer) {
                writeln!(stderr, "{}", declined(argv))?;
                return Ok(Outcome::Declined);
            }
        }

        aws::login::run(argv, &self.env)?;
        Ok(Outcome::LoggedIn)
    }
}

/// Whether there is a human here to answer a question.
///
/// Both ends have to be a terminal. Stderr is where the question is written and
/// stdin is where the answer comes from, and a command with either one
/// redirected is one nobody is sitting in front of.
fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Whether an answer to the login prompt was a yes.
///
/// `[Y/n]` means the empty answer — someone pressing Enter — is a yes, which is
/// the whole reason the prompt is written that way: the question is only ever
/// asked when the alternative is a command that is about to fail.
#[must_use]
pub fn accepted(answer: &str) -> bool {
    let answer = answer.trim();
    answer.is_empty() || answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")
}

/// What to say when the user declines the login.
///
/// It names the command anyway. Saying "fine" and then failing a moment later
/// with a message about credentials would make the question look like it had
/// been ignored.
#[must_use]
pub fn declined(argv: &[String]) -> String {
    format!(
        "Not logging in. Run `{}` yourself when you want to.",
        aws::login::line(argv)
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn pressing_enter_at_the_prompt_is_a_yes() {
        // `[Y/n]`, and the question is only ever asked when the alternative is
        // a command that is about to fail.
        assert!(accepted(""));
        assert!(accepted("\n"));
        assert!(accepted("  \n"));
    }

    #[test]
    fn the_obvious_spellings_of_yes_are_accepted() {
        for answer in ["y", "Y", "yes", "YES", "Yes", " y \n"] {
            assert!(accepted(answer), "{answer:?}");
        }
    }

    #[test]
    fn anything_else_is_a_no() {
        // Deliberately strict: an answer we do not recognise must not open a
        // browser. `n`, `no`, and a typo all decline.
        for answer in ["n", "N", "no", "NO", "later", "q", "why"] {
            assert!(!accepted(answer), "{answer:?}");
        }
    }

    #[test]
    fn declining_still_says_what_to_run() {
        let note = declined(&aws::login::command("prod"));

        assert!(note.contains("aws sso login --profile prod"), "{note}");
        assert!(note.contains("Not logging in"), "{note}");
    }

    #[test]
    fn only_a_credential_refusal_is_worth_a_second_attempt() {
        // A `403` is the cluster saying it knows exactly who you are, and a
        // second login cannot change its mind.
        for failure in [
            client::Failure::Forbidden,
            client::Failure::Unreachable,
            client::Failure::HelperMissing,
            client::Failure::PageExpired,
            client::Failure::Other,
        ] {
            assert!(
                !is_credentials(&client::Error::Cluster {
                    message: "anything".to_owned(),
                    failure,
                }),
                "{failure:?}"
            );
        }

        assert!(is_credentials(&client::Error::Cluster {
            message: "anything".to_owned(),
            failure: client::Failure::Credentials,
        }));
    }

    fn refused() -> client::Error {
        client::Error::Cluster {
            message: "prod rejected your credentials".to_owned(),
            failure: client::Failure::Credentials,
        }
    }

    #[test]
    fn a_refusal_the_pre_flight_said_nothing_about_is_worth_a_second_offer() {
        // The case the retry exists for: the cache said the session was fine,
        // and the cluster disagreed — a token revoked centrally still reads as
        // live locally until something tries to use it.
        assert!(worth_retrying(Outcome::NothingToDo, &refused()));
    }

    #[test]
    fn a_user_who_just_declined_is_not_asked_the_same_question_again() {
        // The pre-flight offered a login, they said no, and the command failed
        // for exactly the reason they were warned about. Putting the identical
        // question a second time in one command is how a tool teaches people
        // to reach for `--login never`.
        assert!(!worth_retrying(Outcome::Declined, &refused()));
    }

    #[test]
    fn a_cluster_that_refuses_a_freshly_minted_token_is_an_error_not_a_loop() {
        assert!(!worth_retrying(Outcome::LoggedIn, &refused()));
    }

    #[test]
    fn a_failure_no_login_could_fix_is_never_retried_however_it_started() {
        for before in [Outcome::NothingToDo, Outcome::Declined, Outcome::LoggedIn] {
            assert!(
                !worth_retrying(
                    before,
                    &client::Error::Cluster {
                        message: "could not reach prod".to_owned(),
                        failure: client::Failure::Unreachable,
                    }
                ),
                "{before:?}"
            );
        }
    }

    /// A profile whose Identity Center session the cache still calls
    /// valid — the exact shape of the state `L` is pressed into, since a
    /// token revoked centrally still reads as live in the cache until
    /// something tries to use it, and a real fetch already just did.
    fn context_with_a_still_valid_looking_cache() -> Context {
        Context {
            mode: LoginMode::Always,
            profile: "prod".to_owned(),
            sso: Some(aws::config::Sso {
                start_url: "https://acme.awsapps.com/start".to_owned(),
                session: Some("corp".to_owned()),
            }),
            session: aws::sso::Session::Valid {
                expires_at: "2099-01-01T00:00:00Z".parse().unwrap(),
            },
            env: Vec::new(),
        }
    }

    #[test]
    fn the_pre_flight_question_would_leave_l_doing_nothing_over_a_stale_cache() {
        // This is the bug `retry_login` exists to avoid: re-asking `before`
        // (what `preflight` runs) after a live refusal reads the same cache
        // that refusal just proved wrong, and finds nothing to do.
        assert_eq!(
            context_with_a_still_valid_looking_cache().before("prod (us-east-1)"),
            Action::Proceed
        );
    }

    #[test]
    fn l_logs_in_even_though_the_cache_still_calls_the_session_valid() {
        // `after_refusal` — what `retry_login` runs instead — does not
        // consult `self.session` at all, so the same context that left
        // `before` with nothing to do here runs the login unconditionally.
        assert!(matches!(
            context_with_a_still_valid_looking_cache().after_refusal("prod (us-east-1)"),
            Action::Run { .. }
        ));
    }

    #[test]
    fn a_profile_with_no_identity_centre_session_leaves_after_refusal_nothing_to_do() {
        // The state `retry_login` turns into an error rather than a silent
        // `Ok`: a profile behind static keys or an instance role still gets
        // classified `Failure::Credentials` when those credentials go bad,
        // and there is no Identity Center session for `aws sso login` to
        // refresh.
        let context = Context {
            mode: LoginMode::Always,
            profile: "prod".to_owned(),
            sso: None,
            session: aws::sso::Session::NotSso,
            env: Vec::new(),
        };

        assert_eq!(
            context
                .act(&context.after_refusal("prod (us-east-1)"))
                .unwrap(),
            Outcome::NothingToDo
        );
    }

    #[test]
    fn a_helper_that_never_answered_is_not_a_refusal() {
        // Nothing has been asked of the cluster yet, so there is no refusal to
        // respond to — and the helper may be sitting on a login prompt of its
        // own, which starting a second one would not help.
        assert!(!is_credentials(&client::Error::HelperStalled(
            "took too long".to_owned()
        )));
    }
}
