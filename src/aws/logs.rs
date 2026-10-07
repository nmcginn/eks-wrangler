//! EKS control-plane logs in CloudWatch: which streams hold which log type,
//! the `aws logs` calls that read them, and the rules for paging and
//! following them. Everything here is pure; `commands::control_plane_logs`
//! does the running.
//!
//! EKS writes every enabled log type into one group, `/aws/eks/<cluster>/cluster`,
//! one stream per type per control-plane instance:
//!
//! | type | streams |
//! |---|---|
//! | `api` | `kube-apiserver-<id>` |
//! | `audit` | `kube-apiserver-audit-<id>` |
//! | `authenticator` | `authenticator-<id>` |
//! | `controllerManager` | `kube-controller-manager-<id>`, `cloud-controller-manager-<id>` |
//! | `scheduler` | `kube-scheduler-<id>` |
//!
//! Three types are one name prefix that no other type shares, and are read with
//! `--log-stream-name-prefix`. The other two cannot be: every audit stream
//! also starts `kube-apiserver-`, and the controller manager's streams have
//! two prefixes. For those, the group's streams are listed first and the
//! type's own are named — see [`Selection`] and [`pick_streams`]. Reading
//! `api` by prefix and dropping the audit events afterwards would page through
//! the audit log, which is usually the largest thing in the group, to find
//! the few lines that are not.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use k8s_openapi::jiff::{SignedDuration, Timestamp};
use serde::Deserialize;

use crate::aws::cli::Call;
use crate::aws::eks::Target;
use crate::format;

/// One of the five control-plane log types EKS can send to CloudWatch.
///
/// `clap::ValueEnum`, so `--type` takes these spellings and lists them when
/// given something else. The flag spells `controllerManager` the way every
/// other flag here is spelled; [`LogType::eks_name`] is EKS's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, clap::ValueEnum)]
pub enum LogType {
    /// The API server's own log.
    Api,
    /// The Kubernetes audit log: who did what to which object.
    Audit,
    /// The IAM authenticator: which AWS identity was mapped to which user,
    /// and which were refused.
    Authenticator,
    /// The controller manager, and the cloud controller manager beside it.
    ControllerManager,
    /// The scheduler.
    Scheduler,
}

/// How a type's streams are chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    /// Every stream whose name starts with this, and only those, are the type's.
    Prefix(&'static str),
    /// The type's streams have to be listed and named.
    Listed,
}

impl LogType {
    /// Every type, in the order EKS documents them.
    pub const ALL: [Self; 5] = [
        Self::Api,
        Self::Audit,
        Self::Authenticator,
        Self::ControllerManager,
        Self::Scheduler,
    ];

    /// EKS's name for the type, as `describe-cluster` and
    /// `update-cluster-config` spell it.
    #[must_use]
    pub fn eks_name(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Audit => "audit",
            Self::Authenticator => "authenticator",
            Self::ControllerManager => "controllerManager",
            Self::Scheduler => "scheduler",
        }
    }

    /// The type from EKS's name for it.
    #[must_use]
    pub fn from_eks(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.eks_name() == name)
    }

    /// The spelling `--type` takes.
    #[must_use]
    pub fn flag(self) -> &'static str {
        match self {
            Self::ControllerManager => "controller-manager",
            other => other.eks_name(),
        }
    }

    /// What one of this type's events is called in a sentence.
    #[must_use]
    pub fn noun(self) -> &'static str {
        match self {
            Self::Api => "API server log lines",
            Self::Audit => "audit events",
            Self::Authenticator => "authenticator log lines",
            Self::ControllerManager => "controller manager log lines",
            Self::Scheduler => "scheduler log lines",
        }
    }

    /// How this type's streams are found.
    #[must_use]
    pub fn selection(self) -> Selection {
        match self {
            Self::Audit => Selection::Prefix("kube-apiserver-audit-"),
            Self::Authenticator => Selection::Prefix("authenticator-"),
            Self::Scheduler => Selection::Prefix("kube-scheduler-"),
            Self::Api | Self::ControllerManager => Selection::Listed,
        }
    }

    /// Whether a stream in the group carries this type.
    #[must_use]
    pub fn owns(self, stream: &str) -> bool {
        match self {
            Self::Api => stream.starts_with("kube-apiserver-") && !Self::Audit.owns(stream),
            Self::ControllerManager => {
                stream.starts_with("kube-controller-manager-")
                    || stream.starts_with("cloud-controller-manager-")
            }
            Self::Audit => stream.starts_with("kube-apiserver-audit-"),
            Self::Authenticator => stream.starts_with("authenticator-"),
            Self::Scheduler => stream.starts_with("kube-scheduler-"),
        }
    }
}

