//! `--json`: the read commands' answers as data rather than as tables.
//!
//! Every function here is pure over rows the table renderers already read, so
//! `eks nodes --json` and `eks nodes` cannot disagree about what a node's row
//! means. Only the *spelling* differs, and it differs on purpose:
//!
//! - **Quantities are numbers in base units.** Cores for CPU (`0.25`, not
//!   `250m`), bytes for memory and storage, a plain count for pods and
//!   devices. A whole number prints as an integer and anything else as a
//!   decimal, so `jq '.nodes[].memory.allocatable'` is arithmetic-ready and
//!   no script has to parse `15.6Gi` back into the number it was rounded from.
//! - **Times are RFC 3339 instants**, not ages. `3d` is relative to the moment
//!   the table was printed and rounded for a glance; an instant is neither.
//! - **Unknown is `null`**, never `-` and never `0`. A figure that could not
//!   be read is a different fact from a measured zero, exactly as it is in the
//!   tables, and `null` is the one spelling a script cannot mistake for data.
//! - **Every field is present on every row**, whatever the terminal width and
//!   whether or not `--wide` was given: a script is not a narrow terminal.
//!
//! `notes` carries the sentences that explain a `null` — the pods could not be
//! listed, metrics-server is not installed — and how old the usage figures
//! are. They are written for a person reading a script's log, not for a
//! program to match on, and are worded for fields rather than for columns. The
//! table's ordering notes are left out: the order of the array is the answer.

use std::collections::BTreeMap;

use k8s_openapi::jiff::Timestamp;
use serde::Serialize;
use serde_json::Number;

use crate::aws::insights::Record;
use crate::cluster::ClusterView;
use crate::k8s::metrics::{self, Sample};
use crate::k8s::nodes::{self, NodeRow};
use crate::k8s::pods::{PodRow, Scope};
use crate::k8s::quantity::Quantity;
use crate::theme::Severity;

/// How a read command prints its answer.
///
/// Carried on each listing's request beside `width` and `palette`, both of
/// which [`Output::Json`] makes moot: a document has no columns to drop and no
/// cells to colour.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Output {
    /// The aligned, possibly coloured, table. The default.
    #[default]
    Table,
    /// One pretty-printed JSON document on stdout.
    Json,
}

impl Output {
    /// `Json` when `--json` was given. Named for the flag, as
    /// `Width::widened` is.
    #[must_use]
    pub fn json(yes: bool) -> Self {
        if yes { Self::Json } else { Self::Table }
    }
}

/// What the table renderers print where the API server left a field empty.
/// Mapped back to `null` here rather than leaked into the data.
const UNKNOWN: &str = "-";

/// One kubeconfig context, as `eks contexts --json` lists it and as the
/// listings name the cluster they read.
#[derive(Debug, Serialize)]
struct Entry<'a> {
    /// The value to pass back to `--context` or `eks use`.
    context: &'a str,
    /// The short cluster name the tables show.
    name: &'a str,
    region: Option<&'a str>,
    account_id: Option<&'a str>,
    namespace: &'a str,
    current: bool,
}

impl<'a> Entry<'a> {
    fn of(view: &'a ClusterView) -> Self {
        Self {
            context: &view.context_name,
            name: &view.display_name,
            region: view.region.as_deref(),
            account_id: view.account_id.as_deref(),
            namespace: &view.namespace,
            current: view.is_current,
        }
    }
}

/// `eks contexts --json`.
///
/// An object around the array rather than a bare array, so a field can be
/// added beside `contexts` later without breaking every script that read it.
/// An empty kubeconfig is `{"contexts": []}`: the table's "run `aws eks
/// update-kubeconfig`" advice is for a person, and a script asked a question
/// whose honest answer is "none".
pub fn contexts(views: &[ClusterView]) -> Result<String, serde_json::Error> {
    #[derive(Serialize)]
    struct Document<'a> {
        contexts: Vec<Entry<'a>>,
    }
    serde_json::to_string_pretty(&Document {
        contexts: views.iter().map(Entry::of).collect(),
    })
}

/// `eks current --json`: the one context, in the shape `contexts` lists it.
pub fn current(view: &ClusterView) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&Entry::of(view))
}

/// `eks nodes --json`. `rows` arrive already sorted; `notes` is [`notes`]'s.
pub fn nodes(
    cluster: &ClusterView,
    rows: &[NodeRow],
    notes: &[String],
) -> Result<String, serde_json::Error> {
    #[derive(Serialize)]
    struct Document<'a> {
        cluster: Entry<'a>,
        nodes: Vec<Node<'a>>,
        notes: &'a [String],
    }
    serde_json::to_string_pretty(&Document {
        cluster: Entry::of(cluster),
        nodes: rows.iter().map(Node::of).collect(),
        notes,
    })
}

/// `eks pods --json`. `rows` arrive already sorted; `notes` is [`notes`]'s.
///
/// `namespace` is the one that was listed, `null` under `--all-namespaces` —
/// worth saying because without `-n` it is the context's own default, which
/// the caller did not necessarily know.
pub fn pods(
    cluster: &ClusterView,
    scope: &Scope,
    rows: &[PodRow],
    notes: &[String],
) -> Result<String, serde_json::Error> {
    #[derive(Serialize)]
    struct Document<'a> {
        cluster: Entry<'a>,
        namespace: Option<&'a str>,
        pods: Vec<Pod<'a>>,
        notes: &'a [String],
    }
    serde_json::to_string_pretty(&Document {
        cluster: Entry::of(cluster),
        namespace: match scope {
            Scope::Namespace(name) => Some(name),
            Scope::All => None,
        },
        pods: rows.iter().map(Pod::of).collect(),
        notes,
    })
}

