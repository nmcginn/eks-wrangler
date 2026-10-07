//! `eks control-plane-logs`: the API server, audit, authenticator, controller
//! manager, and scheduler logs EKS writes to CloudWatch.
//!
//! This is the I/O around [`crate::aws::logs`], [`crate::aws::audit`], and
//! [`crate::aws::cli`], which hold every decision. In order:
//!
//! 1. Work out the cluster, region, and profile from the context
//!    ([`crate::aws::eks::Target`]) and make the same login offer every
//!    command makes ([`AwsLogin`]).
//! 2. Ask `describe-cluster` which log types are on — and, for a type whose
//!    streams have to be named, list the group's streams at the same time.
//!    A type that is off ends the command with the command that would turn it
//!    on; `eks` never runs it.
//! 3. Page through `filter-log-events` from `--since` to now, counting on the
//!    progress line, then print in time order.
//! 4. With `--follow`, poll from just before the newest event every few
//!    seconds and print what is new.
//!
//! Every `aws` run is a child `--timeout` kills, and a refusal for expired
//! credentials is offered a login once and tried again.

use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context as _, Result, bail};
use k8s_openapi::jiff::Timestamp;

use crate::aws::cli::{self, Call, Failure};
use crate::aws::eks::{self, Target};
use crate::aws::logs::{self, Event, LogType, Query, Scope, Selection, Since, Tail};
use crate::aws::{LoginMode, audit};
use crate::commands::credentials::AwsLogin;
use crate::commands::nodes::target_cluster;
use crate::json::Output;
use crate::k8s::client;
use crate::k8s::page::{self, Budget};
use crate::kubeconfig::KubeConfig;
use crate::progress::Progress;
use crate::theme::Palette;

/// Everything `eks control-plane-logs` was asked for.
#[derive(Debug)]
pub struct Request<'a> {
    pub kind: LogType,
    pub since: Since,
    pub grep: Option<&'a str>,
    pub follow: bool,
    pub output: Output,
    pub palette: Palette,
    pub budget: Budget,
    pub login: LoginMode,
    pub progress: Progress,
}

/// Where the command writes: the events to `out`, everything said about
/// them — the empty-window note, the follow banner — to `notes`, so a pipe
/// carries events and nothing else.
pub struct Sinks<'a> {
    pub out: &'a mut dyn Write,
    pub notes: &'a mut dyn Write,
}

impl std::fmt::Debug for Sinks<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sinks")
    }
}

/// Run the command until it has printed the window, or, with `--follow`,
/// until it is stopped.
pub async fn run(
    config: &KubeConfig,
    paths: &[PathBuf],
    context: Option<&str>,
    request: Request<'_>,
    sinks: Sinks<'_>,
) -> Result<()> {
    let view = target_cluster(config, context)?;
    let label = view.label();
    let entry = config
        .resolved_contexts()
        .into_iter()
        .find(|resolved| resolved.name == view.context_name)
        .map(|resolved| resolved.cluster_name)
        .unwrap_or_default();
    let resolved = client::resolve(paths, &view).await?;
    let target = Target::of(
        &view.context_name,
        &entry,
        view.region.as_deref(),
        &resolved.auth_info,
    )?;

    let mut aws = Aws {
        target: &target,
        login: AwsLogin::before(&resolved, &label, request.login)?,
        budget: request.budget,
    };
    let mut reader = Reader {
        kind: request.kind,
        grep: request.grep,
        output: request.output,
        palette: request.palette,
        sinks,
    };

    let start = logs::millis(request.since.start(Timestamp::now()));
    let scope = open(&mut aws, &mut reader, &request.progress, &label, start).await?;

    let mut tail = Tail::new(start);
    let events = read_window(
        &mut aws,
        &mut reader,
        &scope,
        tail.start(),
        &request.progress,
    )
    .await?;
    let shown = tail.admit(events);
    if !reader.print(&shown)? {
        return Ok(());
    }

    if request.follow {
        return follow(&mut aws, &mut reader, scope, tail, &label).await;
    }
    if shown.is_empty() {
        reader.note(&format!(
            "No {} from {label} {}{}. CloudWatch can take a minute or two to receive an event.",
            request.kind.noun(),
            request.since.phrase(),
            request
                .grep
                .map(|text| format!(" containing {text:?}"))
                .unwrap_or_default(),
        ));
    }
    Ok(())
}