impl fmt::Display for LogType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.flag())
    }
}

/// The CloudWatch log group EKS writes a cluster's control-plane logs to.
#[must_use]
pub fn group(cluster: &str) -> String {
    format!("/aws/eks/{cluster}/cluster")
}

/// Why `kind` cannot be read, and how to switch it on.
///
/// Says which types *are* on, so somebody who asked for `api` while `audit`
/// is enabled sees the one they can read today. Names the cost, because
/// switching a log type on is a standing charge somebody else may be paying,
/// and says eks will not do it for them.
#[must_use]
pub fn not_enabled(kind: LogType, enabled: &[LogType], target: &Target, label: &str) -> String {
    let on: Vec<String> = enabled.iter().map(|kind| kind.flag().to_owned()).collect();
    let now = match (format::list(&on, "and"), enabled.first()) {
        (Some(list), Some(first)) => {
            format!("Switched on now: {list} (read it with `--type {first}`).")
        }
        _ => "No control-plane log type is switched on.".to_owned(),
    };
    format!(
        "{label} does not send its {flag} log to CloudWatch. {now}\n\
         EKS only writes the types someone has switched on, and eks never switches one on \
         for you: CloudWatch charges for every gigabyte it ingests and stores{audit}.\n\
         To switch it on (only events from then on are recorded):\n  {command}",
        flag = kind.flag(),
        audit = if kind == LogType::Audit {
            ", and the audit log is usually the largest of the five"
        } else {
            ""
        },
        command = target.enable_command(kind),
    )
}

/// How far back to read: `--since`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Since {
    /// This long before now.
    Ago(Duration),
    /// From this instant.
    At(Timestamp),
}

impl Default for Since {
    /// An hour: long enough to find what just went wrong, short enough that a
    /// busy cluster's audit log is a few pages rather than a few hundred.
    fn default() -> Self {
        Self::Ago(Duration::from_secs(3600))
    }
}

impl Since {
    /// The instant reading starts from.
    #[must_use]
    pub fn start(self, now: Timestamp) -> Timestamp {
        match self {
            Self::At(at) => at,
            Self::Ago(span) => SignedDuration::try_from(span)
                .ok()
                .and_then(|span| now.checked_sub(span).ok())
                .unwrap_or(Timestamp::MIN),
        }
    }

    /// The window as a phrase: `in the last 1h`, `since 2026-10-07T05:00:00Z`.
    #[must_use]
    pub fn phrase(self) -> String {
        match self {
            Self::Ago(span) => format!("in the last {}", span_spelling(span)),
            Self::At(at) => format!("since {}", seconds(at)),
        }
    }
}

/// `d`, `h`, `m`, or `s`, whichever is the largest that divides it.
fn span_spelling(span: Duration) -> String {
    let seconds = span.as_secs();
    for (unit, size) in [("d", 86_400), ("h", 3_600), ("m", 60)] {
        if seconds >= size && seconds.is_multiple_of(size) {
            return format!("{}{unit}", seconds / size);
        }
    }
    format!("{seconds}s")
}

impl fmt::Display for Since {
    /// The spelling [`Since::from_str`] reads back.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ago(span) => f.write_str(&span_spelling(*span)),
            Self::At(at) => f.write_str(&seconds(*at)),
        }
    }
}

/// Why a `--since` could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "not a length of time or an instant; give `30s`, `15m`, `2h`, `3d`, or an RFC 3339 time \
     such as `2026-10-07T05:00:00Z`."
)]
pub struct SinceError;

impl FromStr for Since {
    type Err = SinceError;

