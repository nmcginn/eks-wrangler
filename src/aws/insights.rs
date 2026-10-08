//! Container logs in CloudWatch, for a pod the cluster no longer has.
//!
//! `kubectl logs` reads from the kubelet, so a pod that has been deleted or
//! rescheduled takes its log with it, and that pod is usually the one being
//! troubleshot. Clusters with Container Insights (the Amazon CloudWatch
//! Observability add-on, or Fluent Bit set up by hand) also ship every
//! container's lines to one CloudWatch group,
//! `/aws/containerinsights/<cluster>/application`, one JSON record per line:
//!
//! ```json
//! {"time": "2026-10-07T06:21:02.123456789Z", "stream": "stdout", "log": "GET /healthz 200\n",
//!  "kubernetes": {"pod_name": "api-7d9f-xk2", "namespace_name": "shop",
//!                 "container_name": "app", "docker_id": "3f2a…", "host": "ip-10-0-1-2…"}}
//! ```
//!
//! This module holds every decision `eks logs` makes about that group, and
//! all of it is pure: which group ([`group`]), the filter pattern
//! ([`pattern`]), reading a record ([`record`]), which pod, container, and
//! instance a typed prefix means ([`resolve`]), how a line is printed
//! ([`line()`]), and what is said when there is nothing to print.
//! `commands::logs` does the running.
//!
//! The stream names cannot narrow the read: Fluent Bit names a stream after
//! the node first and the pod second, so no prefix picks out one pod. The
//! filter pattern does the narrowing instead, on the server.

use std::collections::BTreeMap;

use k8s_openapi::jiff::Timestamp;
use serde::Deserialize;

use crate::aws::cli::Call;
use crate::aws::eks::Target;
use crate::aws::logs::{Event, Since, event_time};
use crate::format::{self, Cell};
use crate::k8s::client;
use crate::theme::{Palette, Severity};

/// The group Container Insights writes application logs to, with
/// `{cluster}` standing for the cluster's name. `log_group` in `config.toml`
/// replaces it, in the same spelling.
pub const DEFAULT_GROUP: &str = "/aws/containerinsights/{cluster}/application";

/// Where to read how to set Container Insights up.
pub const SETUP_URL: &str = "https://docs.aws.amazon.com/AmazonCloudWatch/latest/monitoring/install-CloudWatch-Observability-EKS-addon.html";

/// The group to read: `template`, or [`DEFAULT_GROUP`], with `{cluster}`
/// filled in. A template without `{cluster}` is used as written, so one
/// group shared by every cluster can be named too.
#[must_use]
pub fn group(template: Option<&str>, cluster: &str) -> String {
    template
        .unwrap_or(DEFAULT_GROUP)
        .replace("{cluster}", cluster)
}

/// Whether `text` could be part of a pod or namespace name: lower-case
/// letters, digits, `-`, and `.`.
///
/// Anything else cannot match a pod, and would need escaping inside a filter
/// pattern, so it is refused before CloudWatch is asked anything.
#[must_use]
pub fn is_name(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
}

/// The filter pattern for every line from a pod in `namespace` whose name
/// starts with `prefix`, or `None` when either cannot be a name.
///
/// A prefix rather than the exact name, so that what was typed is matched by
/// [`resolve`] with `eks exec`'s rules, an exact name winning. Every
/// container is read, not only the one asked for, so a container that is not
/// there can be answered with the ones that are.
#[must_use]
pub fn pattern(namespace: &str, prefix: &str) -> Option<String> {
    (is_name(namespace) && is_name(prefix)).then(|| {
        format!(
            "{{ $.kubernetes.namespace_name = \"{namespace}\" && \
             $.kubernetes.pod_name = \"{prefix}*\" }}"
        )
    })
}

/// `aws logs filter-log-events` for one page of a pod's lines.
#[must_use]
pub fn filter_events(
    target: &Target,
    group: &str,
    pattern: &str,
    start: i64,
    token: Option<&str>,
) -> Call {
    let mut extra = vec![
        "--log-group-name".to_owned(),
        group.to_owned(),
        "--start-time".to_owned(),
        start.to_string(),
        "--filter-pattern".to_owned(),
        pattern.to_owned(),
        "--max-items".to_owned(),
        crate::aws::logs::PAGE_ITEMS.to_string(),
    ];
    if let Some(token) = token {
        extra.extend(["--starting-token".to_owned(), token.to_owned()]);
    }
    target.call(
        &["logs", "filter-log-events"],
        extra,
        "logs:FilterLogEvents",
    )
}

