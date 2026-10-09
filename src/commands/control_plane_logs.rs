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

use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Instant;

use anyhow::{Context as _, Result, bail};
use k8s_openapi::jiff::Timestamp;

use crate::aws::cli::{Failure, Surface};
use crate::aws::eks::{self, Target};
use crate::aws::logs::{self, Event, LogType, Query, Scope, Selection, Since, Tail};
use crate::aws::{LoginMode, audit};
use crate::commands::cloudwatch::{self, Aws, Pages, passing, quiet_on_closed_pipe};
use crate::commands::credentials::AwsLogin;
use crate::commands::nodes::target_cluster;
use crate::commands::{self as cmd, FetchError, StreamHandle};
use crate::json::Output;
use crate::k8s::client;
use crate::k8s::page::Budget;
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
    let found = locate(config, paths, context).await?;
    let label = found.label;
    let target = found.target;

    let mut aws = Aws {
        target: &target,
        login: AwsLogin::before(&found.resolved, &label, request.login)?,
        budget: request.budget,
        surface: Surface::Command,
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

/// The cluster a context names, as the AWS CLI is pointed at it.
struct Located {
    label: String,
    resolved: kube::Config,
    target: Target,
}

/// Work out the cluster, region, and profile from the context.
async fn locate(config: &KubeConfig, paths: &[PathBuf], context: Option<&str>) -> Result<Located> {
    let view = target_cluster(config, context)?;
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
    Ok(Located {
        label: view.label(),
        resolved,
        target,
    })
}

/// Check the type is switched on, and work out which streams hold it.
async fn open(
    aws: &mut Aws<'_>,
    reader: &mut Reader<'_, '_>,
    progress: &Progress,
    label: &str,
    start: i64,
) -> Result<Scope> {
    let kind = reader.kind;
    let described = describe(aws, kind, progress).await?;
    if !described.enabled.contains(&kind) {
        bail!(
            "{}",
            logs::not_enabled(kind, &described.enabled, aws.target, label, aws.surface)
        );
    }
    match kind.selection() {
        Selection::Prefix(prefix) => Ok(Scope::Prefix(prefix)),
        Selection::Listed => Ok(reader.named(&described.streams, start)),
    }
}

/// What `describe-cluster`, and the stream listing beside it, said.
struct Described {
    /// The log types the cluster sends to CloudWatch.
    enabled: Vec<LogType>,
    /// The group's streams, for a type read by name; empty otherwise.
    streams: Vec<logs::Stream>,
}

/// Ask which log types are on, and, for a type read by stream name, list
/// the group's streams.
///
/// `describe-cluster` and the listing run together: two Python start-ups
/// side by side rather than one after the other.
async fn describe(aws: &mut Aws<'_>, kind: LogType, progress: &Progress) -> Result<Described> {
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
    let streams = match streams {
        Some(reply) => read_streams(&reply)?,
        None => Vec::new(),
    };
    Ok(Described { enabled, streams })
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
                    reader.note(&cloudwatch::retrying(&error));
                    failing = true;
                }
            }
            Err(error) => return Err(error),
        }
    }
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
    let pages = read_events(aws, reader.kind, scope, start, reader.grep, progress).await?;
    if pages.stalled {
        reader.note(cloudwatch::STALLED);
    }
    Ok(pages.events)
}

/// Every page of `kind` from `start` to now, in `scope`, with `grep` applied.
async fn read_events(
    aws: &mut Aws<'_>,
    kind: LogType,
    scope: &Scope,
    start: i64,
    grep: Option<&str>,
    progress: &Progress,
) -> Result<Pages> {
    // Nothing to read: a listed type whose streams have all gone quiet.
    if matches!(scope, Scope::Named(names) if names.is_empty()) {
        return Ok(Pages {
            events: Vec::new(),
            stalled: false,
        });
    }
    let query = Query {
        kind,
        scope,
        start,
        grep,
    };
    let target = aws.target;
    cloudwatch::read_pages(
        aws,
        progress,
        kind.noun(),
        |token| logs::filter_events(target, &query, token),
        |event| logs::matches(&event.message, grep),
    )
    .await
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
        if let Some(note) = logs::left_out(self.kind, names.len(), dropped, Surface::Command) {
            self.note(&note);
        }
        Scope::Named(names)
    }
}