    /// `30s`, `15m`, `2h`, `3d`, or an RFC 3339 instant.
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let input = input.trim();
        if let Ok(at) = input.parse::<Timestamp>() {
            return Ok(Self::At(at));
        }
        let digits = input
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(input.len());
        let (count, unit) = input.split_at(digits);
        let count: u64 = count.parse().map_err(|_| SinceError)?;
        let size = match unit {
            "s" => 1,
            "m" => 60,
            "h" => 3_600,
            "d" => 86_400,
            _ => return Err(SinceError),
        };
        match count.checked_mul(size) {
            Some(seconds) if seconds > 0 => Ok(Self::Ago(Duration::from_secs(seconds))),
            _ => Err(SinceError),
        }
    }
}

/// An instant to the second, `2026-10-07T05:00:00Z`.
#[must_use]
pub fn seconds(at: Timestamp) -> String {
    format!("{at:.0}")
}

/// Milliseconds since the epoch, CloudWatch's unit for time.
#[must_use]
pub fn millis(at: Timestamp) -> i64 {
    at.as_millisecond()
}

/// An event's CloudWatch timestamp as an instant to the second.
#[must_use]
pub fn event_time(timestamp: i64) -> String {
    Timestamp::from_millisecond(timestamp).map_or_else(|_| timestamp.to_string(), seconds)
}

/// One stream in the group, as `describe-log-streams` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Stream {
    #[serde(rename = "logStreamName")]
    pub name: String,
    #[serde(rename = "lastEventTimestamp", default)]
    pub last_event: Option<i64>,
    #[serde(rename = "lastIngestionTime", default)]
    pub last_ingestion: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct Streams {
    #[serde(rename = "logStreams", default)]
    streams: Vec<Stream>,
}

/// Read `describe-log-streams`' reply.
pub fn streams(reply: &[u8]) -> Result<Vec<Stream>, serde_json::Error> {
    Ok(serde_json::from_slice::<Streams>(reply)?.streams)
}

/// The most streams `filter-log-events` takes by name.
pub const MAX_STREAMS: usize = 100;

/// How stale a stream's last-event time may look and still be read.
///
/// CloudWatch updates `lastEventTimestamp` "on an eventual consistency
/// basis, typically within an hour", so a stream that looks an hour quiet
/// may still hold the event being looked for.
const STREAM_LAG_MS: i64 = 3_600_000;

/// The streams to read `kind` from, for events from `start` on.
///
/// The type's own streams that may hold an event after `start`, newest
/// first, and at most [`MAX_STREAMS`] of them. The second value is how many
/// more there were. A control plane is two or three instances at a time, so
/// that only matters for a `--since` reaching back across months of upgrades.
#[must_use]
pub fn pick_streams(kind: LogType, streams: &[Stream], start: i64) -> (Vec<String>, usize) {
    let mut mine: Vec<(Option<i64>, &str)> = streams
        .iter()
        .filter(|stream| kind.owns(&stream.name))
        .map(|stream| {
            (
                stream.last_event.max(stream.last_ingestion),
                stream.name.as_str(),
            )
        })
        .filter(|(last, _)| last.is_none_or(|last| last.saturating_add(STREAM_LAG_MS) >= start))
        .collect();
    // `None` — never written to, as far as the listing knows — sorts first,
    // which is where a stream just created belongs.
    mine.sort_by(|(a_last, a_name), (b_last, b_name)| {
        let newest_first = match (a_last, b_last) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (Some(_), None) => std::cmp::Ordering::Greater,
            (Some(a), Some(b)) => b.cmp(a),
        };
        newest_first.then_with(|| a_name.cmp(b_name))
    });
    let dropped = mine.len().saturating_sub(MAX_STREAMS);
    let names = mine
        .into_iter()
        .take(MAX_STREAMS)
        .map(|(_, name)| name.to_owned())
        .collect();
    (names, dropped)
}

/// Which streams one read covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Prefix(&'static str),
    Named(Vec<String>),
}

/// How many events one `filter-log-events` run asks for.
///
/// Each run is a Python start-up, so a page is large; each is also one
/// redraw of the progress line, so it is not unlimited.
pub const PAGE_ITEMS: u32 = 5_000;