/// Check the type is switched on, and work out which streams hold it.
///
/// `describe-cluster` and, for a type read by stream name, the group's
/// stream listing run together: two Python start-ups side by side rather than
/// one after the other.
async fn open(
    aws: &mut Aws<'_>,
    reader: &mut Reader<'_, '_>,
    progress: &Progress,
    label: &str,
    start: i64,
) -> Result<Scope> {
    let kind = reader.kind;
    let describe = aws.target.describe_cluster();
    let listing = logs::describe_streams(aws.target);
    let wanted = match kind.selection() {
        Selection::Listed => vec![&describe, &listing],
        Selection::Prefix(_) => vec![&describe],
    };
    let replies = loop {
        let step = progress.waiting("running aws eks describe-cluster");
        match step.tick(aws.attempt(&wanted)).await {
            Ok(replies) => break replies,
            Err(refusal) => {
                // Off the screen before a login is offered: a progress line
                // redrawn over a question is the tool talking over itself.
                drop(step);
                aws.recover(refusal).await?;
            }
        }
    };
    let mut replies = replies.into_iter();
    let (described, streams) = (replies.next().unwrap_or_default(), replies.next());

    let enabled =
        eks::logging(&described).context("could not read `aws eks describe-cluster`'s reply")?;
    if !enabled.contains(&kind) {
        bail!("{}", logs::not_enabled(kind, &enabled, aws.target, label));
    }

    match kind.selection() {
        Selection::Prefix(prefix) => Ok(Scope::Prefix(prefix)),
        Selection::Listed => Ok(reader.named(&read_streams(&streams.unwrap_or_default())?, start)),
    }
}

/// Poll for new events until stopped: every [`logs::POLL`], from just
/// before the newest event printed, listing the streams again every
/// [`logs::RELIST`] for a type read by name.
///
/// A poll that fails for a reason that passes — throttling, the network, a
/// call outliving `--timeout` — is reported once and tried again on the next
/// tick, rather than ending a tail somebody may have left running for an
/// hour. Anything else ends it, explained, as it would end a bounded read.
async fn follow(
    aws: &mut Aws<'_>,
    reader: &mut Reader<'_, '_>,
    mut scope: Scope,
    mut tail: Tail,
    label: &str,
) -> Result<()> {
    reader.note(&format!(
        "Following {} from {label}; Ctrl-C to stop.",
        reader.kind.noun()
    ));
    let listing = logs::describe_streams(aws.target);
    let mut listed_at = Instant::now();
    let mut failing = false;
    loop {
        tokio::time::sleep(logs::POLL).await;
        let polled = async {
            if matches!(scope, Scope::Named(_)) && listed_at.elapsed() >= logs::RELIST {
                let reply = aws.run(&listing).await?;
                scope = reader.named(&read_streams(&reply)?, tail.start());
                listed_at = Instant::now();
            }
            read_window(aws, reader, &scope, tail.start(), &Progress::none()).await
        }
        .await;
        match polled {
            Ok(events) => {
                if failing {
                    reader.note("Reading again.");
                    failing = false;
                }
                if !reader.print(&tail.admit(events))? {
                    return Ok(());
                }
            }
            Err(error) if passing(&error) => {
                if !failing {
                    reader.note(&format!(
                        "{error:#}\nTrying again every {}s.",
                        logs::POLL.as_secs()
                    ));
                    failing = true;
                }
            }
            Err(error) => return Err(error),
        }
    }
}

/// Whether a failed poll is worth trying again.
fn passing(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<Explained>()
        .is_some_and(|explained| explained.failure.passes())
}

fn read_streams(reply: &[u8]) -> Result<Vec<logs::Stream>> {
    logs::streams(reply).context("could not read `aws logs describe-log-streams`' reply")
}

/// Every page from `start` to now, `--grep` applied, counted on the
/// progress line.
async fn read_window(
    aws: &mut Aws<'_>,
    reader: &mut Reader<'_, '_>,
    scope: &Scope,
    start: i64,
    progress: &Progress,
) -> Result<Vec<Event>> {
    // Nothing to read: a listed type whose streams have all gone quiet.
    if matches!(scope, Scope::Named(names) if names.is_empty()) {
        return Ok(Vec::new());
    }
    let query = Query {
        kind: reader.kind,
        scope,
        start,
        grep: reader.grep,
    };
    let mut events = Vec::new();
    let mut read = 0;
    let mut token: Option<String> = None;
    let mut step = progress.reading(reader.kind.noun());
    loop {
        let call = logs::filter_events(aws.target, &query, token.as_deref());
        let reply = match step.tick(aws.attempt(&[&call])).await {
            Ok(mut replies) => replies.pop().unwrap_or_default(),
            Err(refusal) => {
                // As in `open`: the line comes off before a login is offered,
                // and goes back up with the count it had.
                drop(step);
                aws.recover(refusal).await?;
                step = progress.reading(reader.kind.noun());
                step.advance(read);
                continue;
            }
        };
        let page =
            logs::page(&reply).context("could not read `aws logs filter-log-events`' reply")?;
        read += page.events.len();
        step.advance(page.events.len());
        events.extend(
            page.events
                .into_iter()
                .filter(|event| logs::matches(&event.message, reader.grep)),
        );
        match page::next(token.as_deref(), page.next.as_deref()) {
            page::Next::Page(next) => token = Some(next),
            page::Next::Done => return Ok(events),
            page::Next::Stalled => {
                reader.note(
                    "CloudWatch handed back the paging token it was given, so eks stopped \
                     reading there; the events above may be incomplete.",
                );
                return Ok(events);
            }
        }
    }
}