/// Everything [`notes`] is assembled from, already resolved by the time a
/// listing calls it — the same trade the tables' own footnote inputs make.
#[derive(Debug)]
pub struct NoteInputs<'a> {
    /// The pod listing behind a node listing's `requested` figures and pod
    /// counts. `None` for a pod listing, which has no such second request:
    /// its own pods failing ends the command.
    pub requests: Option<Result<(), &'a str>>,
    /// The metrics read, with its failure already worded by
    /// `metrics::explain`.
    pub usage: Result<(), &'a str>,
    /// Whether any row came back with a usage figure — `shows_usage` on the
    /// listing's own rows.
    pub usage_shown: bool,
    pub samples: &'a [Option<Sample>],
    pub now: Timestamp,
    /// The cluster's label, for the sentences that name it.
    pub label: &'a str,
}

/// The notes a JSON listing carries: why a field is `null` across the board,
/// and how old the usage is when there is some.
///
/// The same decisions the tables' footnotes make — through
/// `metrics::Outcome`, so the three usage cases cannot be told apart
/// differently here — worded for fields instead of columns. A script has no
/// `CPU USE` column to be told is missing.
#[must_use]
pub fn notes(inputs: &NoteInputs<'_>) -> Vec<String> {
    let mut notes = Vec::new();

    if let Some(Err(explanation)) = inputs.requests {
        notes.push(format!(
            "Every `requested` figure and pod `count` is null because the pods could not be \
             listed.\n{explanation}"
        ));
    }

    match metrics::Outcome::of(inputs.usage.ok().as_ref(), inputs.usage_shown) {
        metrics::Outcome::Shown => notes.extend(
            metrics::freshness(inputs.samples.iter().flatten(), inputs.now)
                .map(metrics::freshness_note),
        ),
        metrics::Outcome::Unsampled => notes.push(format!(
            "Every `used` figure is null because nothing here has been sampled yet.\n{}",
            metrics::unsampled(inputs.label)
        )),
        metrics::Outcome::Unreadable => {
            if let Err(explanation) = inputs.usage {
                notes.push(format!(
                    "Every `used` figure is null because live usage could not be read.\n\
                     {explanation}"
                ));
            }
        }
    }

    notes
}

/// A listing's secondary read reduced to what [`notes`] asks of it: whether
/// it failed, and the sentence explaining why if it did.
pub fn failure<T>(result: &Result<T, String>) -> Result<(), &str> {
    match result {
        Ok(_) => Ok(()),
        Err(explanation) => Err(explanation),
    }
}

/// Where one of `eks logs --json`'s lines was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogSource {
    /// The API server: the pod was running.
    Api,
    /// Container Insights' group: the pod was gone.
    Cloudwatch,
}

/// One container log line, as `eks logs --json` prints it: one JSON object
/// per line of output (JSON Lines), because a follow never ends.
///
/// The fields are the same whichever source the line came from, so a script
/// need not know which one answered. `time` is when the line was written:
/// the kubelet's stamp for an API line, CloudWatch's for the other. Either
/// is printed to the millisecond, as `control-plane-logs --json` prints its
/// times, so every `time` this tool writes sorts as text. `stream` is
/// `stdout` or `stderr` where the source says, and `null` for an API line:
/// the API server interleaves the two and does not say which was which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine<'a> {
    pub time: Option<Timestamp>,
    pub source: LogSource,
    pub namespace: &'a str,
    pub pod: &'a str,
    pub container: &'a str,
    pub stream: Option<&'a str>,
    pub log: &'a str,
}

impl<'a> LogLine<'a> {
    /// A line Container Insights kept. A field Fluent Bit left out is `null`,
    /// not an empty string.
    #[must_use]
    pub fn cloudwatch(record: &'a Record) -> Self {
        Self {
            time: Timestamp::from_millisecond(record.timestamp).ok(),
            source: LogSource::Cloudwatch,
            namespace: &record.namespace,
            pod: &record.pod,
            container: &record.container,
            stream: Some(record.stream.as_str()),
            log: &record.log,
        }
    }
}

/// [`LogLine`] as one line of JSON, without the newline.
pub fn log_line<'a>(line: &LogLine<'a>) -> Result<String, serde_json::Error> {
    #[derive(Serialize)]
    struct Wire<'a> {
        time: Option<String>,
        source: LogSource,
        namespace: Option<&'a str>,
        pod: Option<&'a str>,
        container: Option<&'a str>,
        stream: Option<&'a str>,
        log: &'a str,
    }
    let present = |text: &'a str| Some(text).filter(|text| !text.is_empty());
    serde_json::to_string(&Wire {
        time: line.time.map(|at| format!("{at:.3}")),
        source: line.source,
        namespace: present(line.namespace),
        pod: present(line.pod),
        container: present(line.container),
        stream: line.stream.and_then(present),
        log: line.log,
    })
}