/// What the dashboard's control-plane pane hears from [`spawn_dashboard`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    /// A read finished. `lines` are the events it found that were not shown
    /// before, oldest first, one entry per screen line. A quiet poll sends
    /// none, and so does a first read that found nothing; that is how the
    /// pane knows the window has been read. `note` describes the read as a
    /// whole (streams left out, a read CloudWatch cut short) and replaces
    /// whatever the last read said.
    Read {
        lines: Vec<String>,
        note: Option<String>,
    },
    /// A call failed for a reason that passes: throttling, the network, a
    /// call outliving `--timeout`. It is tried again in [`logs::POLL`].
    Retrying(String),
    /// The cluster does not send this type to CloudWatch: why, and the
    /// command that would switch it on.
    Off(String),
    /// Reading has stopped for good.
    Failed(FetchError),
}

/// Read one log type of the named context's cluster in the background, the
/// last [`Since::default`] of it and then every [`logs::POLL`], until the
/// returned [`StreamHandle`] is dropped.
///
/// The dashboard's `C`. The same calls `eks control-plane-logs --follow`
/// makes, with two differences. Nothing here may ask the user anything: the
/// login offer is [`LoginMode::Never`], and the `aws` children already run
/// with no terminal (decision 120). A failure that passes is waited out from
/// the first call on, not only once following has begun, because a pane can
/// be left open far longer than a command is. Dropping the handle drops the
/// read in progress, and with it the `aws` child, which is `kill_on_drop`.
#[must_use]
pub fn spawn_dashboard(
    config: KubeConfig,
    paths: Vec<PathBuf>,
    context: String,
    kind: LogType,
    budget: Budget,
) -> (mpsc::Receiver<Update>, StreamHandle) {
    cmd::spawn_stream(move |tx, stop| async move {
        tokio::select! {
            outcome = stream(&config, &paths, &context, kind, budget, &tx) => {
                if let Err(error) = outcome {
                    let _ = tx.send(Update::Failed(failed(&error)));
                }
            }
            _ = stop => {}
        }
    })
}

/// [`spawn_dashboard`]'s task. `Ok` when the pane has gone, or the type is
/// off; the error that stopped it otherwise.
async fn stream(
    config: &KubeConfig,
    paths: &[PathBuf],
    context: &str,
    kind: LogType,
    budget: Budget,
    tx: &mpsc::Sender<Update>,
) -> Result<()> {
    let found = locate(config, paths, Some(context)).await?;
    let mut aws = Aws {
        target: &found.target,
        login: AwsLogin::before(&found.resolved, &found.label, LoginMode::Never)?,
        budget,
        surface: Surface::Dashboard,
    };
    let start = logs::millis(Since::default().start(Timestamp::now()));

    let described = loop {
        match describe(&mut aws, kind, &Progress::none()).await {
            Ok(described) => break described,
            Err(error) if passing(&error) => {
                if tx
                    .send(Update::Retrying(cloudwatch::retrying(&error)))
                    .is_err()
                {
                    return Ok(());
                }
                tokio::time::sleep(logs::POLL).await;
            }
            Err(error) => return Err(error),
        }
    };
    if !described.enabled.contains(&kind) {
        let advice = logs::not_enabled(
            kind,
            &described.enabled,
            aws.target,
            &found.label,
            Surface::Dashboard,
        );
        let _ = tx.send(Update::Off(advice));
        return Ok(());
    }

    let mut chosen = choose(kind, &described.streams, start);
    let mut tail = Tail::new(start);
    let listing = logs::describe_streams(aws.target);
    let mut listed_at = Instant::now();
    loop {
        let read = async {
            if matches!(chosen.scope, Scope::Named(_)) && listed_at.elapsed() >= logs::RELIST {
                let reply = aws.run(&listing).await?;
                chosen = choose(kind, &read_streams(&reply)?, tail.start());
                listed_at = Instant::now();
            }
            read_events(
                &mut aws,
                kind,
                &chosen.scope,
                tail.start(),
                None,
                &Progress::none(),
            )
            .await
        }
        .await;
        let update = match read {
            Ok(pages) => Update::Read {
                lines: lines(kind, &tail.admit(pages.events)),
                note: read_note(chosen.left_out.as_deref(), pages.stalled),
            },
            Err(error) if passing(&error) => Update::Retrying(cloudwatch::retrying(&error)),
            Err(error) => return Err(error),
        };
        if tx.send(update).is_err() {
            return Ok(());
        }
        tokio::time::sleep(logs::POLL).await;
    }
}