/// What one read asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query<'a> {
    pub kind: LogType,
    pub scope: &'a Scope,
    /// Milliseconds since the epoch, inclusive.
    pub start: i64,
    pub grep: Option<&'a str>,
}

/// `aws logs describe-log-streams` for the whole group. The AWS CLI pages
/// this one itself: a group holds a handful of streams per instance.
#[must_use]
pub fn describe_streams(target: &Target) -> Call {
    target.call(
        &["logs", "describe-log-streams"],
        vec!["--log-group-name".to_owned(), group(&target.cluster)],
        "logs:DescribeLogStreams",
    )
}

/// `aws logs filter-log-events` for one page of `query`.
///
/// `--max-items` and `--starting-token` are the AWS CLI's own paging, which
/// hands back a `NextToken` while there is more; `eks` follows it so the
/// progress line can count each page as it lands.
#[must_use]
pub fn filter_events(target: &Target, query: &Query<'_>, token: Option<&str>) -> Call {
    let mut extra = vec![
        "--log-group-name".to_owned(),
        group(&target.cluster),
        "--start-time".to_owned(),
        query.start.to_string(),
    ];
    match query.scope {
        Scope::Prefix(prefix) => {
            extra.extend(["--log-stream-name-prefix".to_owned(), (*prefix).to_owned()]);
        }
        Scope::Named(names) => {
            extra.push("--log-stream-names".to_owned());
            extra.extend(names.iter().cloned());
        }
    }
    if let Some(text) = query.grep {
        extra.extend(["--filter-pattern".to_owned(), pattern(text)]);
    }
    extra.extend(["--max-items".to_owned(), PAGE_ITEMS.to_string()]);
    if let Some(token) = token {
        extra.extend(["--starting-token".to_owned(), token.to_owned()]);
    }
    target.call(
        &["logs", "filter-log-events"],
        extra,
        "logs:FilterLogEvents",
    )
}

/// `--grep` as a CloudWatch filter pattern: one quoted term, so its spaces
/// and punctuation are matched as written rather than read as pattern
/// syntax.
///
/// The server narrows what crosses the network; [`matches()`] then decides,
/// so what `--grep` means is a plain, case-sensitive substring whatever the
/// pattern language makes of an edge case.
#[must_use]
pub fn pattern(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', r"\\").replace('"', "\\\""))
}

/// Whether an event's message contains the `--grep` text.
#[must_use]
pub fn matches(message: &str, grep: Option<&str>) -> bool {
    grep.is_none_or(|text| message.contains(text))
}

/// One event, as `filter-log-events` returns it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Event {
    #[serde(rename = "logStreamName", default)]
    pub stream: String,
    /// Milliseconds since the epoch.
    pub timestamp: i64,
    #[serde(default)]
    pub message: String,
    #[serde(rename = "eventId", default)]
    pub id: String,
}

/// One page of `filter-log-events`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Page {
    #[serde(default)]
    pub events: Vec<Event>,
    /// `NextToken` from the AWS CLI's own paging; `nextToken` from the API's,
    /// should a CLI ever pass it through.
    #[serde(rename = "NextToken", alias = "nextToken", default)]
    pub next: Option<String>,
}

/// Read one page.
pub fn page(reply: &[u8]) -> Result<Page, serde_json::Error> {
    serde_json::from_slice(reply)
}

/// How far behind the newest event a `--follow` poll starts.
///
/// Events reach CloudWatch from several control-plane instances, each with its
/// own delay, so an event stamped a few seconds before the newest one seen
/// can arrive after it. Starting each poll exactly at the newest timestamp
/// would skip it for good; starting a little before and dropping the repeats
/// costs one window of re-reading.
pub const LOOKBACK_MS: i64 = 30_000;

/// How often `--follow` polls.
pub const POLL: Duration = Duration::from_secs(5);

/// How often `--follow` lists the streams again, for a type read by name: a
/// control-plane instance replaced during an upgrade starts a new stream.
pub const RELIST: Duration = Duration::from_secs(60);