/// One node. Field names follow the table's columns where there is one, in
/// `snake_case`, with the column's pair split into its halves.
#[derive(Debug, Serialize)]
struct Node<'a> {
    name: &'a str,
    status: &'a str,
    severity: &'static str,
    version: Option<&'a str>,
    created_at: Option<String>,
    /// Cores.
    cpu: Resource,
    /// Bytes.
    memory: Resource,
    pods: PodCount,
    /// Bytes. No `requested`: nothing tracks requests against it yet.
    ephemeral_storage: Capacity,
    /// Bytes, by pool name (`hugepages-2Mi`). Every size the node reports,
    /// zero included — which sizes earn a column is the table's business.
    hugepages: BTreeMap<&'a str, Capacity>,
    /// Counts, by fully-qualified resource name.
    devices: BTreeMap<&'a str, Device>,
    pressure: Pressure,
    /// Whether this node's usage sample is old enough to call stale.
    usage_stale: bool,
    internal_ip: Option<&'a str>,
    external_ip: Option<&'a str>,
    os_image: Option<&'a str>,
    kernel_version: Option<&'a str>,
    container_runtime: Option<&'a str>,
}

impl<'a> Node<'a> {
    fn of(row: &'a NodeRow) -> Self {
        Self {
            name: &row.name,
            status: &row.status,
            severity: severity(row.severity),
            version: known(&row.version),
            created_at: row.created_at.map(|at| at.to_string()),
            cpu: Resource::of(row.cpu, row.cpu_requested, row.cpu_used),
            memory: Resource::of(row.memory, row.memory_requested, row.memory_used),
            pods: PodCount {
                count: row.pods.amount.map(number),
                allocatable: row.pods.allocatable.map(number),
            },
            ephemeral_storage: Capacity::of(row.ephemeral_storage),
            hugepages: row
                .hugepages
                .iter()
                .map(|(name, capacity)| (name.as_str(), Capacity::of(*capacity)))
                .collect(),
            devices: row
                .devices
                .iter()
                .map(|(name, device)| (name.as_str(), Device::of(*device)))
                .collect(),
            pressure: Pressure::of(row.pressure),
            usage_stale: row.usage_stale,
            internal_ip: known(&row.internal_ip),
            external_ip: known(&row.external_ip),
            os_image: known(&row.os_image),
            kernel_version: known(&row.kernel_version),
            container_runtime: known(&row.container_runtime),
        }
    }
}

/// One of a node's CPU or memory, every figure the table spreads over three
/// columns in one place.
#[derive(Debug, Serialize)]
struct Resource {
    capacity: Option<Number>,
    allocatable: Option<Number>,
    /// What the pods on the node have booked. `null` only when the pods could
    /// not be listed; a node running nothing is `0`.
    requested: Option<Number>,
    /// What metrics-server last measured. `null` when there is no sample.
    used: Option<Number>,
}

impl Resource {
    fn of(capacity: nodes::Capacity, requested: nodes::Share, used: nodes::Share) -> Self {
        Self {
            capacity: capacity.capacity.map(number),
            allocatable: capacity.allocatable.map(number),
            requested: requested.amount.map(number),
            used: used.amount.map(number),
        }
    }
}

#[derive(Debug, Serialize)]
struct Capacity {
    capacity: Option<Number>,
    allocatable: Option<Number>,
}

impl Capacity {
    fn of(capacity: nodes::Capacity) -> Self {
        Self {
            capacity: capacity.capacity.map(number),
            allocatable: capacity.allocatable.map(number),
        }
    }
}

#[derive(Debug, Serialize)]
struct PodCount {
    /// Pods on the node. `null` when the pods could not be listed.
    count: Option<Number>,
    /// How many it will accept — the kubelet's `--max-pods`.
    allocatable: Option<Number>,
}

#[derive(Debug, Serialize)]
struct Device {
    capacity: Option<Number>,
    allocatable: Option<Number>,
    /// Booked by the node's pods; `null` only when they could not be listed.
    requested: Option<Number>,
}

impl Device {
    fn of(device: nodes::Device) -> Self {
        Self {
            capacity: device.capacity.capacity.map(number),
            allocatable: device.capacity.allocatable.map(number),
            requested: device.booked.map(number),
        }
    }
}

// The node's own four independent conditions, mirrored field for field; see
// `nodes::Pressure` for why these are not a state machine.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Serialize)]
struct Pressure {
    memory: bool,
    disk: bool,
    pid: bool,
    network_unavailable: bool,
}

impl Pressure {
    fn of(pressure: nodes::Pressure) -> Self {
        Self {
            memory: pressure.memory,
            disk: pressure.disk,
            pid: pressure.pid,
            network_unavailable: pressure.network_unavailable,
        }
    }
}

/// One pod.
#[derive(Debug, Serialize)]
struct Pod<'a> {
    namespace: Option<&'a str>,
    name: &'a str,
    status: &'a str,
    severity: &'static str,
    ready: Option<Fraction>,
    restarts: i32,
    /// When the newest restart that counts towards `restarts` finished.
    last_restart_at: Option<String>,
    created_at: Option<String>,
    /// Cores.
    cpu: PodResource,
    /// Bytes.
    memory: PodResource,
    /// What the pod asked for of each extended resource, by name. A resource
    /// it did not ask for is absent, which is a real zero.
    extended_requested: BTreeMap<&'a str, Number>,
    /// Whether this pod's usage sample is old enough to call stale.
    usage_stale: bool,
    node: Option<&'a str>,
    ip: Option<&'a str>,
    nominated_node: Option<&'a str>,
    /// `null` for a pod with no readiness gates, which is nearly every pod.
    readiness_gates: Option<Fraction>,
}