/// One container's line, read out of a CloudWatch event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Milliseconds since the epoch: the event's CloudWatch timestamp.
    pub timestamp: i64,
    /// CloudWatch's event ID, for dropping repeats as `--follow` polls overlap.
    pub id: String,
    pub namespace: String,
    pub pod: String,
    pub container: String,
    /// The container instance, `docker_id`: a restart starts a new one.
    /// Empty when the record does not say.
    pub instance: String,
    /// `stdout` or `stderr`; empty when the record does not say.
    pub stream: String,
    /// The line, without the newline the container printed.
    pub log: String,
}

#[derive(Debug, Deserialize)]
struct Raw {
    #[serde(default)]
    log: Option<String>,
    #[serde(default)]
    stream: Option<String>,
    kubernetes: Meta,
}

#[derive(Debug, Deserialize)]
struct Meta {
    pod_name: String,
    #[serde(default)]
    namespace_name: String,
    #[serde(default)]
    container_name: String,
    #[serde(default)]
    docker_id: String,
}

/// Read one event as a container's line, or `None` when it is not one: not
/// JSON, or JSON without the pod it came from.
#[must_use]
pub fn record(event: &Event) -> Option<Record> {
    let raw: Raw = serde_json::from_str(&event.message).ok()?;
    let log = raw.log.unwrap_or_default();
    Some(Record {
        timestamp: event.timestamp,
        id: event.id.clone(),
        namespace: raw.kubernetes.namespace_name,
        pod: raw.kubernetes.pod_name,
        container: raw.kubernetes.container_name,
        instance: raw.kubernetes.docker_id,
        stream: raw.stream.unwrap_or_default(),
        log: log.trim_end_matches(['\n', '\r']).to_owned(),
    })
}

/// A pod that CloudWatch has lines from, and when its last one was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub pod: String,
    /// Milliseconds since the epoch.
    pub last: i64,
}

/// What `eks logs` was asked for, as far as CloudWatch is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wanted<'a> {
    /// The pod: a full name or a prefix.
    pub pod: &'a str,
    /// `--container`.
    pub container: Option<&'a str>,
    /// `--previous`.
    pub previous: bool,
}

/// One container instance's lines: what [`resolve`] chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub pod: String,
    pub container: String,
    /// The instance chosen with `--previous`; `None` reads every instance,
    /// in time order, as `kubectl logs` reads the one it has.
    pub instance: Option<String>,
    /// How many instances of the container the window held.
    pub instances: usize,
    /// The lines, in time order.
    pub lines: Vec<Record>,
}

impl Found {
    /// Whether a record read later, by a `--follow` poll, belongs here.
    #[must_use]
    pub fn admits(&self, record: &Record) -> bool {
        record.pod == self.pod
            && record.container == self.container
            && self
                .instance
                .as_ref()
                .is_none_or(|instance| *instance == record.instance)
    }
}

/// Why [`resolve`] could not choose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unresolved {
    /// No line from any pod starting with the prefix.
    Nothing,
    /// Lines from several pods that start with it, none called exactly it.
    /// Most recently heard from first.
    SeveralPods(Vec<Seen>),
    /// The pod had several containers and none was named.
    SeveralContainers {
        pod: String,
        containers: Vec<String>,
    },
    /// `--container` named one the pod's lines do not come from.
    NoSuchContainer {
        pod: String,
        wanted: String,
        containers: Vec<String>,
    },
    /// `--previous`, and the window holds only one instance of the container.
    NoPrevious { pod: String, container: String },
}