/// What has been printed, for dropping repeats as reads overlap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tail {
    /// Never ask for anything before this: the `--since` start.
    floor: i64,
    /// The newest timestamp printed.
    newest: Option<i64>,
    /// Every event printed inside the lookback window.
    seen: BTreeSet<(i64, String)>,
}

impl Tail {
    /// Nothing printed yet; reading starts at `start`.
    #[must_use]
    pub fn new(start: i64) -> Self {
        Self {
            floor: start,
            newest: None,
            seen: BTreeSet::new(),
        }
    }

    /// Where the next read starts.
    #[must_use]
    pub fn start(&self) -> i64 {
        self.newest
            .map_or(self.floor, |newest| newest.saturating_sub(LOOKBACK_MS))
            .max(self.floor)
    }

    /// Take a read's events: drop the ones already printed, put the rest in
    /// time order, and remember them until they fall out of the window.
    pub fn admit(&mut self, mut events: Vec<Event>) -> Vec<Event> {
        events.sort_by(|a, b| (a.timestamp, &a.id).cmp(&(b.timestamp, &b.id)));
        events.retain(|event| self.seen.insert((event.timestamp, event.id.clone())));
        if let Some(last) = events.last() {
            self.newest = Some(
                self.newest
                    .map_or(last.timestamp, |n| n.max(last.timestamp)),
            );
        }
        let start = self.start();
        self.seen.retain(|(timestamp, _)| *timestamp >= start);
        events
    }