impl<'a> Pod<'a> {
    fn of(row: &'a PodRow) -> Self {
        Self {
            namespace: known(&row.namespace),
            name: &row.name,
            status: &row.status,
            severity: severity(row.severity),
            ready: Fraction::parse(&row.ready),
            restarts: row.restarts,
            last_restart_at: row.last_restart.map(|at| at.to_string()),
            created_at: row.created_at.map(|at| at.to_string()),
            cpu: PodResource {
                requested: number(row.cpu_requested),
                limit: row.cpu_limit.map(number),
                used: row.cpu_used.map(number),
            },
            memory: PodResource {
                requested: number(row.memory_requested),
                limit: row.memory_limit.map(number),
                used: row.memory_used.map(number),
            },
            extended_requested: row
                .extended_requested
                .iter()
                .map(|(name, amount)| (name.as_str(), number(*amount)))
                .collect(),
            usage_stale: row.usage_stale,
            node: known(&row.node),
            ip: known(&row.ip),
            nominated_node: known(&row.nominated_node),
            readiness_gates: row.readiness_gates.as_deref().and_then(Fraction::parse),
        }
    }
}

#[derive(Debug, Serialize)]
struct PodResource {
    /// What the pod asked for, as the scheduler sums it. `0` for a pod that
    /// set no request, which is what it asked for.
    requested: Number,
    /// The pod-wide limit; `null` when any container counting towards it left
    /// this resource unbounded.
    limit: Option<Number>,
    /// What metrics-server last measured. `null` when there is no sample.
    used: Option<Number>,
}

/// `kubectl`'s `1/2`, as the two counts it is made of.
#[derive(Debug, PartialEq, Eq, Serialize)]
struct Fraction {
    ready: u32,
    total: u32,
}

impl Fraction {
    /// Read back the `ready/total` text `PodRow` carries.
    ///
    /// The row holds the fraction as the string the table prints, and both
    /// fields are built by `PodRow::from_pod` from two counts with a `/`
    /// between them, so this cannot fail on a row that came from a pod.
    /// `None` rather than an error anyway: a malformed row costs one field,
    /// not the document.
    fn parse(text: &str) -> Option<Self> {
        let (ready, total) = text.split_once('/')?;
        Some(Self {
            ready: ready.parse().ok()?,
            total: total.parse().ok()?,
        })
    }
}

/// A quantity in its base unit, as an integer when it is whole and a decimal
/// otherwise.
///
/// Decided from the integer thousandths, not from a float, so `4` cores is
/// `4` and 16 GiB is exactly `17179869184`. A value too large for an `i64` —
/// thousandths of an exbibyte do not fit, though no node reports one — falls
/// back to the float, which is the right magnitude rather than a wrong number.
// The float half divides an `i128` of thousandths; anything that loses
// precision there is already beyond what a JSON reader holds exactly anyway.
#[allow(clippy::cast_precision_loss)]
fn number(quantity: Quantity) -> Number {
    let thousandths = quantity.thousandths();
    if thousandths % 1000 == 0
        && let Ok(whole) = i64::try_from(thousandths / 1000)
    {
        return Number::from(whole);
    }
    // `from_f64` refuses only NaN and the infinities, which a finite `i128`
    // divided by a thousand never is; zero is the honest fallback either way.
    Number::from_f64(thousandths as f64 / 1000.0).unwrap_or_else(|| Number::from(0))
}

/// The table's `-` placeholder back to `null`.
fn known(text: &str) -> Option<&str> {
    (text != UNKNOWN && !text.is_empty()).then_some(text)
}