/// The AWS CLI, as this command runs it: under the budget, and with one
/// login offered when the credentials turn out to have expired.
///
/// Split in two so the offer is made with nothing else on the screen:
/// [`Aws::attempt`] runs calls and hands back a [`Refusal`] untouched, and
/// [`Aws::recover`] — called once the caller has taken its progress line down
/// — offers the login or explains. A loop around the pair ends, because the
/// login is offered at most once per command.
struct Aws<'a> {
    target: &'a Target,
    login: AwsLogin,
    budget: Budget,
}

/// A call that failed, not yet explained.
struct Refusal {
    call: Call,
    error: cli::Error,
}

/// A failed call, explained: the sentence, and what it was, so `--follow` can
/// tell a failure that passes from one that does not.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct Explained {
    message: String,
    failure: Failure,
}

impl Aws<'_> {
    /// One call, with the login offer taken if it is needed. For a call with
    /// no progress line to take down.
    async fn run(&mut self, call: &Call) -> Result<Vec<u8>> {
        loop {
            match self.attempt(&[call]).await {
                Ok(mut replies) => return Ok(replies.pop().unwrap_or_default()),
                Err(refusal) => self.recover(refusal).await?,
            }
        }
    }

    /// Several calls at once, replies in order. Any one failing fails all.
    async fn attempt(&self, calls: &[&Call]) -> Result<Vec<Vec<u8>>, Refusal> {
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
    async fn recover(&mut self, refusal: Refusal) -> Result<()> {
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
            message: failure.explain(&call, self.login.profile(), found.as_deref()),
            failure,
        }
        .into())
    }
}

/// Printing, and what is printed.
struct Reader<'s, 'w> {
    kind: LogType,
    grep: Option<&'s str>,
    output: Output,
    palette: Palette,
    sinks: Sinks<'w>,
}

impl Reader<'_, '_> {
    /// Print events. `false` once the reader at the other end of stdout has
    /// gone — `| head` — which ends the command quietly.
    fn print(&mut self, events: &[Event]) -> Result<bool> {
        for event in events {
            let line = match self.output {
                Output::Json => audit::json_line(self.kind, event)
                    .context("could not write the event as JSON")?,
                Output::Table => audit::line(self.kind, event, self.palette),
            };
            if let Err(error) = writeln!(self.sinks.out, "{line}") {
                return quiet_on_closed_pipe(error);
            }
        }
        match self.sinks.out.flush() {
            Ok(()) => Ok(true),
            Err(error) => quiet_on_closed_pipe(error),
        }
    }

    /// A sentence about the output, on the notes stream.
    fn note(&mut self, text: &str) {
        // A note that cannot be written is not worth failing the command
        // over: the events are what was asked for.
        let _ = writeln!(self.sinks.notes, "{text}");
    }

    /// The streams to read a listed type from, saying so when there were
    /// more than one read can name.
    fn named(&mut self, streams: &[logs::Stream], start: i64) -> Scope {
        let (names, dropped) = logs::pick_streams(self.kind, streams, start);
        if dropped > 0 {
            self.note(&format!(
                "Reading the {} most recently written of {} {} streams, the most CloudWatch \
                 searches at once; events only in the other {dropped} are left out. A shorter \
                 --since needs fewer streams.",
                names.len(),
                names.len() + dropped,
                self.kind.flag(),
            ));
        }
        Scope::Named(names)
    }
}

fn quiet_on_closed_pipe(error: io::Error) -> Result<bool> {
    if error.kind() == io::ErrorKind::BrokenPipe {
        Ok(false)
    } else {
        Err(anyhow::Error::new(error).context("could not write the events"))
    }
}
