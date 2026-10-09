//! Running the AWS CLI for the commands that read CloudWatch:
//! `eks control-plane-logs`, and `eks logs` once a pod is gone.
//!
//! Both run calls under `--timeout`, offer one login when the credentials
//! turn out to have expired, explain any other failure, and page through
//! `filter-log-events` with a count on the progress line. That machinery
//! lives here so the two commands cannot drift apart in how they treat a
//! refusal; what they read and print stays in their own modules.

use std::io;

use anyhow::{Context as _, Result};

use crate::aws::cli::{self, Call, Failure, Surface};
use crate::aws::eks::Target;
use crate::aws::logs::{self, Event};
use crate::commands::credentials::AwsLogin;
use crate::k8s::page::{self, Budget};
use crate::progress::Progress;

/// The AWS CLI, as a command runs it: under the budget, and with one login
/// offered when the credentials turn out to have expired.
///
/// Split in two so the offer is made with nothing else on the screen:
/// [`Aws::attempt`] runs calls and hands back a [`Refusal`] untouched, and
/// [`Aws::recover`], called once the caller has taken its progress line
/// down, offers the login or explains. A loop around the pair ends, because
/// the login is offered at most once per command.
pub(crate) struct Aws<'a> {
    pub(crate) target: &'a Target,
    pub(crate) login: AwsLogin,
    pub(crate) budget: Budget,
    /// Whose advice a failure gets: a flag's, or a key's.
    pub(crate) surface: Surface,
}

/// A call that failed, not yet explained.
pub(crate) struct Refusal {
    call: Call,
    error: cli::Error,
}

/// A failed call, explained: the sentence, and what it was, so a caller can
/// tell a failure that passes from one that does not, or put its own
/// sentence on one it knows more about.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(crate) struct Explained {
    pub(crate) message: String,
    pub(crate) failure: Failure,
}

impl Aws<'_> {
    /// One call, with the login offer taken if it is needed. For a call with
    /// no progress line to take down.
    pub(crate) async fn run(&mut self, call: &Call) -> Result<Vec<u8>> {
        loop {
            match self.attempt(&[call]).await {
                Ok(mut replies) => return Ok(replies.pop().unwrap_or_default()),
                Err(refusal) => self.recover(refusal).await?,
            }
        }
    }

    /// Several calls at once, replies in order. Any one failing fails all.
    pub(crate) async fn attempt(&self, calls: &[&Call]) -> Result<Vec<Vec<u8>>, Refusal> {
        let outcomes =
            futures_util::future::join_all(calls.iter().map(|call| cli::run(call, self.budget)))
                .await;
        let mut replies = Vec::with_capacity(calls.len());
        for (call, outcome) in calls.iter().zip(outcomes) {
            match outcome {
                Ok(reply) => replies.push(reply),
                Err(error) => {
                    return Err(Refusal {
                        call: (*call).clone(),
                        error,
                    });
                }
            }
        }
        Ok(replies)
    }

    /// After a refusal: `Ok` when a login ran and the calls are worth making
    /// again, the explanation otherwise.
    pub(crate) async fn recover(&mut self, refusal: Refusal) -> Result<()> {
        let Refusal { call, error } = refusal;
        let failure = Failure::of(&error, call.action);
        if failure.fixed_by_signing_in() && self.login.after_refusal()? {
            return Ok(());
        }
        // A usage error is the one failure worth a second process: the
        // CLI's version says whether it is simply too old.
        let found = match failure {
            Failure::Usage { .. } => cli::version(&self.target.program, self.budget).await,
            _ => None,
        };
        tracing::debug!(%error, "AWS CLI call failed");
        Err(Explained {
            message: failure.explain(&call, self.login.profile(), found.as_deref(), self.surface),
            failure,
        }
        .into())
    }
}

/// What [`read_pages`] read.
#[derive(Debug)]
pub(crate) struct Pages {
    /// The events `keep` kept, in the order CloudWatch returned them.
    pub(crate) events: Vec<Event>,
    /// CloudWatch handed back the paging token it was given, so reading
    /// stopped there and `events` may be short.
    pub(crate) stalled: bool,
}

/// The sentence for [`Pages::stalled`].
pub(crate) const STALLED: &str = "CloudWatch handed back the paging token it was given, so eks \
                                  stopped reading there; the events above may be incomplete.";

/// Every page of a `filter-log-events` read, counted on the progress line as
/// `noun`. `call` builds the call for a page from its token. `keep` decides
/// each event as it lands, so a long read holds only what will be printed.
pub(crate) async fn read_pages(
    aws: &mut Aws<'_>,
    progress: &Progress,
    noun: &str,
    call: impl Fn(Option<&str>) -> Call,
    mut keep: impl FnMut(&Event) -> bool,
) -> Result<Pages> {
    let mut events = Vec::new();
    let mut read = 0;
    let mut token: Option<String> = None;
    let mut step = progress.reading(noun);
    loop {
        let page_call = call(token.as_deref());
        let reply = match step.tick(aws.attempt(&[&page_call])).await {
            Ok(mut replies) => replies.pop().unwrap_or_default(),
            Err(refusal) => {
                // The line comes off before a login is offered, and goes
                // back up with the count it had: a progress line redrawn over
                // a question is the tool talking over itself.
                drop(step);
                aws.recover(refusal).await?;
                step = progress.reading(noun);
                step.advance(read);
                continue;
            }
        };
        let page =
            logs::page(&reply).context("could not read `aws logs filter-log-events`' reply")?;
        read += page.events.len();
        step.advance(page.events.len());
        events.extend(page.events.into_iter().filter(|event| keep(event)));
        match page::next(token.as_deref(), page.next.as_deref()) {
            page::Next::Page(next) => token = Some(next),
            page::Next::Done => {
                return Ok(Pages {
                    events,
                    stalled: false,
                });
            }
            page::Next::Stalled => {
                return Ok(Pages {
                    events,
                    stalled: true,
                });
            }
        }
    }
}

/// Whether a failed call is worth trying again in a moment, as `--follow`
/// does: throttling, the network, a call outliving `--timeout`.
pub(crate) fn passing(error: &anyhow::Error) -> bool {
    failure(error).is_some_and(Failure::passes)
}

/// What to say while a failure that passes is waited out: the failure, and
/// that the read will be tried again.
pub(crate) fn retrying(error: &anyhow::Error) -> String {
    format!("{error:#}\nTrying again every {}s.", logs::POLL.as_secs())
}

/// What a failed call was, when the error is one [`Aws::recover`] explained.
pub(crate) fn failure(error: &anyhow::Error) -> Option<&Failure> {
    error
        .downcast_ref::<Explained>()
        .map(|explained| &explained.failure)
}

/// A write to stdout failed. `Ok(false)` when the reader at the other end has
/// gone (`| head`), which ends a command quietly; the error otherwise.
pub(crate) fn quiet_on_closed_pipe(error: io::Error) -> Result<bool> {
    if error.kind() == io::ErrorKind::BrokenPipe {
        Ok(false)
    } else {
        Err(anyhow::Error::new(error).context("could not write to stdout"))
    }
}