/// The streams a pane reads, and what to say about any it left out.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Chosen {
    scope: Scope,
    left_out: Option<String>,
}

/// [`Reader::named`] for the pane, which has nowhere to print a note
/// immediately, so it keeps the note to send with each read.
fn choose(kind: LogType, streams: &[logs::Stream], start: i64) -> Chosen {
    match kind.selection() {
        Selection::Prefix(prefix) => Chosen {
            scope: Scope::Prefix(prefix),
            left_out: None,
        },
        Selection::Listed => {
            let (names, dropped) = logs::pick_streams(kind, streams, start);
            let left_out = logs::left_out(kind, names.len(), dropped, Surface::Dashboard);
            Chosen {
                scope: Scope::Named(names),
                left_out,
            }
        }
    }
}

/// Events as the pane's lines: the command's own line for each, uncoloured
/// because the pane draws in its theme, and split where an event's message
/// carries a newline, since one screen line cannot.
fn lines(kind: LogType, events: &[Event]) -> Vec<String> {
    events
        .iter()
        .flat_map(|event| {
            audit::line(kind, event, Palette::Plain)
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// What to say about a read as a whole: streams left out, and a read
/// CloudWatch cut short.
fn read_note(left_out: Option<&str>, stalled: bool) -> Option<String> {
    let notes: Vec<&str> = left_out
        .into_iter()
        .chain(stalled.then_some(cloudwatch::STALLED))
        .collect();
    (!notes.is_empty()).then(|| notes.join("\n"))
}

/// A stopped read, for the pane, with `L` offered when signing in again
/// would fix it: a cluster's refusal, or the AWS CLI's.
fn failed(error: &anyhow::Error) -> FetchError {
    let mut fetch = FetchError::of(error);
    fetch.credentials |= cloudwatch::failure(error).is_some_and(Failure::fixed_by_signing_in);
    fetch
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::commands::cloudwatch::Explained;

    fn event(message: &str) -> Event {
        Event {
            stream: "kube-apiserver-1".to_owned(),
            timestamp: 1_791_353_662_000,
            message: message.to_owned(),
            id: "1".to_owned(),
        }
    }

    fn stream(name: &str) -> logs::Stream {
        logs::Stream {
            name: name.to_owned(),
            last_event: None,
            last_ingestion: None,
        }
    }

    #[test]
    fn a_pane_line_is_the_command_s_line_without_colour() {
        let shown = lines(LogType::Api, &[event("I1009 06:21:02 started")]);

        assert_eq!(
            shown,
            [audit::line(
                LogType::Api,
                &event("I1009 06:21:02 started"),
                Palette::Plain
            )]
        );
        assert!(shown[0].ends_with("  I1009 06:21:02 started"), "{shown:?}");
        assert!(!shown[0].contains('\x1b'), "{shown:?}");
    }

    #[test]
    fn an_audit_event_is_summarised_as_the_command_summarises_it() {
        let audit = r#"{"kind":"Event","apiVersion":"audit.k8s.io/v1","stage":"ResponseComplete","verb":"delete","user":{"username":"alice"},"objectRef":{"resource":"pods","namespace":"shop","name":"api"},"responseStatus":{"code":200}}"#;

        let shown = lines(LogType::Audit, &[event(audit)]);

        assert_eq!(shown.len(), 1);
        assert!(shown[0].contains("alice  delete"), "{shown:?}");
        assert!(shown[0].ends_with("200"), "{shown:?}");
    }

    #[test]
    fn an_event_with_a_newline_in_it_is_two_screen_lines() {
        let shown = lines(
            LogType::Api,
            &[event("panic: boom\ngoroutine 1 [running]:"), event("after")],
        );

        assert_eq!(shown.len(), 3, "{shown:?}");
        assert!(shown[0].ends_with("panic: boom"), "{shown:?}");
        assert_eq!(shown[1], "goroutine 1 [running]:");
        assert!(shown[2].ends_with("after"), "{shown:?}");
    }

    #[test]
    fn no_events_are_no_lines() {
        assert_eq!(lines(LogType::Audit, &[]), Vec::<String>::new());
    }

    #[test]
    fn a_read_with_nothing_to_say_about_it_has_no_note() {
        assert_eq!(read_note(None, false), None);
    }

    #[test]
    fn a_read_cut_short_says_so_and_keeps_the_streams_left_out() {
        assert_eq!(read_note(None, true).as_deref(), Some(cloudwatch::STALLED));
        assert_eq!(
            read_note(Some("left out"), true),
            Some(format!("left out\n{}", cloudwatch::STALLED))
        );
        assert_eq!(
            read_note(Some("left out"), false).as_deref(),
            Some("left out")
        );
    }

    #[test]
    fn a_prefixed_type_is_read_by_prefix_with_nothing_left_out() {
        let chosen = choose(LogType::Audit, &[stream("kube-apiserver-audit-1")], 0);

        assert_eq!(chosen.scope, Scope::Prefix("kube-apiserver-audit-"));
        assert_eq!(chosen.left_out, None);
    }

    #[test]
    fn a_listed_type_is_read_by_its_own_streams_names() {
        let streams = [
            stream("kube-apiserver-audit-1"),
            stream("kube-apiserver-1"),
            stream("kube-scheduler-1"),
        ];

        let chosen = choose(LogType::Api, &streams, 0);

        assert_eq!(
            chosen.scope,
            Scope::Named(vec!["kube-apiserver-1".to_owned()])
        );
        assert_eq!(chosen.left_out, None);
    }

    #[test]
    fn a_listed_type_with_too_many_streams_says_which_were_left_out_without_naming_a_flag() {
        let streams: Vec<logs::Stream> = (0..logs::MAX_STREAMS + 2)
            .map(|n| stream(&format!("kube-apiserver-{n:03}")))
            .collect();

        let chosen = choose(LogType::Api, &streams, 0);

        let note = chosen.left_out.unwrap();
        assert!(note.contains("the other 2 are left out"), "{note}");
        assert!(!note.contains("--since"), "{note}");
    }

    #[test]
    fn an_expired_aws_session_offers_l() {
        let error = anyhow::Error::new(Explained {
            message: "AWS refused `aws logs filter-log-events`".to_owned(),
            failure: Failure::Expired {
                detail: "Token has expired".to_owned(),
            },
        });

        let fetch = failed(&error);

        assert!(fetch.credentials);
        assert_eq!(fetch.message, "AWS refused `aws logs filter-log-events`");
    }

    #[test]
    fn a_failure_signing_in_could_not_fix_does_not_offer_l() {
        let denied = anyhow::Error::new(Explained {
            message: "AWS refused".to_owned(),
            failure: Failure::Denied {
                action: "logs:FilterLogEvents".to_owned(),
                principal: None,
            },
        });
        assert!(!failed(&denied).credentials);
        assert!(!failed(&anyhow::anyhow!("could not read the reply")).credentials);
    }
}