/// Choose one pod, one container, and with `--previous` one instance, from
/// the records a read turned up, by `eks exec`'s rules as far as CloudWatch
/// can follow them.
///
/// - **Pod:** an exact name wins over longer ones it starts; otherwise the
///   prefix must be the start of exactly one.
/// - **Container:** the one named; else the only one. CloudWatch keeps no pod
///   spec, so the `default-container` annotation cannot be read, and several
///   containers with none named is a question rather than a guess.
/// - **Instance:** every one, in time order, which is what a gone pod's
///   history is. With `--previous`, the one before the last.
pub fn resolve(mut records: Vec<Record>, wanted: Wanted<'_>) -> Result<Found, Unresolved> {
    records.retain(|record| record.pod.starts_with(wanted.pod) && !wanted.pod.is_empty());
    records.sort_by(|a, b| (a.timestamp, &a.id).cmp(&(b.timestamp, &b.id)));

    let mut pods: BTreeMap<&str, i64> = BTreeMap::new();
    for record in &records {
        let last = pods.entry(record.pod.as_str()).or_insert(record.timestamp);
        *last = (*last).max(record.timestamp);
    }
    let pod = if pods.contains_key(wanted.pod) {
        wanted.pod.to_owned()
    } else {
        match pods.len() {
            0 => return Err(Unresolved::Nothing),
            1 => pods
                .keys()
                .next()
                .map(|pod| (*pod).to_owned())
                .unwrap_or_default(),
            _ => {
                let mut seen: Vec<Seen> = pods
                    .iter()
                    .map(|(pod, last)| Seen {
                        pod: (*pod).to_owned(),
                        last: *last,
                    })
                    .collect();
                seen.sort_by(|a, b| b.last.cmp(&a.last).then_with(|| a.pod.cmp(&b.pod)));
                return Err(Unresolved::SeveralPods(seen));
            }
        }
    };
    records.retain(|record| record.pod == pod);

    let mut containers: Vec<String> = records
        .iter()
        .map(|record| record.container.clone())
        .collect();
    containers.sort();
    containers.dedup();
    let container = match (wanted.container, containers.as_slice()) {
        (Some(named), _) if containers.iter().any(|c| c == named) => named.to_owned(),
        (Some(named), _) => {
            return Err(Unresolved::NoSuchContainer {
                pod,
                wanted: named.to_owned(),
                containers,
            });
        }
        (None, [only]) => only.clone(),
        (None, _) => return Err(Unresolved::SeveralContainers { pod, containers }),
    };
    records.retain(|record| record.container == container);

    // In the order each instance first spoke, which is the order they ran.
    let mut instances: Vec<&str> = Vec::new();
    for record in &records {
        if !instances.contains(&record.instance.as_str()) {
            instances.push(&record.instance);
        }
    }
    let count = instances.len();
    let instance = if wanted.previous {
        match instances
            .len()
            .checked_sub(2)
            .and_then(|i| instances.get(i))
        {
            Some(previous) => Some((*previous).to_owned()),
            None => return Err(Unresolved::NoPrevious { pod, container }),
        }
    } else {
        None
    };
    if let Some(instance) = &instance {
        records.retain(|record| record.instance == *instance);
    }

    Ok(Found {
        pod,
        container,
        instance,
        instances: count,
        lines: records,
    })
}

/// One line as it is printed, labelled as CloudWatch's so a reader never
/// mistakes it for the kubelet's: `[cloudwatch 2026-10-07T06:21:02Z] GET /`.
///
/// `stderr` joins the label when the record says so, since the two streams
/// are interleaved here as `kubectl logs` interleaves them.
#[must_use]
pub fn line(record: &Record, palette: Palette) -> String {
    let stream = if record.stream == "stderr" {
        " stderr"
    } else {
        ""
    };
    let label = format!("[cloudwatch {}{stream}]", event_time(record.timestamp));
    let label = palette.paint(&label, Severity::Unknown);
    if record.log.is_empty() {
        label.into_owned()
    } else {
        format!("{label} {}", record.log)
    }
}

/// The note printed before CloudWatch's lines, on stderr: what is gone, and
/// where its lines are coming from instead.
///
/// Says how far back it reads, and with no `--since` how to reach further:
/// an hour of a pod's lines must not pass for the whole of them.
#[must_use]
pub fn reading_note(
    wanted: &str,
    namespace: &str,
    found: &Found,
    group: &str,
    since: Since,
    since_given: bool,
) -> String {
    let what = if found.pod == wanted {
        format!("No pod called {wanted} is running in namespace {namespace}")
    } else {
        format!(
            "No pod starting {wanted:?} is running in namespace {namespace}; {} was",
            found.pod
        )
    };
    let instance = match (&found.instance, found.instances) {
        (Some(_), _) => " (its previous instance)".to_owned(),
        (None, n) if n > 1 => format!(" ({n} instances, in time order)"),
        _ => String::new(),
    };
    let further = if since_given {
        String::new()
    } else {
        format!(
            " Older lines need `--since`, e.g. `--since {}`.",
            further(since)
        )
    };
    format!(
        "{what}. Reading {container}'s lines {phrase}{instance} from CloudWatch, {group}.\
         {further}",
        container = found.container,
        phrase = since.phrase(),
    )
}

/// Everything about a read that found nothing to print.
#[derive(Debug, Clone, Copy)]
pub struct Search<'a> {
    pub wanted: Wanted<'a>,
    pub namespace: &'a str,
    pub since: Since,
    pub group: &'a str,
    /// Live pods elsewhere in the cluster whose names start with the
    /// prefix, as `namespace/name`: the likelier answer when CloudWatch
    /// has nothing.
    pub elsewhere: &'a [String],
    /// `-l` or `--field-selector` narrowed the live search. CloudWatch's
    /// records carry no labels, so a selector may be why the live pod was
    /// missed.
    pub selected: bool,
}