    /// How many events are held for de-duplication: a bound, not a count of
    /// everything printed.
    #[cfg(test)]
    fn held(&self) -> usize {
        self.seen.len()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn target() -> Target {
        Target {
            cluster: "prod".to_owned(),
            region: "us-east-1".to_owned(),
            profile: Some("prod-admin".to_owned()),
            env: Vec::new(),
            program: "aws".to_owned(),
        }
    }

    fn event(timestamp: i64, id: &str) -> Event {
        Event {
            stream: "kube-apiserver-audit-1".to_owned(),
            timestamp,
            message: format!("event {id}"),
            id: id.to_owned(),
        }
    }

    fn stream(name: &str, last: Option<i64>) -> Stream {
        Stream {
            name: name.to_owned(),
            last_event: last,
            last_ingestion: None,
        }
    }

    // --- log types and their streams -------------------------------------

    #[test]
    fn every_type_round_trips_through_eks_s_own_name() {
        for kind in LogType::ALL {
            assert_eq!(LogType::from_eks(kind.eks_name()), Some(kind));
        }
        assert_eq!(LogType::from_eks("controller-manager"), None);
        assert_eq!(LogType::ControllerManager.flag(), "controller-manager");
    }

    #[test]
    fn the_flag_spellings_are_the_ones_clap_accepts() {
        use clap::ValueEnum;
        for kind in LogType::ALL {
            assert_eq!(LogType::from_str(kind.flag(), false), Ok(kind));
        }
    }

    #[test]
    fn audit_streams_are_never_read_as_the_api_server_s_log() {
        // `a` is a hex digit, so no prefix can tell `kube-apiserver-a1b2…`
        // from `kube-apiserver-audit-…`; only the whole word can.
        assert!(LogType::Api.owns("kube-apiserver-a1b2c3d4e5f60718293a4b5c6d7e8f90"));
        assert!(!LogType::Api.owns("kube-apiserver-audit-a1b2c3d4e5f60718293a4b5c6d7e8f90"));
        assert!(LogType::Audit.owns("kube-apiserver-audit-a1b2c3d4e5f60718293a4b5c6d7e8f90"));
        assert!(!LogType::Audit.owns("kube-apiserver-a1b2c3d4e5f60718293a4b5c6d7e8f90"));
    }

    #[test]
    fn the_controller_manager_includes_the_cloud_controller_manager() {
        assert!(LogType::ControllerManager.owns("kube-controller-manager-0a1b"));
        assert!(LogType::ControllerManager.owns("cloud-controller-manager-0a1b"));
        assert!(!LogType::Scheduler.owns("cloud-controller-manager-0a1b"));
    }

    #[test]
    fn a_type_read_by_prefix_owns_exactly_the_streams_its_prefix_matches() {
        let names = [
            "kube-apiserver-0a",
            "kube-apiserver-audit-0a",
            "authenticator-0a",
            "kube-controller-manager-0a",
            "cloud-controller-manager-0a",
            "kube-scheduler-0a",
        ];
        for kind in LogType::ALL {
            if let Selection::Prefix(prefix) = kind.selection() {
                for name in names {
                    assert_eq!(kind.owns(name), name.starts_with(prefix), "{kind} {name}");
                }
            }
        }
    }

    #[test]
    fn picking_streams_keeps_the_type_s_own_that_may_hold_recent_events() {
        let start = 10 * STREAM_LAG_MS;
        let streams = [
            stream("kube-apiserver-new", Some(start + 5)),
            stream("kube-apiserver-audit-new", Some(start + 5)),
            stream("kube-apiserver-lagging", Some(start - STREAM_LAG_MS / 2)),
            stream("kube-apiserver-retired", Some(start - 2 * STREAM_LAG_MS)),
            stream("kube-apiserver-unwritten", None),
        ];

        let (names, dropped) = pick_streams(LogType::Api, &streams, start);

        assert_eq!(
            names,
            [
                "kube-apiserver-unwritten",
                "kube-apiserver-new",
                "kube-apiserver-lagging"
            ]
        );
        assert_eq!(dropped, 0);
    }

    #[test]
    fn picking_streams_reads_the_ingestion_time_when_it_is_newer() {
        let mut lagging = stream("kube-scheduler-x", Some(0));
        lagging.last_ingestion = Some(10 * STREAM_LAG_MS);

        let (names, _) = pick_streams(LogType::Scheduler, &[lagging], 10 * STREAM_LAG_MS);

        assert_eq!(names, ["kube-scheduler-x"]);
    }

    #[test]
    fn picking_streams_stops_at_the_hundred_newest_and_counts_the_rest() {
        let streams: Vec<Stream> = (0..130)
            .map(|n| stream(&format!("kube-apiserver-{n:03}"), Some(1_000 + n)))
            .collect();

        let (names, dropped) = pick_streams(LogType::Api, &streams, 0);

        assert_eq!(names.len(), MAX_STREAMS);
        assert_eq!(dropped, 30);
        assert_eq!(names[0], "kube-apiserver-129");
    }

    #[test]
    fn a_group_with_none_of_the_type_s_streams_picks_nothing() {
        assert_eq!(pick_streams(LogType::Api, &[], 0), (Vec::new(), 0));
    }

    #[test]
    fn the_stream_listing_is_read_with_its_optional_times_absent() {
        let reply = br#"{"logStreams": [
            {"logStreamName": "kube-apiserver-1", "creationTime": 1, "lastEventTimestamp": 5, "lastIngestionTime": 6},
            {"logStreamName": "kube-apiserver-2", "creationTime": 1}
        ]}"#;
        assert_eq!(
            streams(reply).unwrap(),
            [
                Stream {
                    name: "kube-apiserver-1".to_owned(),
                    last_event: Some(5),
                    last_ingestion: Some(6)
                },
                stream("kube-apiserver-2", None)
            ]
        );
        assert_eq!(streams(b"{}").unwrap(), []);
    }

    // --- not enabled -----------------------------------------------------

    #[test]
    fn a_type_that_is_off_is_reported_with_the_command_and_the_cost() {
        let message = not_enabled(
            LogType::Audit,
            &[LogType::Api, LogType::Authenticator],
            &target(),
            "prod (us-east-1)",
        );

        assert!(
            message.starts_with("prod (us-east-1) does not send its audit log to CloudWatch."),
            "{message}"
        );
        assert!(
            message.contains("Switched on now: api and authenticator (read it with `--type api`)."),
            "{message}"
        );
        assert!(message.contains("eks never switches one on"), "{message}");
        assert!(message.contains("CloudWatch charges"), "{message}");
        assert!(message.contains("usually the largest"), "{message}");
        assert!(
            message.contains(&target().enable_command(LogType::Audit)),
            "{message}"
        );
    }

    #[test]
    fn a_cluster_with_nothing_on_says_so_rather_than_listing_nothing() {
        let message = not_enabled(LogType::Scheduler, &[], &target(), "prod");

        assert!(
            message.contains("No control-plane log type is switched on."),
            "{message}"
        );
        assert!(!message.contains("largest"), "{message}");
        assert!(message.contains("\"scheduler\""), "{message}");
    }

    // --- since -----------------------------------------------------------

    #[test]
    fn since_reads_a_length_of_time_in_any_of_four_units() {
        assert_eq!("45s".parse(), Ok(Since::Ago(Duration::from_secs(45))));
        assert_eq!("15m".parse(), Ok(Since::Ago(Duration::from_secs(900))));
        assert_eq!("2h".parse(), Ok(Since::Ago(Duration::from_secs(7_200))));
        assert_eq!("3d".parse(), Ok(Since::Ago(Duration::from_secs(259_200))));
    }

    #[test]
    fn since_reads_an_instant() {
        let since: Since = "2026-10-07T05:00:00Z".parse().unwrap();
        assert_eq!(since, Since::At("2026-10-07T05:00:00Z".parse().unwrap()));
        assert_eq!(since.phrase(), "since 2026-10-07T05:00:00Z");
    }

    #[test]
    fn since_refuses_nothing_zero_and_units_it_does_not_know() {
        for bad in [
            "",
            "0",
            "0m",
            "5",
            "5w",
            "m",
            "-5m",
            "1h30m",
            "9999999999999999999d",
        ] {
            assert_eq!(bad.parse::<Since>(), Err(SinceError), "{bad:?}");
        }
        assert!(SinceError.to_string().contains("`15m`"));
    }

    #[test]
    fn since_prints_the_spelling_it_reads_back() {
        for spelling in ["45s", "90s", "15m", "2h", "36h", "3d"] {
            let since: Since = spelling.parse().unwrap();
            assert_eq!(since.to_string(), spelling);
        }
        assert_eq!(Since::default().to_string(), "1h");
        assert_eq!(Since::default().phrase(), "in the last 1h");
    }

    #[test]
    fn since_counts_back_from_the_moment_given() {
        let now: Timestamp = "2026-10-07T06:00:00Z".parse().unwrap();
        assert_eq!(
            Since::default().start(now),
            "2026-10-07T05:00:00Z".parse::<Timestamp>().unwrap()
        );
        assert_eq!(
            Since::Ago(Duration::from_secs(u64::MAX)).start(now),
            Timestamp::MIN
        );
    }

    #[test]
    fn event_times_read_to_the_second() {
        assert_eq!(event_time(1_791_354_062_123), "2026-10-07T06:21:02Z");
        assert_eq!(
            millis("2026-10-07T06:21:02.123Z".parse().unwrap()),
            1_791_354_062_123
        );
    }

    // --- the calls -------------------------------------------------------

    #[test]
    fn a_page_by_prefix_names_the_group_the_start_and_the_page_size() {
        let scope = Scope::Prefix("kube-apiserver-audit-");
        let query = Query {
            kind: LogType::Audit,
            scope: &scope,
            start: 1_000,
            grep: None,
        };

        let call = filter_events(&target(), &query, None);

        assert_eq!(
            call.line(),
            "aws logs filter-log-events --log-group-name /aws/eks/prod/cluster --start-time 1000 \
             --log-stream-name-prefix kube-apiserver-audit- --max-items 5000 --region us-east-1 \
             --profile prod-admin"
        );
        assert_eq!(call.action, "logs:FilterLogEvents");
    }

    #[test]
    fn a_later_page_carries_its_token_and_a_listed_type_names_its_streams() {
        let scope = Scope::Named(vec![
            "kube-apiserver-1".to_owned(),
            "kube-apiserver-2".to_owned(),
        ]);
        let query = Query {
            kind: LogType::Api,
            scope: &scope,
            start: 1_000,
            grep: Some("pod \"api\""),
        };

        let argv = filter_events(&target(), &query, Some("tok==")).argv;

        let names = argv.iter().position(|a| a == "--log-stream-names").unwrap();
        assert_eq!(
            argv[names + 1..names + 3],
            ["kube-apiserver-1", "kube-apiserver-2"]
        );
        let pattern = argv.iter().position(|a| a == "--filter-pattern").unwrap();
        assert_eq!(argv[pattern + 1], r#""pod \"api\"""#);
        let token = argv.iter().position(|a| a == "--starting-token").unwrap();
        assert_eq!(argv[token + 1], "tok==");
    }

    #[test]
    fn the_stream_listing_covers_the_whole_group() {
        assert_eq!(
            describe_streams(&target()).argv[..5],
            [
                "aws",
                "logs",
                "describe-log-streams",
                "--log-group-name",
                "/aws/eks/prod/cluster"
            ]
        );
    }

    #[test]
    fn grep_is_a_plain_case_sensitive_substring() {
        assert!(matches("deleted pod api-7f9", Some("pod api")));
        assert!(!matches("deleted pod api-7f9", Some("Pod")));
        assert!(matches("anything", None));
        assert_eq!(pattern(r"a\b"), r#""a\\b""#);
    }

    // --- pages -----------------------------------------------------------

    #[test]
    fn a_page_is_read_with_the_cli_s_token_or_the_api_s() {
        let cli = br#"{"events": [{"logStreamName": "s", "timestamp": 5, "message": "m", "ingestionTime": 6, "eventId": "e"}],
                       "searchedLogStreams": [], "NextToken": "abc"}"#;
        let page = page(cli).unwrap();
        assert_eq!(
            page.events,
            [Event {
                stream: "s".to_owned(),
                timestamp: 5,
                message: "m".to_owned(),
                id: "e".to_owned()
            }]
        );
        assert_eq!(page.next.as_deref(), Some("abc"));

        let api = br#"{"events": [], "nextToken": "def"}"#;
        assert_eq!(super::page(api).unwrap().next.as_deref(), Some("def"));

        let last = br#"{"events": []}"#;
        assert_eq!(super::page(last).unwrap().next, None);
    }

    #[test]
    fn a_page_that_is_not_json_is_an_error() {
        assert!(page(b"<html>").is_err());
    }

    // --- tail ------------------------------------------------------------

    #[test]
    fn the_first_read_starts_at_since_and_comes_back_in_time_order() {
        let mut tail = Tail::new(1_000);
        assert_eq!(tail.start(), 1_000);

        let shown = tail.admit(vec![
            event(3_000, "b"),
            event(2_000, "a"),
            event(3_000, "a"),
        ]);

        let ids: Vec<(i64, &str)> = shown.iter().map(|e| (e.timestamp, e.id.as_str())).collect();
        assert_eq!(ids, [(2_000, "a"), (3_000, "a"), (3_000, "b")]);
    }

    #[test]
    fn a_poll_starts_a_little_before_the_newest_event_and_drops_repeats() {
        let newest = 100_000;
        let mut tail = Tail::new(0);
        tail.admit(vec![event(newest - 10_000, "a"), event(newest, "b")]);
        assert_eq!(tail.start(), newest - LOOKBACK_MS);

        // The overlap comes back, plus one late arrival stamped before the
        // newest event and one genuinely new event.
        let shown = tail.admit(vec![
            event(newest - 10_000, "a"),
            event(newest, "b"),
            event(newest - 5_000, "late"),
            event(newest + 1, "c"),
        ]);

        let ids: Vec<&str> = shown.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["late", "c"]);
    }

    #[test]
    fn a_poll_never_reaches_back_before_since() {
        let mut tail = Tail::new(50_000);
        tail.admit(vec![event(55_000, "a")]);
        assert_eq!(tail.start(), 50_000);
    }

    #[test]
    fn a_poll_with_nothing_new_changes_nothing() {
        let mut tail = Tail::new(0);
        tail.admit(vec![event(100_000, "a")]);
        let before = tail.clone();

        assert_eq!(tail.admit(Vec::new()), []);
        assert_eq!(tail, before);
    }

    #[test]
    fn the_events_held_for_de_duplication_stay_inside_the_window() {
        let mut tail = Tail::new(0);
        for second in 0..600 {
            tail.admit(vec![event(second * 1_000, &second.to_string())]);
        }
        // Thirty seconds of one event a second, plus the one at the edge.
        assert_eq!(tail.held(), 31);
    }
}