/// The shared severity scale, in the words a script would filter on.
fn severity(severity: Severity) -> &'static str {
    match severity {
        Severity::Ok => "ok",
        Severity::Warn => "warn",
        Severity::Critical => "critical",
        Severity::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use k8s_openapi::api::core::v1::{Node as ApiNode, Pod as ApiPod};
    use serde_json::{Value, json};

    use super::*;
    use crate::k8s::metrics::Usage;
    use crate::k8s::pods::Placed;

    fn now() -> Timestamp {
        "2026-10-02T06:00:00Z".parse().unwrap()
    }

    fn view() -> ClusterView {
        ClusterView {
            context_name: "arn:aws:eks:us-east-1:111122223333:cluster/prod".to_owned(),
            display_name: "prod".to_owned(),
            region: Some("us-east-1".to_owned()),
            account_id: Some("111122223333".to_owned()),
            namespace: "payments".to_owned(),
            is_current: true,
        }
    }

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    fn api_node() -> ApiNode {
        serde_json::from_value(json!({
            "metadata": {
                "name": "ip-10-0-1-9.ec2.internal",
                "creationTimestamp": "2026-09-29T06:00:00Z"
            },
            "status": {
                "capacity": {
                    "cpu": "4",
                    "memory": "16Gi",
                    "pods": "58",
                    "ephemeral-storage": "80Gi",
                    "hugepages-2Mi": "0",
                    "nvidia.com/gpu": "4"
                },
                "allocatable": {
                    "cpu": "3920m",
                    "memory": "15Gi",
                    "pods": "58",
                    "ephemeral-storage": "76Gi",
                    "hugepages-2Mi": "0",
                    "nvidia.com/gpu": "3"
                },
                "conditions": [
                    { "type": "Ready", "status": "True" },
                    { "type": "DiskPressure", "status": "True" }
                ],
                "addresses": [{ "type": "InternalIP", "address": "10.0.1.9" }],
                "nodeInfo": {
                    "kubeletVersion": "v1.33.4-eks-1234",
                    "osImage": "Amazon Linux 2023.9.20260714",
                    "kernelVersion": "6.1.150",
                    "containerRuntimeVersion": "containerd://1.7.28",
                    "architecture": "amd64",
                    "bootID": "", "machineID": "", "systemUUID": "",
                    "kubeProxyVersion": "", "operatingSystem": "linux"
                }
            }
        }))
        .unwrap()
    }

    fn api_pod() -> ApiPod {
        serde_json::from_value(json!({
            "metadata": {
                "namespace": "payments",
                "name": "api-7d9f",
                "creationTimestamp": "2026-10-02T03:00:00Z"
            },
            "spec": {
                "nodeName": "ip-10-0-1-9.ec2.internal",
                "containers": [{
                    "name": "api",
                    "resources": {
                        "requests": { "cpu": "250m", "memory": "512Mi", "nvidia.com/gpu": "1" },
                        "limits": { "cpu": "1", "memory": "1Gi", "nvidia.com/gpu": "1" }
                    }
                }, {
                    "name": "proxy",
                    "resources": { "requests": { "cpu": "50m", "memory": "64Mi" } }
                }]
            },
            "status": {
                "phase": "Running",
                "podIP": "10.0.1.42",
                "containerStatuses": [{
                    "name": "api", "ready": true, "restartCount": 2, "image": "", "imageID": "",
                    "state": { "running": {} },
                    "lastState": { "terminated": {
                        "exitCode": 1, "finishedAt": "2026-10-02T05:55:00Z"
                    } }
                }, {
                    "name": "proxy", "ready": false, "restartCount": 0, "image": "", "imageID": "",
                    "state": { "waiting": { "reason": "ContainerCreating" } }
                }]
            }
        }))
        .unwrap()
    }

    fn sample(cpu: &str, memory: &str) -> Sample {
        Sample {
            usage: Usage {
                cpu: Some(Quantity::parse(cpu).unwrap()),
                memory: Some(Quantity::parse(memory).unwrap()),
            },
            taken_at: Some("2026-10-02T05:59:30Z".parse().unwrap()),
            window: Some(k8s_openapi::jiff::SignedDuration::from_secs(15)),
        }
    }

    fn node_row() -> NodeRow {
        let mut placed = Placed {
            pods: 12,
            ..Placed::default()
        };
        placed.requests.cpu = Quantity::parse("1500m").unwrap();
        placed.requests.memory = Quantity::parse("4Gi").unwrap();
        placed
            .requests
            .extended
            .insert("nvidia.com/gpu".to_owned(), Quantity::parse("2").unwrap());
        NodeRow::from_node(
            &api_node(),
            Some(&placed),
            Some(sample("980m", "6Gi")),
            now(),
        )
    }

    fn node_document(rows: &[NodeRow]) -> Value {
        parse(&nodes(&view(), rows, &[]).unwrap())
    }

    #[test]
    fn whole_quantities_are_integers_and_fractional_ones_are_decimals() {
        assert_eq!(number(Quantity::parse("4").unwrap()), Number::from(4));
        assert_eq!(
            number(Quantity::parse("16Gi").unwrap()),
            Number::from(17_179_869_184_i64)
        );
        assert_eq!(
            number(Quantity::parse("250m").unwrap()).as_f64(),
            Some(0.25)
        );
        assert_eq!(
            number(Quantity::parse("3920m").unwrap()).as_f64(),
            Some(3.92)
        );
        assert_eq!(number(Quantity::default()), Number::from(0));
    }

    #[test]
    fn a_quantity_too_large_for_an_integer_is_still_the_right_magnitude() {
        // Thousandths of a byte overflow an `i64` here, which is exactly the
        // case the float fallback exists for.
        let huge = number(Quantity::parse("100Ei").unwrap());
        assert!(huge.as_i64().is_none());
        assert!((huge.as_f64().unwrap() - 100.0 * 2_f64.powi(60)).abs() < 1e6);
    }

    #[test]
    fn a_node_carries_every_figure_in_base_units() {
        let document = node_document(&[node_row()]);
        let node = &document["nodes"][0];

        assert_eq!(node["name"], "ip-10-0-1-9.ec2.internal");
        assert_eq!(node["status"], "Ready");
        assert_eq!(node["severity"], "ok");
        assert_eq!(node["version"], "v1.33.4-eks-1234");
        assert_eq!(node["created_at"], "2026-09-29T06:00:00Z");
        assert_eq!(
            node["cpu"],
            json!({ "capacity": 4, "allocatable": 3.92, "requested": 1.5, "used": 0.98 })
        );
        assert_eq!(
            node["memory"],
            json!({
                "capacity": 17_179_869_184_i64,
                "allocatable": 16_106_127_360_i64,
                "requested": 4_294_967_296_i64,
                "used": 6_442_450_944_i64
            })
        );
        assert_eq!(node["pods"], json!({ "count": 12, "allocatable": 58 }));
        assert_eq!(
            node["devices"],
            json!({ "nvidia.com/gpu": { "capacity": 4, "allocatable": 3, "requested": 2 } })
        );
        assert_eq!(
            node["ephemeral_storage"],
            json!({ "capacity": 85_899_345_920_i64, "allocatable": 81_604_378_624_i64 })
        );
        assert_eq!(
            node["hugepages"],
            json!({ "hugepages-2Mi": { "capacity": 0, "allocatable": 0 } })
        );
        assert_eq!(
            node["pressure"],
            json!({ "memory": false, "disk": true, "pid": false, "network_unavailable": false })
        );
        assert_eq!(node["usage_stale"], false);
    }

    #[test]
    fn a_node_carries_the_wide_columns_whether_or_not_wide_was_asked_for() {
        let document = node_document(&[node_row()]);
        let node = &document["nodes"][0];

        assert_eq!(node["internal_ip"], "10.0.1.9");
        assert_eq!(node["os_image"], "Amazon Linux 2023.9.20260714");
        assert_eq!(node["kernel_version"], "6.1.150");
        assert_eq!(node["container_runtime"], "containerd://1.7.28");
        // Absent from the node, so absent from the data — not the table's `-`.
        assert_eq!(node["external_ip"], Value::Null);
    }

    #[test]
    fn a_node_whose_pods_could_not_be_listed_has_null_requests_not_zero() {
        let row = NodeRow::from_node(&api_node(), None, None, now());
        let document = node_document(&[row]);
        let node = &document["nodes"][0];

        assert_eq!(node["cpu"]["requested"], Value::Null);
        assert_eq!(node["memory"]["requested"], Value::Null);
        assert_eq!(node["pods"]["count"], Value::Null);
        assert_eq!(node["devices"]["nvidia.com/gpu"]["requested"], Value::Null);
        // And no sample is no usage, rather than an idle node.
        assert_eq!(node["cpu"]["used"], Value::Null);
        // The figures the node itself reported are unaffected.
        assert_eq!(node["pods"]["allocatable"], 58);
    }

    #[test]
    fn a_node_running_nothing_has_zero_requests_not_null() {
        let row = NodeRow::from_node(&api_node(), Some(&Placed::default()), None, now());
        let document = node_document(&[row]);
        let node = &document["nodes"][0];

        assert_eq!(node["cpu"]["requested"], 0);
        assert_eq!(node["pods"]["count"], 0);
        assert_eq!(node["devices"]["nvidia.com/gpu"]["requested"], 0);
    }

    #[test]
    fn a_node_still_registering_is_nulls_rather_than_dashes() {
        let bare: ApiNode =
            serde_json::from_value(json!({ "metadata": { "name": "new" } })).unwrap();
        let document = node_document(&[NodeRow::from_node(&bare, None, None, now())]);
        let node = &document["nodes"][0];

        assert_eq!(node["name"], "new");
        assert_eq!(node["severity"], "unknown");
        assert_eq!(node["version"], Value::Null);
        assert_eq!(node["created_at"], Value::Null);
        assert_eq!(
            node["cpu"],
            json!({ "capacity": null, "allocatable": null, "requested": null, "used": null })
        );
        assert_eq!(node["internal_ip"], Value::Null);
        assert_eq!(node["os_image"], Value::Null);
        assert_eq!(node["devices"], json!({}));
        assert_eq!(node["hugepages"], json!({}));
    }

    #[test]
    fn a_node_listing_names_its_cluster_and_keeps_the_rows_order() {
        let mut first = node_row();
        first.name = "b".to_owned();
        let mut second = node_row();
        second.name = "a".to_owned();
        let document = parse(&nodes(&view(), &[first, second], &["a note".to_owned()]).unwrap());

        assert_eq!(
            document["cluster"],
            json!({
                "context": "arn:aws:eks:us-east-1:111122223333:cluster/prod",
                "name": "prod",
                "region": "us-east-1",
                "account_id": "111122223333",
                "namespace": "payments",
                "current": true
            })
        );
        // Sorting is the caller's; the document does not re-sort behind it.
        assert_eq!(document["nodes"][0]["name"], "b");
        assert_eq!(document["nodes"][1]["name"], "a");
        assert_eq!(document["notes"], json!(["a note"]));
    }

    #[test]
    fn an_empty_node_listing_is_an_empty_array() {
        let document = node_document(&[]);
        assert_eq!(document["nodes"], json!([]));
        assert_eq!(document["notes"], json!([]));
    }

    #[test]
    fn a_pod_carries_its_counts_requests_limits_and_usage() {
        let row = PodRow::from_pod(&api_pod(), Some(sample("400m", "300Mi")), now());
        let document = parse(&pods(&view(), &Scope::All, &[row], &[]).unwrap());
        let pod = &document["pods"][0];

        assert_eq!(pod["namespace"], "payments");
        assert_eq!(pod["name"], "api-7d9f");
        assert_eq!(pod["ready"], json!({ "ready": 1, "total": 2 }));
        assert_eq!(pod["restarts"], 2);
        assert_eq!(pod["last_restart_at"], "2026-10-02T05:55:00Z");
        assert_eq!(pod["created_at"], "2026-10-02T03:00:00Z");
        // The proxy left CPU unbounded, so the pod as a whole has no ceiling.
        assert_eq!(
            pod["cpu"],
            json!({ "requested": 0.3, "limit": null, "used": 0.4 })
        );
        assert_eq!(
            pod["memory"],
            json!({ "requested": 603_979_776, "limit": null, "used": 314_572_800 })
        );
        assert_eq!(pod["extended_requested"], json!({ "nvidia.com/gpu": 1 }));
        assert_eq!(pod["node"], "ip-10-0-1-9.ec2.internal");
        assert_eq!(pod["ip"], "10.0.1.42");
        assert_eq!(pod["nominated_node"], Value::Null);
        assert_eq!(pod["readiness_gates"], Value::Null);
        assert_eq!(pod["usage_stale"], false);
    }

    #[test]
    fn a_pod_nobody_has_sampled_or_scheduled_is_nulls() {
        let bare: ApiPod =
            serde_json::from_value(json!({ "metadata": { "name": "pending" } })).unwrap();
        let document = parse(
            &pods(
                &view(),
                &Scope::Namespace("payments".to_owned()),
                &[PodRow::from_pod(&bare, None, now())],
                &[],
            )
            .unwrap(),
        );
        let pod = &document["pods"][0];

        assert_eq!(pod["namespace"], Value::Null);
        assert_eq!(pod["node"], Value::Null);
        assert_eq!(pod["ip"], Value::Null);
        assert_eq!(pod["created_at"], Value::Null);
        assert_eq!(pod["last_restart_at"], Value::Null);
        assert_eq!(pod["ready"], json!({ "ready": 0, "total": 0 }));
        // Asked for nothing is a real zero; never sampled is not.
        assert_eq!(
            pod["cpu"],
            json!({ "requested": 0, "limit": null, "used": null })
        );
        assert_eq!(pod["extended_requested"], json!({}));
    }

    #[test]
    fn a_pod_listing_says_which_namespace_it_read_and_null_for_all_of_them() {
        let one =
            parse(&pods(&view(), &Scope::Namespace("payments".to_owned()), &[], &[]).unwrap());
        assert_eq!(one["namespace"], "payments");
        assert_eq!(one["pods"], json!([]));

        let all = parse(&pods(&view(), &Scope::All, &[], &[]).unwrap());
        assert_eq!(all["namespace"], Value::Null);
    }

    #[test]
    fn readiness_gates_are_counts_too() {
        let mut row = PodRow::from_pod(&api_pod(), None, now());
        row.readiness_gates = Some("1/2".to_owned());
        let document = parse(&pods(&view(), &Scope::All, &[row], &[]).unwrap());

        assert_eq!(
            document["pods"][0]["readiness_gates"],
            json!({ "ready": 1, "total": 2 })
        );
    }

    #[test]
    fn a_fraction_that_is_not_one_costs_its_field_and_nothing_else() {
        assert_eq!(
            Fraction::parse("3/4"),
            Some(Fraction { ready: 3, total: 4 })
        );
        for malformed in ["", "-", "3", "a/b", "3/", "/4", "-1/2"] {
            assert_eq!(Fraction::parse(malformed), None, "{malformed:?}");
        }
    }

    #[test]
    fn every_severity_has_a_word() {
        assert_eq!(severity(Severity::Ok), "ok");
        assert_eq!(severity(Severity::Warn), "warn");
        assert_eq!(severity(Severity::Critical), "critical");
        assert_eq!(severity(Severity::Unknown), "unknown");
    }

    #[test]
    fn contexts_list_every_context_with_its_parts() {
        let other = ClusterView {
            context_name: "kind-dev".to_owned(),
            display_name: "kind-dev".to_owned(),
            region: None,
            account_id: None,
            namespace: "default".to_owned(),
            is_current: false,
        };
        let document = parse(&contexts(&[view(), other]).unwrap());

        assert_eq!(document["contexts"][0]["name"], "prod");
        assert_eq!(document["contexts"][0]["current"], true);
        assert_eq!(
            document["contexts"][1],
            json!({
                "context": "kind-dev",
                "name": "kind-dev",
                "region": null,
                "account_id": null,
                "namespace": "default",
                "current": false
            })
        );
    }

    #[test]
    fn an_empty_kubeconfig_is_an_empty_array_not_advice() {
        assert_eq!(parse(&contexts(&[]).unwrap()), json!({ "contexts": [] }));
    }

    #[test]
    fn current_is_the_one_context_in_the_same_shape() {
        let document = parse(&current(&view()).unwrap());
        assert_eq!(
            document["context"],
            "arn:aws:eks:us-east-1:111122223333:cluster/prod"
        );
        assert_eq!(document["current"], true);
    }

    #[test]
    fn output_is_pretty_printed_for_a_person_reading_it_raw() {
        let text = contexts(&[view()]).unwrap();
        assert!(text.starts_with("{\n  \"contexts\": [\n"), "{text}");
    }

    fn inputs(samples: &[Option<Sample>]) -> NoteInputs<'_> {
        NoteInputs {
            requests: Some(Ok(())),
            usage: Ok(()),
            usage_shown: true,
            samples,
            now: now(),
            label: "prod (us-east-1)",
        }
    }

    #[test]
    fn fresh_usage_is_dated_and_nothing_else_is_said() {
        let samples = [Some(sample("1", "1Gi"))];
        assert_eq!(
            notes(&inputs(&samples)),
            vec!["Usage is up to 30s old, averaged over 15s.".to_owned()]
        );
    }

    #[test]
    fn a_failed_pod_listing_explains_the_null_requests_in_terms_of_fields() {
        let samples = [Some(sample("1", "1Gi"))];
        let notes = notes(&NoteInputs {
            requests: Some(Err("prod (us-east-1) refused to list pods.")),
            ..inputs(&samples)
        });

        assert_eq!(notes.len(), 2);
        assert!(notes[0].starts_with("Every `requested` figure and pod `count` is null"));
        assert!(notes[0].ends_with("refused to list pods."));
        // Never a column name: a script has no columns.
        assert!(!notes[0].contains("REQ"));
    }

    #[test]
    fn unreadable_usage_explains_the_null_figures() {
        let notes = notes(&NoteInputs {
            usage: Err("prod has no metrics.k8s.io API."),
            usage_shown: false,
            ..inputs(&[])
        });

        assert_eq!(
            notes,
            vec![
                "Every `used` figure is null because live usage could not be read.\n\
                 prod has no metrics.k8s.io API."
                    .to_owned()
            ]
        );
    }

    #[test]
    fn usage_that_answered_with_nothing_says_so_rather_than_saying_it_failed() {
        let notes = notes(&NoteInputs {
            usage_shown: false,
            ..inputs(&[None])
        });

        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].starts_with(
                "Every `used` figure is null because nothing here has been sampled yet."
            )
        );
        assert!(notes[0].contains("metrics-server answered for prod (us-east-1)"));
    }

    #[test]
    fn a_pod_listing_has_no_requests_note_to_give() {
        let notes = notes(&NoteInputs {
            requests: None,
            usage_shown: false,
            ..inputs(&[])
        });
        assert!(
            notes.iter().all(|note| !note.contains("requested")),
            "{notes:?}"
        );
    }

    #[test]
    fn output_follows_the_flag() {
        assert_eq!(Output::json(true), Output::Json);
        assert_eq!(Output::json(false), Output::Table);
        assert_eq!(Output::default(), Output::Table);
    }

    // --- eks logs --------------------------------------------------------

    fn record(stream: &str) -> Record {
        Record {
            timestamp: 1_791_354_062_123,
            id: "3780".to_owned(),
            namespace: "shop".to_owned(),
            pod: "api-7d9f-xk2".to_owned(),
            container: "app".to_owned(),
            instance: "3f2a".to_owned(),
            stream: stream.to_owned(),
            log: "panic: out of memory".to_owned(),
        }
    }

    #[test]
    fn a_cloudwatch_line_names_its_source_pod_container_and_stream() {
        let record = record("stderr");
        let line = log_line(&LogLine::cloudwatch(&record)).unwrap();

        assert_eq!(
            line,
            r#"{"time":"2026-10-07T06:21:02.123Z","source":"cloudwatch","namespace":"shop","pod":"api-7d9f-xk2","container":"app","stream":"stderr","log":"panic: out of memory"}"#
        );
    }

    #[test]
    fn an_api_line_has_every_field_with_a_null_stream() {
        let line = log_line(&LogLine {
            time: Some("2026-10-07T06:21:02.123456789Z".parse().unwrap()),
            source: LogSource::Api,
            namespace: "shop",
            pod: "api-live-1",
            container: "app",
            stream: None,
            log: "GET /orders 200",
        })
        .unwrap();

        assert_eq!(
            parse(&line),
            json!({
                "time": "2026-10-07T06:21:02.123Z",
                "source": "api",
                "namespace": "shop",
                "pod": "api-live-1",
                "container": "app",
                "stream": null,
                "log": "GET /orders 200",
            })
        );
    }

    #[test]
    fn a_log_line_is_one_line_however_its_text_reads() {
        // A container can print anything; none of it may break the framing.
        let text = "tab\there \"quoted\" \u{1b}[31mred\u{1b}[0m back\\slash \u{e9}t\u{e9}";
        let line = log_line(&LogLine {
            time: None,
            source: LogSource::Api,
            namespace: "shop",
            pod: "api-live-1",
            container: "app",
            stream: None,
            log: text,
        })
        .unwrap();

        assert!(!line.contains('\n'), "{line}");
        assert_eq!(parse(&line)["log"], text);
    }

    #[test]
    fn an_unknown_time_or_a_field_fluent_bit_left_out_is_null_not_empty() {
        let mut bare = record("");
        bare.namespace = String::new();
        bare.container = String::new();
        bare.timestamp = i64::MAX;

        let value = parse(&log_line(&LogLine::cloudwatch(&bare)).unwrap());

        assert_eq!(value["time"], Value::Null);
        assert_eq!(value["namespace"], Value::Null);
        assert_eq!(value["container"], Value::Null);
        assert_eq!(value["stream"], Value::Null);
        assert_eq!(value["pod"], "api-7d9f-xk2");
    }

    #[test]
    fn an_empty_line_is_an_empty_string_not_a_null() {
        let mut blank = record("stdout");
        blank.log = String::new();

        let value = parse(&log_line(&LogLine::cloudwatch(&blank)).unwrap());

        assert_eq!(value["log"], "");
    }

    #[test]
    fn times_print_to_the_millisecond_whatever_precision_they_were_read_at() {
        let at = |text: &str| {
            let line = log_line(&LogLine {
                time: Some(text.parse().unwrap()),
                source: LogSource::Api,
                namespace: "shop",
                pod: "p",
                container: "c",
                stream: None,
                log: "",
            })
            .unwrap();
            parse(&line)["time"].as_str().unwrap().to_owned()
        };

        assert_eq!(at("2026-10-07T06:21:02Z"), "2026-10-07T06:21:02.000Z");
        assert_eq!(at("2026-10-07T06:21:02.1Z"), "2026-10-07T06:21:02.100Z");
        assert_eq!(
            at("2026-10-07T06:21:02.123999999Z"),
            "2026-10-07T06:21:02.123Z"
        );
    }
}