/// The sentence for an [`Unresolved`] read: what was looked for, what was
/// found instead, and what to type next.
#[must_use]
pub fn explain(why: &Unresolved, search: &Search<'_>, now: Timestamp) -> String {
    let Search {
        wanted, namespace, ..
    } = *search;
    let pod = wanted.pod;
    match why {
        Unresolved::Nothing => nothing(search),
        Unresolved::SeveralPods(seen) => {
            let rows: Vec<Vec<Cell>> = seen
                .iter()
                .map(|seen| {
                    vec![
                        Cell::plain(seen.pod.clone()),
                        Cell::plain(last_heard(seen.last, now)),
                    ]
                })
                .collect();
            let table = format::table(&["POD", "LAST LINE"], &rows, Palette::Plain);
            format!(
                "no pod starting {pod:?} is running in namespace {namespace}, and CloudWatch has \
                 lines from {count} that did {since}:\n\n{table}\n\n\
                 Type more of the name to pick one, e.g. `eks logs {example}`.",
                count = seen.len(),
                since = search.since.phrase(),
                example = seen.first().map_or(pod, |seen| seen.pod.as_str()),
            )
        }
        Unresolved::SeveralContainers { pod, containers } => format!(
            "pod {pod} is gone, and CloudWatch has lines from {count} of its containers; it \
             keeps no record of which is the default.\nPick one with `--container`: {list}.",
            count = containers.len(),
            list = names(containers, "or"),
        ),
        Unresolved::NoSuchContainer {
            pod,
            wanted,
            containers,
        } => format!(
            "pod {pod} is gone, and CloudWatch has no lines from a container called {wanted:?} \
             in it {since}.\nIts lines there come from {list}; pass one of those to \
             `--container`.",
            since = search.since.phrase(),
            list = names(containers, "and"),
        ),
        Unresolved::NoPrevious { pod, container } => format!(
            "pod {pod} is gone, and CloudWatch holds lines from only one instance of {container} \
             {since}, so there is no previous one to show.\nDrop `--previous` to read that \
             instance, or reach further back with a longer `--since`.",
            since = search.since.phrase(),
        ),
    }
}

/// No live pod, and no line in CloudWatch either.
fn nothing(search: &Search<'_>) -> String {
    let Search {
        wanted,
        namespace,
        since,
        group,
        elsewhere,
        selected,
    } = *search;
    let head = format!(
        "no pod in namespace {namespace} is called {pod:?} or starts with it, and CloudWatch \
         ({group}) has no lines from one {phrase}.",
        pod = wanted.pod,
        phrase = since.phrase(),
    );
    match elsewhere {
        [] => {}
        [only] => {
            let namespace = only.split('/').next().unwrap_or_default();
            return format!(
                "{head}\nThere is a running one in namespace {namespace}: pass \
                 `-n {namespace}` to read it."
            );
        }
        several => {
            return format!(
                "{head}\nRunning pods in other namespaces start with it: {}. Pass \
                 `-n <namespace>` with the one you meant.",
                names(several, "and"),
            );
        }
    }
    let selector = if selected {
        "\n`-l` and `--field-selector` narrowed the running pods searched; drop them to \
         search them all."
    } else {
        ""
    };
    format!(
        "{head}{selector}\nIf it ran before that, reach further back, e.g. `--since {}`. \
         Run `eks pods -n {namespace}` to see what is running now.",
        further(since),
    )
}

/// A `--since` worth trying after `since` found nothing: a day, or a week
/// once a day has been tried.
fn further(since: Since) -> &'static str {
    match since {
        Since::Ago(span) if span.as_secs() < 86_400 => "1d",
        _ => "7d",
    }
}

/// The names in a sentence.
fn names(names: &[String], conjunction: &str) -> String {
    format::list(names, conjunction).unwrap_or_else(|| "none".to_owned())
}

/// When a pod was last heard from: `4m ago`.
fn last_heard(last: i64, now: Timestamp) -> String {
    Timestamp::from_millisecond(last).map_or_else(
        |_| event_time(last),
        |at| format!("{} ago", format::human_duration(now.duration_since(at))),
    )
}

/// The group does not exist: Container Insights is not set up, or `log_group`
/// names the wrong one. Says how to set it up, and that the lines a pod
/// printed before then are not coming back.
#[must_use]
pub fn not_set_up(group: &str, overridden: bool, target: &Target, label: &str) -> String {
    if overridden {
        return format!(
            "the log group {group}, which `log_group` in config.toml names, does not exist in \
             {region}.\nCheck the name with `aws logs describe-log-groups --region {region}`, \
             and fix or remove `log_group` in ~/.config/eks/config.toml.",
            region = target.region,
        );
    }
    let mut command = format!(
        "aws eks create-addon --cluster-name {} --addon-name amazon-cloudwatch-observability \
         --region {}",
        client::shell_word(&target.cluster),
        client::shell_word(&target.region),
    );
    if let Some(profile) = &target.profile {
        command.push_str(" --profile ");
        command.push_str(&client::shell_word(profile));
    }
    format!(
        "{label} does not send container logs to CloudWatch: there is no log group {group}, \
         so the lines of a pod that is gone cannot be read.\n\
         Container Insights is what sends them. To set it up (lines are kept from then on, \
         not before), install the CloudWatch Observability add-on:\n  {command}\n\
         Its nodes need the CloudWatchAgentServerPolicy; see {SETUP_URL}\n\
         If this cluster's logs go to another group, name it as `log_group` in \
         ~/.config/eks/config.toml."
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::time::Duration;

    use super::*;
    use crate::theme::Theme;

    /// 2026-10-07T06:21:02Z.
    const T0: i64 = 1_791_354_062_000;

    fn target() -> Target {
        Target {
            cluster: "prod".to_owned(),
            region: "us-east-1".to_owned(),
            profile: Some("prod-admin".to_owned()),
            env: Vec::new(),
            program: "aws".to_owned(),
        }
    }

    /// A Container Insights record, as Fluent Bit writes it.
    fn message(pod: &str, container: &str, instance: &str, stream: &str, log: &str) -> String {
        serde_json::json!({
            "time": "2026-10-07T06:21:02.123456789Z",
            "stream": stream,
            "_p": "F",
            "log": format!("{log}\n"),
            "kubernetes": {
                "pod_name": pod,
                "namespace_name": "shop",
                "pod_id": "9b1c",
                "host": "ip-10-0-1-2.ec2.internal",
                "container_name": container,
                "docker_id": instance,
                "container_image": "app:1"
            }
        })
        .to_string()
    }

    fn rec(at: i64, pod: &str, container: &str, instance: &str, log: &str) -> Record {
        record(&Event {
            stream: "ip-10-0-1-2.ec2.internal-application.var.log.containers.x.log".to_owned(),
            timestamp: at,
            message: message(pod, container, instance, "stdout", log),
            id: format!("{at}-{log}"),
        })
        .unwrap()
    }

    fn wanted(pod: &str) -> Wanted<'_> {
        Wanted {
            pod,
            container: None,
            previous: false,
        }
    }

    fn search<'a>(wanted: Wanted<'a>, elsewhere: &'a [String]) -> Search<'a> {
        Search {
            wanted,
            namespace: "shop",
            since: Since::default(),
            group: "/aws/containerinsights/prod/application",
            elsewhere,
            selected: false,
        }
    }

    fn now() -> Timestamp {
        Timestamp::from_millisecond(T0 + 300_000).unwrap()
    }

    // --- the group and the pattern -------------------------------------------

    #[test]
    fn the_group_is_container_insights_own_unless_config_names_another() {
        assert_eq!(
            group(None, "prod"),
            "/aws/containerinsights/prod/application"
        );
        assert_eq!(group(Some("/eks/{cluster}/pods"), "prod"), "/eks/prod/pods");
        assert_eq!(group(Some("/shared/pods"), "prod"), "/shared/pods");
    }

    #[test]
    fn the_pattern_matches_the_namespace_and_the_start_of_the_pod_s_name() {
        assert_eq!(
            pattern("shop", "api-7d9f").as_deref(),
            Some(
                "{ $.kubernetes.namespace_name = \"shop\" && \
                 $.kubernetes.pod_name = \"api-7d9f*\" }"
            )
        );
    }

    #[test]
    fn text_that_cannot_be_a_name_is_never_put_in_a_pattern() {
        for bad in ["", "API", "api\"", "a b", "api*", "x}"] {
            assert_eq!(pattern("shop", bad), None, "{bad:?}");
        }
        assert_eq!(pattern("Shop", "api"), None);
        assert!(is_name("db-0.primary"));
    }

    #[test]
    fn a_page_names_the_group_the_pattern_the_start_and_the_token() {
        let call = filter_events(
            &target(),
            "/aws/containerinsights/prod/application",
            "{ x }",
            1_000,
            Some("tok"),
        );

        assert_eq!(
            call.line(),
            "aws logs filter-log-events --log-group-name /aws/containerinsights/prod/application \
             --start-time 1000 --filter-pattern '{ x }' --max-items 5000 --starting-token tok \
             --region us-east-1 --profile prod-admin"
        );
        assert_eq!(call.action, "logs:FilterLogEvents");
    }

    // --- records -------------------------------------------------------------

    #[test]
    fn a_fluent_bit_record_is_read_without_its_trailing_newline() {
        let record = rec(T0, "api-1", "app", "3f2a", "GET / 200");

        assert_eq!(record.pod, "api-1");
        assert_eq!(record.namespace, "shop");
        assert_eq!(record.container, "app");
        assert_eq!(record.instance, "3f2a");
        assert_eq!(record.stream, "stdout");
        assert_eq!(record.log, "GET / 200");
        assert_eq!(record.timestamp, T0);
    }

    #[test]
    fn a_record_missing_its_optional_fields_still_reads() {
        let event = Event {
            stream: "s".to_owned(),
            timestamp: T0,
            message: r#"{"kubernetes": {"pod_name": "api-1"}}"#.to_owned(),
            id: "e".to_owned(),
        };

        let record = record(&event).unwrap();

        assert_eq!(record.pod, "api-1");
        assert_eq!(record.container, "");
        assert_eq!(record.instance, "");
        assert_eq!(record.log, "");
    }

    #[test]
    fn an_event_that_is_not_a_container_s_line_is_not_a_record() {
        for message in [
            "plain text",
            "{}",
            r#"{"log": "x"}"#,
            r#"{"kubernetes": {}}"#,
        ] {
            let event = Event {
                stream: "s".to_owned(),
                timestamp: T0,
                message: message.to_owned(),
                id: "e".to_owned(),
            };
            assert_eq!(record(&event), None, "{message}");
        }
    }

    // --- resolving -----------------------------------------------------------

    #[test]
    fn a_prefix_of_one_pod_reads_that_pod_in_time_order() {
        let records = vec![
            rec(T0 + 2, "api-7d9f-xk2", "app", "a", "second"),
            rec(T0 + 1, "api-7d9f-xk2", "app", "a", "first"),
            rec(T0 + 3, "worker-1", "app", "w", "not this pod"),
        ];

        let found = resolve(records, wanted("api")).unwrap();

        assert_eq!(found.pod, "api-7d9f-xk2");
        assert_eq!(found.container, "app");
        let logs: Vec<&str> = found.lines.iter().map(|r| r.log.as_str()).collect();
        assert_eq!(logs, ["first", "second"]);
    }

    #[test]
    fn an_exact_name_wins_over_longer_names_it_starts() {
        let records = vec![
            rec(T0, "api", "app", "a", "short"),
            rec(T0 + 1, "api-canary", "app", "b", "long"),
        ];

        let found = resolve(records, wanted("api")).unwrap();

        assert_eq!(found.pod, "api");
        assert_eq!(found.lines.len(), 1);
    }

    #[test]
    fn a_prefix_of_several_pods_lists_them_most_recent_first() {
        let records = vec![
            rec(T0, "api-1", "app", "a", "x"),
            rec(T0 + 5_000, "api-2", "app", "b", "y"),
            rec(T0 + 1_000, "api-1", "app", "a", "z"),
        ];

        let why = resolve(records, wanted("api")).unwrap_err();

        assert_eq!(
            why,
            Unresolved::SeveralPods(vec![
                Seen {
                    pod: "api-2".to_owned(),
                    last: T0 + 5_000
                },
                Seen {
                    pod: "api-1".to_owned(),
                    last: T0 + 1_000
                },
            ])
        );
    }

    #[test]
    fn no_records_is_nothing_and_an_empty_prefix_matches_nothing() {
        assert_eq!(resolve(Vec::new(), wanted("api")), Err(Unresolved::Nothing));
        assert_eq!(
            resolve(vec![rec(T0, "api-1", "app", "a", "x")], wanted("")),
            Err(Unresolved::Nothing)
        );
    }

    #[test]
    fn several_containers_with_none_named_is_a_question() {
        let records = vec![
            rec(T0, "api-1", "app", "a", "x"),
            rec(T0 + 1, "api-1", "istio-proxy", "p", "y"),
        ];

        assert_eq!(
            resolve(records, wanted("api-1")),
            Err(Unresolved::SeveralContainers {
                pod: "api-1".to_owned(),
                containers: vec!["app".to_owned(), "istio-proxy".to_owned()],
            })
        );
    }

    #[test]
    fn a_named_container_reads_only_its_own_lines() {
        let records = vec![
            rec(T0, "api-1", "app", "a", "app line"),
            rec(T0 + 1, "api-1", "istio-proxy", "p", "proxy line"),
        ];

        let found = resolve(
            records,
            Wanted {
                container: Some("istio-proxy"),
                ..wanted("api-1")
            },
        )
        .unwrap();

        assert_eq!(found.container, "istio-proxy");
        let logs: Vec<&str> = found.lines.iter().map(|r| r.log.as_str()).collect();
        assert_eq!(logs, ["proxy line"]);
    }

    #[test]
    fn a_container_the_lines_do_not_come_from_names_the_ones_they_do() {
        let records = vec![rec(T0, "api-1", "app", "a", "x")];

        assert_eq!(
            resolve(
                records,
                Wanted {
                    container: Some("sidecar"),
                    ..wanted("api-1")
                }
            ),
            Err(Unresolved::NoSuchContainer {
                pod: "api-1".to_owned(),
                wanted: "sidecar".to_owned(),
                containers: vec!["app".to_owned()],
            })
        );
    }

    #[test]
    fn every_instance_is_read_in_time_order_and_counted() {
        let records = vec![
            rec(T0, "api-1", "app", "first", "boot"),
            rec(T0 + 1, "api-1", "app", "first", "crash"),
            rec(T0 + 2, "api-1", "app", "second", "boot again"),
        ];

        let found = resolve(records, wanted("api-1")).unwrap();

        assert_eq!(found.instance, None);
        assert_eq!(found.instances, 2);
        assert_eq!(found.lines.len(), 3);
    }

    #[test]
    fn previous_reads_the_instance_before_the_last() {
        let records = vec![
            rec(T0, "api-1", "app", "first", "one"),
            rec(T0 + 1, "api-1", "app", "second", "two"),
            rec(T0 + 2, "api-1", "app", "third", "three"),
        ];

        let found = resolve(
            records,
            Wanted {
                previous: true,
                ..wanted("api-1")
            },
        )
        .unwrap();

        assert_eq!(found.instance.as_deref(), Some("second"));
        let logs: Vec<&str> = found.lines.iter().map(|r| r.log.as_str()).collect();
        assert_eq!(logs, ["two"]);
    }

    #[test]
    fn previous_with_one_instance_says_there_is_none() {
        let records = vec![rec(T0, "api-1", "app", "only", "x")];

        assert_eq!(
            resolve(
                records,
                Wanted {
                    previous: true,
                    ..wanted("api-1")
                }
            ),
            Err(Unresolved::NoPrevious {
                pod: "api-1".to_owned(),
                container: "app".to_owned()
            })
        );
    }

    #[test]
    fn a_later_poll_admits_only_the_chosen_pod_container_and_instance() {
        let found = resolve(
            vec![
                rec(T0, "api-1", "app", "first", "one"),
                rec(T0 + 1, "api-1", "app", "second", "two"),
            ],
            Wanted {
                previous: true,
                ..wanted("api-1")
            },
        )
        .unwrap();

        assert!(found.admits(&rec(T0 + 5, "api-1", "app", "first", "late")));
        assert!(!found.admits(&rec(T0 + 5, "api-1", "app", "second", "x")));
        assert!(!found.admits(&rec(T0 + 5, "api-1", "sidecar", "first", "x")));
        assert!(!found.admits(&rec(T0 + 5, "api-12", "app", "first", "x")));
    }

    // --- lines ---------------------------------------------------------------

    #[test]
    fn every_line_is_labelled_as_cloudwatch_s() {
        let record = rec(T0, "api-1", "app", "a", "GET / 200");

        assert_eq!(
            line(&record, Palette::Plain),
            "[cloudwatch 2026-10-07T06:21:02Z] GET / 200"
        );
    }

    #[test]
    fn a_stderr_line_says_so_and_an_empty_one_is_just_the_label() {
        let mut record = rec(T0, "api-1", "app", "a", "");
        assert_eq!(
            line(&record, Palette::Plain),
            "[cloudwatch 2026-10-07T06:21:02Z]"
        );
        record.stream = "stderr".to_owned();
        record.log = "panic".to_owned();
        assert_eq!(
            line(&record, Palette::Plain),
            "[cloudwatch 2026-10-07T06:21:02Z stderr] panic"
        );
    }

    #[test]
    fn the_label_is_muted_in_colour_and_the_line_is_not() {
        let record = rec(T0, "api-1", "app", "a", "GET /");

        let painted = line(&record, Palette::Colour(Theme::dark()));

        assert!(painted.starts_with("\x1b["), "{painted:?}");
        assert!(painted.ends_with("\x1b[39m GET /"), "{painted:?}");
    }

    #[test]
    fn the_reading_note_names_the_pod_the_container_and_the_group() {
        let found = resolve(
            vec![
                rec(T0, "api-7d9f", "app", "a", "x"),
                rec(T0 + 1, "api-7d9f", "app", "b", "y"),
            ],
            wanted("api"),
        )
        .unwrap();

        let note = reading_note("api", "shop", &found, "/g", Since::default(), false);

        assert_eq!(
            note,
            "No pod starting \"api\" is running in namespace shop; api-7d9f was. Reading app's \
             lines in the last 1h (2 instances, in time order) from CloudWatch, /g. Older lines \
             need `--since`, e.g. `--since 1d`."
        );
        let exact = reading_note("api-7d9f", "shop", &found, "/g", Since::default(), true);
        assert!(
            exact.starts_with("No pod called api-7d9f is running"),
            "{exact}"
        );
        assert!(exact.ends_with("from CloudWatch, /g."), "{exact}");
    }

    // --- explaining ----------------------------------------------------------

    #[test]
    fn nothing_anywhere_suggests_a_longer_window_and_the_listing() {
        let text = explain(&Unresolved::Nothing, &search(wanted("api"), &[]), now());

        assert!(
            text.starts_with(
                "no pod in namespace shop is called \"api\" or starts with it, and CloudWatch \
                 (/aws/containerinsights/prod/application) has no lines from one in the last 1h."
            ),
            "{text}"
        );
        assert!(text.contains("`--since 1d`"), "{text}");
        assert!(text.contains("`eks pods -n shop`"), "{text}");
        assert!(!text.contains("-l"), "{text}");
    }

    #[test]
    fn nothing_after_a_day_suggests_a_week() {
        let mut search = search(wanted("api"), &[]);
        search.since = Since::Ago(Duration::from_secs(86_400));

        assert!(explain(&Unresolved::Nothing, &search, now()).contains("`--since 7d`"));
    }

    #[test]
    fn nothing_here_but_a_running_pod_elsewhere_names_its_namespace() {
        let elsewhere = ["payments/api-1".to_owned()];

        let text = explain(
            &Unresolved::Nothing,
            &search(wanted("api"), &elsewhere),
            now(),
        );

        assert!(
            text.ends_with(
                "There is a running one in namespace payments: pass `-n payments` to read it."
            ),
            "{text}"
        );
    }

    #[test]
    fn nothing_here_but_several_elsewhere_lists_them() {
        let elsewhere = ["payments/api-1".to_owned(), "staging/api-2".to_owned()];

        let text = explain(
            &Unresolved::Nothing,
            &search(wanted("api"), &elsewhere),
            now(),
        );

        assert!(
            text.contains("payments/api-1 and staging/api-2. Pass `-n <namespace>`"),
            "{text}"
        );
    }

    #[test]
    fn nothing_with_a_selector_says_the_selector_may_be_why() {
        let mut search = search(wanted("api"), &[]);
        search.selected = true;

        assert!(
            explain(&Unresolved::Nothing, &search, now()).contains("drop them to search them all")
        );
    }

    #[test]
    fn several_pods_are_a_table_with_when_each_was_last_heard_from() {
        let why = Unresolved::SeveralPods(vec![
            Seen {
                pod: "api-2".to_owned(),
                last: T0 + 240_000,
            },
            Seen {
                pod: "api-1".to_owned(),
                last: T0,
            },
        ]);

        let text = explain(&why, &search(wanted("api"), &[]), now());

        assert!(
            text.contains("CloudWatch has lines from 2 that did in the last 1h"),
            "{text}"
        );
        assert!(text.contains("POD    LAST LINE"), "{text}");
        assert!(text.contains("api-2  60s ago"), "{text}");
        assert!(text.contains("api-1  5m ago"), "{text}");
        assert!(text.ends_with("e.g. `eks logs api-2`."), "{text}");
    }

    #[test]
    fn container_and_instance_problems_say_which_flag_to_change() {
        let several = explain(
            &Unresolved::SeveralContainers {
                pod: "api-1".to_owned(),
                containers: vec!["app".to_owned(), "proxy".to_owned()],
            },
            &search(wanted("api-1"), &[]),
            now(),
        );
        assert!(
            several.ends_with("`--container`: app or proxy."),
            "{several}"
        );

        let missing = explain(
            &Unresolved::NoSuchContainer {
                pod: "api-1".to_owned(),
                wanted: "sidecar".to_owned(),
                containers: vec!["app".to_owned()],
            },
            &search(wanted("api-1"), &[]),
            now(),
        );
        assert!(missing.contains("called \"sidecar\""), "{missing}");
        assert!(missing.contains("come from app; pass one"), "{missing}");

        let previous = explain(
            &Unresolved::NoPrevious {
                pod: "api-1".to_owned(),
                container: "app".to_owned(),
            },
            &search(wanted("api-1"), &[]),
            now(),
        );
        assert!(previous.contains("Drop `--previous`"), "{previous}");
        assert!(previous.contains("longer `--since`"), "{previous}");
    }

    // --- not set up ----------------------------------------------------------

    #[test]
    fn a_missing_group_says_container_insights_is_not_set_up_and_how_to_set_it_up() {
        let text = not_set_up(
            "/aws/containerinsights/prod/application",
            false,
            &target(),
            "prod (us-east-1)",
        );

        assert!(
            text.starts_with(
                "prod (us-east-1) does not send container logs to CloudWatch: there is no log \
                 group /aws/containerinsights/prod/application"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "aws eks create-addon --cluster-name prod --addon-name \
                 amazon-cloudwatch-observability --region us-east-1 --profile prod-admin"
            ),
            "{text}"
        );
        assert!(text.contains("not before"), "{text}");
        assert!(text.contains(SETUP_URL), "{text}");
        assert!(text.contains("`log_group`"), "{text}");
    }

    #[test]
    fn a_missing_group_config_named_points_at_the_config() {
        let text = not_set_up("/wrong", true, &target(), "prod");

        assert!(text.contains("`log_group` in config.toml names"), "{text}");
        assert!(
            text.contains("aws logs describe-log-groups --region us-east-1"),
            "{text}"
        );
        assert!(!text.contains("create-addon"), "{text}");
    }
}
