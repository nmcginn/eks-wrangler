//! One pod, reduced to a row — and in particular, its `STATUS`.
//!
//! The other columns are lookups. `STATUS` is not: the string `kubectl` prints
//! appears nowhere in the API. `pod.status.phase` only ever holds one of five
//! words, and none of them is `CrashLoopBackOff`, `Terminating`, `Init:0/2`, or
//! `Evicted` — the four a person actually looks for. Those are derived from the
//! container statuses underneath, and the derivation is order-dependent and
//! full of special cases.
//!
//! So this module reimplements the derivation `kubectl get pods` performs, on
//! purpose and deliberately faithfully, including the parts that look odd:
//!
//! - The init containers are walked in order and the *first* one that has not
//!   exited cleanly decides the status, as `Init:<n>/<total>` or `Init:<reason>`.
//! - A *sidecar* — an init container with `restartPolicy: Always` — is skipped
//!   by that walk once it has started, counts towards the ready fraction, and
//!   is the only init container whose restarts survive into the final count.
//! - The `RESTARTS` cell carries *when* the newest surviving restart happened
//!   — `9 (5m ago)` — taken from the newest `lastState.terminated.finishedAt`
//!   across exactly the containers whose counts survived. The recency follows
//!   the count rather than being gathered separately, so the two halves of the
//!   cell can never describe different sets of containers.
//! - The app containers are walked *backwards*, so when several are unhappy the
//!   first one in the spec is the one named.
//! - A pod being deleted reads `Terminating`, except on a lost node, where it
//!   reads `Unknown`.
//!
//! Everything here is a pure function over a `Pod` and an explicit `now`, so
//! each of those cases is a fixture and a test rather than a cluster someone
//! has to break on purpose.

use k8s_openapi::api::core::v1::{
    Container, ContainerState, ContainerStateTerminated, ContainerStatus, Pod, PodCondition,
    PodStatus,
};
use k8s_openapi::jiff::Timestamp;

use crate::format;
use crate::k8s::metrics::Usage;
use crate::k8s::pods::is_sidecar;
use crate::k8s::quantity::{self, Quantity};
use crate::theme::Severity;

/// Shown wherever the API server left a field empty, as elsewhere in the tool.
const UNKNOWN: &str = "-";

/// The reason a pod carries when the node running it stopped answering. Its
/// pods are not really terminating — nobody can confirm anything about them.
const NODE_LOST: &str = "NodeLost";

/// One pod, as a table row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodRow {
    pub namespace: String,
    pub name: String,
    /// Containers ready over containers expected, `kubectl`'s `1/2`.
    pub ready: String,
    /// `kubectl`'s wording — see the module docs.
    pub status: String,
    /// How alarming that status is. Carried here rather than derived at the
    /// call site so the CLI table and the dashboard cannot disagree about it.
    pub severity: Severity,
    pub restarts: i32,
    /// How long before `now` the newest surviving restart finished, formatted
    /// the way [`PodRow::age`] is.
    ///
    /// `None` for a pod that has never restarted, which reads as a bare count.
    /// Pre-formatted rather than a `Timestamp` for the same reason `age` is:
    /// every row in a listing is rendered against the one instant passed to
    /// [`PodRow::from_pod`], so rendering cannot reach for a clock of its own.
    pub restart_age: Option<String>,
    /// The same moment, unformatted, for [`crate::k8s::pods::order`] to sort on.
    ///
    /// Ordering needs the instant rather than the words: `2m` and `1h` do not
    /// compare as strings, and rounding "eight seconds ago" to `8s` throws away
    /// exactly the precision that separates two pods crashing in the same
    /// minute. It is carried beside `restart_age` rather than replacing it so
    /// that rendering still has nothing to compute.
    pub last_restart: Option<Timestamp>,
    pub age: String,
    /// The moment the pod was created, unformatted, for
    /// [`crate::k8s::pods::order`] to sort on.
    ///
    /// Carried beside `age` for the same reason `last_restart` is carried
    /// beside `restart_age`: `2m` and `1h` do not compare as strings, and the
    /// rounding that makes an age readable throws away exactly the precision
    /// that separates two pods rolled out in the same minute. `None` where the
    /// API server returned no `creationTimestamp`, which is also what makes
    /// `age` read as `-`.
    pub created_at: Option<Timestamp>,
    /// Cores the pod is actually burning, from metrics-server.
    ///
    /// `None` where there is no metrics-server, where it has not sampled this
    /// pod yet, or where a container's figure would not parse — never a zero,
    /// which would draw an idle pod. See `metrics::pod_usage`.
    pub cpu_used: Option<Quantity>,
    /// Memory the pod is actually holding, from metrics-server.
    pub memory_used: Option<Quantity>,
    /// The node the pod landed on, or `-` while it is still unscheduled.
    pub node: String,
}

impl PodRow {
    /// Build a row from a `Pod`, as of `now`.
    ///
    /// `now` is a parameter rather than a call to the clock so the age column
    /// is testable and so every row in one listing shares a single instant.
    ///
    /// `used` is what metrics-server last sampled for this pod, already summed
    /// across its containers. `None` covers every reason there is no figure —
    /// no metrics-server, or a pod it has not reached — and all of them render
    /// the same way, because to a reader they mean the same thing.
    #[must_use]
    pub fn from_pod(pod: &Pod, used: Option<Usage>, now: Timestamp) -> Self {
        let derived = derive(pod);
        // Read once and used for both cells, so the formatted age and the
        // instant the ordering sorts on cannot describe different moments.
        let created_at = pod
            .metadata
            .creation_timestamp
            .as_ref()
            .map(|created| created.0);

        Self {
            namespace: pod
                .metadata
                .namespace
                .clone()
                .unwrap_or_else(|| UNKNOWN.to_owned()),
            name: pod
                .metadata
                .name
                .clone()
                .unwrap_or_else(|| UNKNOWN.to_owned()),
            ready: format!("{}/{}", derived.ready, derived.total),
            severity: severity(&derived.status, derived.ready, derived.total),
            status: derived.status,
            restarts: derived.restarts,
            restart_age: derived
                .last_restart
                .map(|at| format::human_duration(now.duration_since(at))),
            last_restart: derived.last_restart,
            age: created_at.map_or_else(
                || UNKNOWN.to_owned(),
                |created| format::human_duration(now.duration_since(created)),
            ),
            created_at,
            cpu_used: used.and_then(|usage| usage.cpu),
            memory_used: used.and_then(|usage| usage.memory),
            node: pod
                .spec
                .as_ref()
                .and_then(|spec| spec.node_name.as_deref())
                .filter(|name| !name.is_empty())
                .map_or_else(|| UNKNOWN.to_owned(), str::to_owned),
        }
    }
}

/// What the container statuses add up to.
struct Derived {
    status: String,
    ready: usize,
    total: usize,
    restarts: i32,
    /// The newest restart among the containers `restarts` counted, if any.
    last_restart: Option<Timestamp>,
}

/// Walk a pod's containers the way `kubectl get pods` does.
fn derive(pod: &Pod) -> Derived {
    let spec = pod.spec.as_ref();
    let status = pod.status.as_ref();
    let init_specs = spec
        .and_then(|spec| spec.init_containers.as_deref())
        .unwrap_or_default();

    // A sidecar keeps running alongside the app containers, so it is one of the
    // containers the ready fraction is out of. A plain init container has
    // exited by then and is not.
    let total = spec.map_or(0, |spec| spec.containers.len())
        + init_specs.iter().filter(|c| is_sidecar(c)).count();

    let phase = status.and_then(|s| s.phase.as_deref()).unwrap_or_default();
    let mut reason = base_reason(status, phase);

    let init = init_phase(status, init_specs);
    let mut ready = init.ready;
    let mut restarts = init.restarts;
    let mut last_restart = init.last_restart;
    if let Some(blocked) = init.reason.clone() {
        reason = blocked;
    }

    // Once `Initialized` is true the init phase is over whatever the init
    // statuses still say — a sidecar that started and then began crashing
    // leaves one reporting forever — and what the app containers are doing is
    // the news.
    let initialised = condition(status, "Initialized").is_some_and(|c| c.status == "True");
    if init.reason.is_none() || initialised {
        let steady = steady_state(status);
        ready += steady.ready;
        // Only a sidecar's restarts survive: a plain init container having
        // restarted before the pod came up is history, not a live warning. The
        // recency is discarded with the count it belonged to, so a finished
        // init container cannot leave its timestamp behind on a `0`.
        restarts = init.sidecar_restarts.saturating_add(steady.restarts);
        last_restart = newest(init.sidecar_last_restart, steady.last_restart);
        if let Some(current) = steady.reason {
            reason = current;
        }

        // A pod whose Job container finished while a sidecar keeps running is
        // not `Completed` — there is still something on the node.
        if reason == "Completed" && steady.any_running {
            reason = String::from(
                if condition(status, "Ready").is_some_and(|c| c.status == "True") {
                    "Running"
                } else {
                    "NotReady"
                },
            );
        }
    }

    if let Some(deleted) = deletion_reason(pod, phase) {
        reason = deleted;
    }

    Derived {
        // A pod with no status at all — one caught between admission and the
        // first kubelet report — has nothing to say, and an empty cell reads
        // like a rendering bug.
        status: if reason.is_empty() {
            String::from("Unknown")
        } else {
            reason
        },
        ready,
        total,
        restarts,
        last_restart,
    }
}

/// The status a pod has before any container is consulted.
///
/// `status.reason` outranks the phase when it is set — `Evicted` says far more
/// than `Failed` — and a gated pod outranks both, because `Pending` would send
/// the reader off to look at node capacity for a pod no scheduler has been
/// allowed to consider yet.
fn base_reason(status: Option<&PodStatus>, phase: &str) -> String {
    if condition(status, "PodScheduled").and_then(|c| c.reason.as_deref())
        == Some("SchedulingGated")
    {
        return String::from("SchedulingGated");
    }

    status
        .and_then(|s| s.reason.as_deref())
        .filter(|reason| !reason.is_empty())
        .unwrap_or(phase)
        .to_owned()
}

/// What the init containers add up to.
#[derive(Default)]
struct Init {
    /// The first init container that is not finished, if there is one. `None`
    /// means initialisation is not what is holding the pod up.
    reason: Option<String>,
    ready: usize,
    restarts: i32,
    sidecar_restarts: i32,
    /// The newest restart across every init container walked.
    last_restart: Option<Timestamp>,
    /// The same, restricted to the sidecars — the subset that survives once
    /// initialisation is over.
    sidecar_last_restart: Option<Timestamp>,
}

/// Walk the init containers in order, stopping at the first one that has not
/// exited cleanly — that one is what the pod is waiting for.
fn init_phase(status: Option<&PodStatus>, specs: &[Container]) -> Init {
    let mut init = Init::default();

    for (index, container) in status
        .and_then(|s| s.init_container_statuses.as_deref())
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        let sidecar = specs
            .iter()
            .find(|spec| spec.name == container.name)
            .is_some_and(is_sidecar);

        let restarted = last_terminated_at(container);
        init.restarts = init.restarts.saturating_add(container.restart_count);
        init.last_restart = newest(init.last_restart, restarted);
        if sidecar {
            init.sidecar_restarts = init
                .sidecar_restarts
                .saturating_add(container.restart_count);
            init.sidecar_last_restart = newest(init.sidecar_last_restart, restarted);
        }

        let terminated = state(container, |state| state.terminated.as_ref());

        // This one is done; the next decides the status.
        if terminated.is_some_and(|terminated| terminated.exit_code == 0) {
            continue;
        }
        // A started sidecar is not blocking initialisation — it is meant to
        // still be running — and it counts towards the ready fraction.
        if sidecar && container.started == Some(true) {
            if container.ready {
                init.ready += 1;
            }
            continue;
        }

        // `PodInitializing` is the kubelet saying "not this one's turn yet",
        // which is the plain progress case rather than a reason to report.
        let waiting = state(container, |state| state.waiting.as_ref())
            .and_then(|waiting| waiting.reason.as_deref())
            .filter(|reason| !reason.is_empty() && *reason != "PodInitializing");

        init.reason = Some(if let Some(terminated) = terminated {
            format!("Init:{}", exit_reason(terminated))
        } else if let Some(waiting) = waiting {
            format!("Init:{waiting}")
        } else {
            format!("Init:{index}/{}", specs.len())
        });
        break;
    }

    init
}

/// What the app containers add up to.
#[derive(Default)]
struct Steady {
    /// The last unhappy container seen, if any. Because the walk runs
    /// backwards, that is the *first* one in the spec.
    reason: Option<String>,
    ready: usize,
    restarts: i32,
    /// The newest restart across the app containers.
    last_restart: Option<Timestamp>,
    /// Whether anything is still up, which is what separates a finished Job
    /// from one whose sidecar never exits.
    any_running: bool,
}

/// Walk the app containers backwards, so that when several are unhappy the one
/// reported is the first in the spec — the one a reader thinks of as *the*
/// container.
fn steady_state(status: Option<&PodStatus>) -> Steady {
    let mut steady = Steady::default();

    for container in status
        .and_then(|s| s.container_statuses.as_deref())
        .unwrap_or_default()
        .iter()
        .rev()
    {
        steady.restarts = steady.restarts.saturating_add(container.restart_count);
        steady.last_restart = newest(steady.last_restart, last_terminated_at(container));

        let waiting = state(container, |state| state.waiting.as_ref())
            .and_then(|waiting| waiting.reason.as_deref())
            .filter(|reason| !reason.is_empty());
        let terminated = state(container, |state| state.terminated.as_ref());

        if let Some(waiting) = waiting {
            steady.reason = Some(waiting.to_owned());
        } else if let Some(terminated) = terminated {
            steady.reason = Some(exit_reason(terminated));
        } else if container.ready && state(container, |state| state.running.as_ref()).is_some() {
            steady.any_running = true;
            steady.ready += 1;
        }
    }

    steady
}

/// What a pod being deleted should read as, if it is being deleted at all.
///
/// A pod on a node that stopped answering is not shutting down — nobody can
/// confirm anything about it — and a pod that already finished stopped long
/// ago, so neither should read `Terminating`.
fn deletion_reason(pod: &Pod, phase: &str) -> Option<String> {
    pod.metadata.deletion_timestamp.as_ref()?;

    if pod.status.as_ref().and_then(|s| s.reason.as_deref()) == Some(NODE_LOST) {
        return Some(String::from("Unknown"));
    }
    if matches!(phase, "Succeeded" | "Failed") {
        return None;
    }

    Some(String::from("Terminating"))
}

/// How a container ended, when it did not end cleanly.
///
/// The reason is usually filled in (`Error`, `OOMKilled`, `Completed`); when it
/// is not, the exit code or signal is all there is to go on, and it still beats
/// an empty cell.
fn exit_reason(terminated: &ContainerStateTerminated) -> String {
    match terminated
        .reason
        .as_deref()
        .filter(|reason| !reason.is_empty())
    {
        Some(reason) => reason.to_owned(),
        None => match terminated.signal {
            Some(signal) if signal != 0 => format!("Signal:{signal}"),
            _ => format!("ExitCode:{}", terminated.exit_code),
        },
    }
}

/// When a container's *previous* run ended, if it had one.
///
/// This is what makes a restart count recent or historical. `lastState` is only
/// populated once the kubelet has restarted a container, so its absence is the
/// ordinary case rather than missing data — and `finishedAt` can still be unset
/// on a container killed before it was ever seen to stop, which reads the same
/// way: no recency to show.
fn last_terminated_at(container: &ContainerStatus) -> Option<Timestamp> {
    Some(
        container
            .last_state
            .as_ref()?
            .terminated
            .as_ref()?
            .finished_at
            .as_ref()?
            .0,
    )
}

/// The later of two moments, tolerating either being absent.
///
/// A fold rather than a `max` over an iterator because the two sides are
/// gathered in different places — the init walk stops early, and the app walk
/// runs backwards.
fn newest(current: Option<Timestamp>, candidate: Option<Timestamp>) -> Option<Timestamp> {
    match (current, candidate) {
        (Some(current), Some(candidate)) => Some(current.max(candidate)),
        (current, candidate) => current.or(candidate),
    }
}

/// One arm of a container's current state, if it is in that state.
fn state<T>(
    container: &ContainerStatus,
    pick: impl Fn(&ContainerState) -> Option<&T>,
) -> Option<&T> {
    container.state.as_ref().and_then(pick)
}

/// One of a pod's conditions, by type.
fn condition<'a>(status: Option<&'a PodStatus>, kind: &str) -> Option<&'a PodCondition> {
    status?
        .conditions
        .as_ref()?
        .iter()
        .find(|condition| condition.type_ == kind)
}

/// How alarming a status is.
///
/// The calm words are listed, the settling ones are listed, and everything else
/// is treated as a problem. That way round on purpose: the set of things that
/// can go wrong with a pod grows with every Kubernetes release, and a new
/// failure reason no one here has heard of should arrive coloured as a failure
/// rather than quietly as fine.
#[must_use]
fn severity(status: &str, ready: usize, total: usize) -> Severity {
    let (initialising, reason) = match status.strip_prefix("Init:") {
        Some(rest) => (true, rest),
        None => (false, status),
    };

    match reason {
        "Unknown" => Severity::Unknown,
        "Completed" | "Succeeded" if !initialising => Severity::Ok,
        // Every container up and ready is the only shape of `Running` that
        // deserves the calm colour; `1/2 Running` is a pod in trouble.
        "Running" if !initialising && total > 0 && ready == total => Severity::Ok,
        "Running" | "Pending" | "ContainerCreating" | "PodInitializing" | "Terminating"
        | "SchedulingGated" | "NotReady" => Severity::Warn,
        // `Init:0/2` is progress; `Init:CrashLoopBackOff` is not.
        progress if is_init_progress(initialising, progress) => Severity::Warn,
        _ => Severity::Critical,
    }
}

/// Whether an `Init:` suffix is the `<n>/<total>` progress form rather than a
/// reason a container gave.
fn is_init_progress(initialising: bool, reason: &str) -> bool {
    initialising
        && reason.split_once('/').is_some_and(|(done, total)| {
            !done.is_empty()
                && !total.is_empty()
                && done.chars().all(|c| c.is_ascii_digit())
                && total.chars().all(|c| c.is_ascii_digit())
        })
}

/// Whether a listing has live usage worth two columns.
///
/// Same rule as the node table: a cluster with no metrics-server — the default
/// on EKS, where it is not installed for you — gains no empty columns, and the
/// footnote carries the news instead. `any` rather than `all`, so one pod the
/// sampler has not reached does not cost everyone else their figures.
fn shows_usage(rows: &[PodRow]) -> bool {
    rows.iter()
        .any(|row| row.cpu_used.is_some() || row.memory_used.is_some())
}

/// The `RESTARTS` cell: the count, and when the newest restart happened.
///
/// A bare count cannot tell a pod that crashed nine times last Tuesday from one
/// crashing right now, which is the only question anyone asks of the column. A
/// pod that has never restarted keeps the bare count — `0 (— ago)` would be
/// noise on every healthy row, which is most of them.
fn restarts_cell(row: &PodRow) -> String {
    match &row.restart_age {
        Some(age) => format!("{} ({age} ago)", row.restarts),
        None => row.restarts.to_string(),
    }
}

/// One usage cell: the figure, or `-` where there is not one.
///
/// No percentage, unlike the node table's: a pod has no allocatable of its own
/// to be a share of. What it *asked* for would be the honest denominator, and
/// that is a column of its own rather than something to smuggle in here.
fn usage_cell(amount: Option<Quantity>, show: fn(Quantity) -> String) -> String {
    amount.map_or_else(|| UNKNOWN.to_owned(), show)
}

/// Render the `eks pods` table.
///
/// `cluster` is the human label used in the empty-list message, so a user who
/// typed the wrong `--context` or the wrong namespace finds out from the answer
/// rather than from a bare header.
///
/// `notes` are appended under the table — see [`usage_unavailable`]. They are
/// dropped when there are no pods, where a footnote about missing columns would
/// only be noise on top of a bigger problem.
#[must_use]
pub fn render(
    rows: &[PodRow],
    cluster: &str,
    scope: &super::Scope,
    selectors: &super::Selectors,
    notes: &[String],
) -> String {
    if rows.is_empty() {
        return empty(cluster, scope, selectors);
    }

    let namespaced = scope.needs_namespace_column();
    let usage = shows_usage(rows);
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            let mut cells = Vec::with_capacity(9);
            if namespaced {
                cells.push(row.namespace.clone());
            }
            cells.extend([
                row.name.clone(),
                row.ready.clone(),
                row.status.clone(),
                restarts_cell(row),
            ]);
            if usage {
                cells.push(usage_cell(row.cpu_used, quantity::cpu));
                cells.push(usage_cell(row.memory_used, quantity::memory));
            }
            cells.extend([row.age.clone(), row.node.clone()]);
            cells
        })
        .collect();

    // CPU and MEMORY sit with STATUS and RESTARTS, the other columns about how
    // the pod is doing, rather than at the end: a pod that is unhappy and one
    // that is burning a core are usually the same investigation. AGE and NODE
    // stay last, where every `kubectl get pods` leaves them.
    let mut headers = vec!["NAME", "READY", "STATUS", "RESTARTS"];
    if usage {
        headers.extend(["CPU", "MEMORY"]);
    }
    headers.extend(["AGE", "NODE"]);
    if namespaced {
        headers.insert(0, "NAMESPACE");
    }

    let table = format::table(&headers, &cells);

    if notes.is_empty() {
        table
    } else {
        format!("{table}\n\n{}", notes.join("\n\n"))
    }
}

/// The footnote shown when there is no live usage to put in a column.
///
/// The columns are absent rather than empty in this case, so the note has to
/// say what is missing — otherwise a perfectly ordinary table silently answers
/// a question the user thought they had asked. Worded like the node table's for
/// the same reason it looks the same: it is the same failure.
///
/// `explanation` is `k8s::metrics::explain`'s sentence, which for the usual
/// cause says what to install.
#[must_use]
pub fn usage_unavailable(explanation: &str) -> String {
    format!("CPU and MEMORY are not shown because live usage could not be read.\n{explanation}")
}

/// What to say instead of an empty table.
fn empty(cluster: &str, scope: &super::Scope, selectors: &super::Selectors) -> String {
    // A filter that matches nothing must not read like an empty namespace, or
    // the user goes looking for pods that are there but filtered out. When a
    // selector is active it, not the scope, is the likeliest reason for a blank
    // listing, so it leads.
    if let Some(note) = selector_note(selectors) {
        return format!("{cluster} has no pods matching {note}.");
    }

    match scope {
        super::Scope::Namespace(name) => format!(
            "{cluster} has no pods in namespace {name:?}.\n\
             Try `eks pods --all-namespaces`, or `-n <namespace>` to look somewhere else."
        ),
        super::Scope::All => format!(
            "{cluster} reports no pods in any namespace you can see.\n\
             If you expected some, check you are on the right cluster with `eks contexts`."
        ),
    }
}

/// A phrase naming the active selectors, or `None` when none are set.
fn selector_note(selectors: &super::Selectors) -> Option<String> {
    match (&selectors.label, &selectors.field) {
        (Some(label), Some(field)) => Some(format!(
            "label selector `{label}` and field selector `{field}`"
        )),
        (Some(label), None) => Some(format!("label selector `{label}`")),
        (None, Some(field)) => Some(format!("field selector `{field}`")),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use k8s_openapi::api::core::v1::{ContainerStateRunning, ContainerStateWaiting, PodSpec};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};
    use k8s_openapi::jiff::SignedDuration;

    use super::super::{Scope, Selectors};
    use super::*;

    /// Most rendering tests are not about selectors; this is the "no filter"
    /// case they pass so the signature reads at the call site.
    fn unfiltered() -> Selectors {
        Selectors::default()
    }

    const NODE: &str = "ip-10-0-1-9.ec2.internal";

    fn now() -> Timestamp {
        "2026-08-18T12:00:00Z".parse().unwrap()
    }

    fn ago(minutes: i64) -> Time {
        Time(now() - SignedDuration::from_mins(minutes))
    }

    fn container(name: &str) -> Container {
        Container {
            name: name.to_owned(),
            ..Default::default()
        }
    }

    fn sidecar(name: &str) -> Container {
        Container {
            restart_policy: Some("Always".to_owned()),
            ..container(name)
        }
    }

    fn status(name: &str, state: ContainerState, ready: bool, restarts: i32) -> ContainerStatus {
        ContainerStatus {
            name: name.to_owned(),
            ready,
            restart_count: restarts,
            state: Some(state),
            ..Default::default()
        }
    }

    /// A container status carrying the wreckage of a previous run — what the
    /// kubelet leaves behind after it restarts a container, and the only place
    /// the recency of a restart is recorded.
    fn restarted(
        name: &str,
        state: ContainerState,
        restarts: i32,
        finished: Option<Time>,
    ) -> ContainerStatus {
        ContainerStatus {
            last_state: Some(ContainerState {
                terminated: Some(ContainerStateTerminated {
                    reason: Some("Error".to_owned()),
                    exit_code: 1,
                    finished_at: finished,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..status(name, state, false, restarts)
        }
    }

    fn running() -> ContainerState {
        ContainerState {
            running: Some(ContainerStateRunning::default()),
            ..Default::default()
        }
    }

    fn waiting(reason: &str) -> ContainerState {
        ContainerState {
            waiting: Some(ContainerStateWaiting {
                reason: Some(reason.to_owned()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn terminated(reason: Option<&str>, exit_code: i32) -> ContainerState {
        ContainerState {
            terminated: Some(ContainerStateTerminated {
                reason: reason.map(str::to_owned),
                exit_code,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn condition(kind: &str, state: &str, reason: Option<&str>) -> PodCondition {
        PodCondition {
            type_: kind.to_owned(),
            status: state.to_owned(),
            reason: reason.map(str::to_owned),
            ..Default::default()
        }
    }

    fn pod(spec: PodSpec, status: PodStatus) -> Pod {
        Pod {
            metadata: ObjectMeta {
                name: Some("api-7c9f".to_owned()),
                namespace: Some("payments".to_owned()),
                creation_timestamp: Some(ago(90)),
                ..Default::default()
            },
            // Deliberately not defaulting `node_name` here: struct-update
            // syntax would silently override a test that means to leave a pod
            // unscheduled.
            spec: Some(spec),
            status: Some(status),
        }
    }

    /// A plausible, healthy pod: one container, up and ready. Tests change one
    /// thing at a time from here so it is obvious what each is about.
    fn healthy() -> Pod {
        pod(
            PodSpec {
                containers: vec![container("app")],
                node_name: Some(NODE.to_owned()),
                ..Default::default()
            },
            PodStatus {
                phase: Some("Running".to_owned()),
                conditions: Some(vec![condition("Ready", "True", None)]),
                container_statuses: Some(vec![status("app", running(), true, 0)]),
                ..Default::default()
            },
        )
    }

    #[test]
    fn a_healthy_pod_reads_as_running_and_fully_ready() {
        let row = PodRow::from_pod(&healthy(), None, now());

        assert_eq!(row.namespace, "payments");
        assert_eq!(row.name, "api-7c9f");
        assert_eq!(row.ready, "1/1");
        assert_eq!(row.status, "Running");
        assert_eq!(row.severity, Severity::Ok);
        assert_eq!(row.restarts, 0);
        assert_eq!(row.age, "90m");
        assert_eq!(row.node, NODE);
    }

    #[test]
    fn a_crashlooping_container_reads_as_crashloopbackoff() {
        // The word people actually grep for, and it exists nowhere in the API:
        // `phase` still says `Running`.
        let pod = pod(
            PodSpec {
                containers: vec![container("app")],
                ..Default::default()
            },
            PodStatus {
                phase: Some("Running".to_owned()),
                container_statuses: Some(vec![status(
                    "app",
                    waiting("CrashLoopBackOff"),
                    false,
                    7,
                )]),
                ..Default::default()
            },
        );

        let row = PodRow::from_pod(&pod, None, now());
        assert_eq!(row.status, "CrashLoopBackOff");
        assert_eq!(row.ready, "0/1");
        assert_eq!(row.restarts, 7);
        assert_eq!(row.severity, Severity::Critical);
    }

    #[test]
    fn a_pod_being_deleted_reads_as_terminating() {
        let mut terminating = healthy();
        terminating.metadata.deletion_timestamp = Some(ago(1));

        let row = PodRow::from_pod(&terminating, None, now());
        assert_eq!(row.status, "Terminating");
        // Deliberate and temporary — worth noticing during a drain, not alarming.
        assert_eq!(row.severity, Severity::Warn);
    }

    #[test]
    fn a_pod_that_already_finished_is_not_relabelled_terminating() {
        // Garbage collection deleting a Succeeded pod is not the pod stopping;
        // it stopped long ago, and `Completed` is still the useful word.
        let mut finished = pod(
            PodSpec {
                containers: vec![container("job")],
                ..Default::default()
            },
            PodStatus {
                phase: Some("Succeeded".to_owned()),
                container_statuses: Some(vec![status(
                    "job",
                    terminated(Some("Completed"), 0),
                    false,
                    0,
                )]),
                ..Default::default()
            },
        );
        finished.metadata.deletion_timestamp = Some(ago(1));

        assert_eq!(PodRow::from_pod(&finished, None, now()).status, "Completed");
    }

    #[test]
    fn a_pod_on_a_lost_node_reads_as_unknown_rather_than_terminating() {
        // Nobody can confirm anything about it, which is a different problem
        // from a pod that is shutting down, and it must not look like one.
        let mut lost = healthy();
        lost.metadata.deletion_timestamp = Some(ago(5));
        if let Some(status) = lost.status.as_mut() {
            status.reason = Some(NODE_LOST.to_owned());
        }

        let row = PodRow::from_pod(&lost, None, now());
        assert_eq!(row.status, "Unknown");
        assert_eq!(row.severity, Severity::Unknown);
    }

    #[test]
    fn a_pod_still_running_its_init_containers_shows_how_far_it_has_got() {
        let pod = pod(
            PodSpec {
                containers: vec![container("app")],
                init_containers: Some(vec![container("migrate"), container("seed")]),
                ..Default::default()
            },
            PodStatus {
                phase: Some("Pending".to_owned()),
                init_container_statuses: Some(vec![status(
                    "migrate",
                    waiting("PodInitializing"),
                    false,
                    0,
                )]),
                ..Default::default()
            },
        );

        let row = PodRow::from_pod(&pod, None, now());
        // `PodInitializing` is the kubelet saying "not started yet", so the
        // progress count is the news, not the reason.
        assert_eq!(row.status, "Init:0/2");
        assert_eq!(row.ready, "0/1");
        // Progress, not a problem.
        assert_eq!(row.severity, Severity::Warn);
    }

    #[test]
    fn the_init_progress_counts_the_containers_that_finished_cleanly() {
        let pod = pod(
            PodSpec {
                containers: vec![container("app")],
                init_containers: Some(vec![
                    container("migrate"),
                    container("seed"),
                    container("warm"),
                ]),
                ..Default::default()
            },
            PodStatus {
                phase: Some("Pending".to_owned()),
                init_container_statuses: Some(vec![
                    status("migrate", terminated(Some("Completed"), 0), false, 0),
                    status("seed", running(), false, 0),
                ]),
                ..Default::default()
            },
        );

        assert_eq!(PodRow::from_pod(&pod, None, now()).status, "Init:1/3");
    }

    #[test]
    fn an_init_container_that_failed_names_the_reason_it_gave() {
        let cases = [
            (terminated(Some("Error"), 1), "Init:Error"),
            (terminated(None, 137), "Init:ExitCode:137"),
            (waiting("ImagePullBackOff"), "Init:ImagePullBackOff"),
            (waiting("CrashLoopBackOff"), "Init:CrashLoopBackOff"),
        ];

        for (state, expected) in cases {
            let pod = pod(
                PodSpec {
                    containers: vec![container("app")],
                    init_containers: Some(vec![container("migrate"), container("seed")]),
                    ..Default::default()
                },
                PodStatus {
                    phase: Some("Pending".to_owned()),
                    init_container_statuses: Some(vec![status("migrate", state, false, 3)]),
                    ..Default::default()
                },
            );

            let row = PodRow::from_pod(&pod, None, now());
            assert_eq!(row.status, expected);
            // A named failure is a failure, unlike bare `Init:n/total`.
            assert_eq!(row.severity, Severity::Critical, "{expected}");
            // While initialising, the init container's restarts are the ones
            // worth showing.
            assert_eq!(row.restarts, 3, "{expected}");
        }
    }

    #[test]
    fn a_killed_container_falls_back_to_its_signal_when_it_gave_no_reason() {
        let killed = ContainerState {
            terminated: Some(ContainerStateTerminated {
                signal: Some(9),
                exit_code: 137,
                ..Default::default()
            }),
            ..Default::default()
        };

        let pod = pod(
            PodSpec {
                containers: vec![container("app")],
                ..Default::default()
            },
            PodStatus {
                phase: Some("Failed".to_owned()),
                container_statuses: Some(vec![status("app", killed, false, 0)]),
                ..Default::default()
            },
        );

        assert_eq!(PodRow::from_pod(&pod, None, now()).status, "Signal:9");
    }

    #[test]
    fn a_finished_job_pod_reads_as_completed() {
        let pod = pod(
            PodSpec {
                containers: vec![container("job")],
                ..Default::default()
            },
            PodStatus {
                phase: Some("Succeeded".to_owned()),
                container_statuses: Some(vec![status(
                    "job",
                    terminated(Some("Completed"), 0),
                    false,
                    0,
                )]),
                ..Default::default()
            },
        );

        let row = PodRow::from_pod(&pod, None, now());
        assert_eq!(row.status, "Completed");
        assert_eq!(row.ready, "0/1");
        // A job that did its work is good news, not a warning.
        assert_eq!(row.severity, Severity::Ok);
    }

    #[test]
    fn a_completed_container_beside_a_running_one_is_not_completed() {
        // The classic stuck Job: the work finished but a sidecar never exits,
        // so the pod is still on the node. Saying `Completed` would hide that.
        let statuses = vec![
            status("job", terminated(Some("Completed"), 0), false, 0),
            status("proxy", running(), true, 0),
        ];
        let spec = PodSpec {
            containers: vec![container("job"), container("proxy")],
            ..Default::default()
        };

        let not_ready = pod(
            spec.clone(),
            PodStatus {
                phase: Some("Running".to_owned()),
                container_statuses: Some(statuses.clone()),
                ..Default::default()
            },
        );
        assert_eq!(PodRow::from_pod(&not_ready, None, now()).status, "NotReady");

        let ready = pod(
            spec,
            PodStatus {
                phase: Some("Running".to_owned()),
                conditions: Some(vec![condition("Ready", "True", None)]),
                container_statuses: Some(statuses),
                ..Default::default()
            },
        );
        assert_eq!(PodRow::from_pod(&ready, None, now()).status, "Running");
    }

    #[test]
    fn a_started_sidecar_counts_towards_the_ready_fraction() {
        // Its restarts are the only init-container restarts that survive, too:
        // a sidecar restarting is a live problem, a finished init container
        // having restarted is history.
        let mut proxy = status("proxy", running(), true, 3);
        proxy.started = Some(true);

        let pod = pod(
            PodSpec {
                containers: vec![container("app")],
                init_containers: Some(vec![sidecar("proxy"), container("migrate")]),
                ..Default::default()
            },
            PodStatus {
                phase: Some("Running".to_owned()),
                conditions: Some(vec![condition("Ready", "True", None)]),
                init_container_statuses: Some(vec![
                    proxy,
                    status("migrate", terminated(Some("Completed"), 0), false, 5),
                ]),
                container_statuses: Some(vec![status("app", running(), true, 1)]),
                ..Default::default()
            },
        );

        let row = PodRow::from_pod(&pod, None, now());
        assert_eq!(row.status, "Running");
        // Two of two: the app container and the sidecar.
        assert_eq!(row.ready, "2/2");
        // The sidecar's 3 and the app's 1; the finished init container's 5 are
        // forgotten.
        assert_eq!(row.restarts, 4);
        assert_eq!(row.severity, Severity::Ok);
    }

    #[test]
    fn the_first_unhappy_container_in_the_spec_is_the_one_named() {
        // kubectl walks the statuses backwards, so with two failures the one
        // reported is the first one declared — the one a reader thinks of as
        // "the" container.
        let pod = pod(
            PodSpec {
                containers: vec![container("app"), container("exporter")],
                ..Default::default()
            },
            PodStatus {
                phase: Some("Pending".to_owned()),
                container_statuses: Some(vec![
                    status("app", waiting("CreateContainerConfigError"), false, 0),
                    status("exporter", waiting("ImagePullBackOff"), false, 0),
                ]),
                ..Default::default()
            },
        );

        assert_eq!(
            PodRow::from_pod(&pod, None, now()).status,
            "CreateContainerConfigError"
        );
    }

    #[test]
    fn an_initialised_pod_still_counts_its_running_app_containers() {
        // A sidecar that came up and later started crashing leaves an init
        // container reporting forever. `Initialized` being true is what says
        // the app containers are running anyway, and their readiness counts.
        let mut proxy = status("proxy", waiting("CrashLoopBackOff"), false, 4);
        proxy.started = Some(false);

        let pod = pod(
            PodSpec {
                containers: vec![container("app")],
                init_containers: Some(vec![sidecar("proxy")]),
                ..Default::default()
            },
            PodStatus {
                phase: Some("Running".to_owned()),
                conditions: Some(vec![condition("Initialized", "True", None)]),
                init_container_statuses: Some(vec![proxy]),
                container_statuses: Some(vec![status("app", running(), true, 2)]),
                ..Default::default()
            },
        );

        let row = PodRow::from_pod(&pod, None, now());
        // The crashing sidecar is still what is wrong, and still what is named.
        assert_eq!(row.status, "Init:CrashLoopBackOff");
        // But the app container is up, and one of two is honest.
        assert_eq!(row.ready, "1/2");
        // The sidecar's restarts survive initialisation; so do the app's.
        assert_eq!(row.restarts, 6);
        assert_eq!(row.severity, Severity::Critical);
    }

    #[test]
    fn a_gated_pod_says_so_rather_than_looking_like_a_capacity_problem() {
        let pod = pod(
            PodSpec {
                containers: vec![container("app")],
                node_name: None,
                ..Default::default()
            },
            PodStatus {
                phase: Some("Pending".to_owned()),
                conditions: Some(vec![condition(
                    "PodScheduled",
                    "False",
                    Some("SchedulingGated"),
                )]),
                ..Default::default()
            },
        );

        let row = PodRow::from_pod(&pod, None, now());
        assert_eq!(row.status, "SchedulingGated");
        assert_eq!(row.severity, Severity::Warn);
    }

    #[test]
    fn an_evicted_pod_keeps_the_reason_the_api_server_gave() {
        // `status.reason` outranks the phase, which would only say `Failed`.
        let pod = pod(
            PodSpec {
                containers: vec![container("app")],
                ..Default::default()
            },
            PodStatus {
                phase: Some("Failed".to_owned()),
                reason: Some("Evicted".to_owned()),
                ..Default::default()
            },
        );

        let row = PodRow::from_pod(&pod, None, now());
        assert_eq!(row.status, "Evicted");
        assert_eq!(row.severity, Severity::Critical);
    }

    #[test]
    fn an_unscheduled_pod_has_no_node_to_show() {
        let pod = pod(
            PodSpec {
                containers: vec![container("app")],
                node_name: None,
                ..Default::default()
            },
            PodStatus {
                phase: Some("Pending".to_owned()),
                ..Default::default()
            },
        );

        let row = PodRow::from_pod(&pod, None, now());
        assert_eq!(row.node, "-");
        assert_eq!(row.status, "Pending");
        assert_eq!(row.ready, "0/1");
        assert_eq!(row.severity, Severity::Warn);
    }

    #[test]
    fn a_pod_with_nothing_filled_in_still_produces_a_row() {
        // Every field under `status` is optional, and a pod caught between
        // admission and its first kubelet report really can arrive like this.
        let row = PodRow::from_pod(&Pod::default(), None, now());

        assert_eq!(row.namespace, "-");
        assert_eq!(row.name, "-");
        assert_eq!(row.ready, "0/0");
        // An empty STATUS cell reads like a rendering bug rather than a fact.
        assert_eq!(row.status, "Unknown");
        assert_eq!(row.severity, Severity::Unknown);
        assert_eq!(row.restarts, 0);
        assert_eq!(row.age, "-");
        assert_eq!(row.node, "-");
    }

    #[test]
    fn a_running_pod_missing_a_container_is_a_warning_not_a_success() {
        let pod = pod(
            PodSpec {
                containers: vec![container("app"), container("exporter")],
                ..Default::default()
            },
            PodStatus {
                phase: Some("Running".to_owned()),
                container_statuses: Some(vec![
                    status("app", running(), true, 0),
                    status("exporter", running(), false, 0),
                ]),
                ..Default::default()
            },
        );

        let row = PodRow::from_pod(&pod, None, now());
        assert_eq!(row.ready, "1/2");
        assert_eq!(row.status, "Running");
        assert_eq!(row.severity, Severity::Warn);
    }

    #[test]
    fn an_unrecognised_failure_reason_is_treated_as_a_failure() {
        // The set of things that can go wrong grows every release; a reason
        // this tool has never heard of should arrive coloured as a problem.
        assert_eq!(
            severity("SomeNewBackoffKubeInvented", 0, 1),
            Severity::Critical
        );
        assert_eq!(severity("Init:SomethingNew", 0, 1), Severity::Critical);
        // But the numeric init form is progress however many containers there are.
        assert_eq!(severity("Init:12/40", 0, 1), Severity::Warn);
        assert_eq!(severity("Init:1/", 0, 1), Severity::Critical);
    }

    fn rows() -> Vec<PodRow> {
        let mut other = healthy();
        other.metadata.name = Some("checkout-5d4b".to_owned());
        other.metadata.namespace = Some("storefront".to_owned());
        other.metadata.creation_timestamp = Some(ago(3));

        vec![
            PodRow::from_pod(&healthy(), None, now()),
            PodRow::from_pod(&other, None, now()),
        ]
    }

    #[test]
    fn a_namespaced_listing_does_not_repeat_the_namespace_on_every_row() {
        let rendered = render(
            &rows(),
            "prod (us-east-1)",
            &Scope::Namespace("payments".to_owned()),
            &unfiltered(),
            &[],
        );

        assert_eq!(
            rendered,
            "NAME           READY  STATUS   RESTARTS  AGE  NODE\n\
             api-7c9f       1/1    Running  0         90m  ip-10-0-1-9.ec2.internal\n\
             checkout-5d4b  1/1    Running  0         3m   ip-10-0-1-9.ec2.internal"
        );
    }

    #[test]
    fn a_cluster_wide_listing_leads_with_the_namespace() {
        // Without it, two pods called `api-7c9f` in different namespaces are
        // indistinguishable.
        let rendered = render(&rows(), "prod (us-east-1)", &Scope::All, &unfiltered(), &[]);

        assert_eq!(
            rendered,
            "NAMESPACE   NAME           READY  STATUS   RESTARTS  AGE  NODE\n\
             payments    api-7c9f       1/1    Running  0         90m  ip-10-0-1-9.ec2.internal\n\
             storefront  checkout-5d4b  1/1    Running  0         3m   ip-10-0-1-9.ec2.internal"
        );
    }

    #[test]
    fn an_empty_namespace_suggests_where_else_to_look() {
        let message = render(
            &[],
            "prod (us-east-1)",
            &Scope::Namespace("payments".to_owned()),
            &unfiltered(),
            &[],
        );

        assert!(message.contains("prod (us-east-1)"), "{message}");
        assert!(message.contains("\"payments\""), "{message}");
        assert!(message.contains("--all-namespaces"), "{message}");
        assert!(!message.contains("NAME"), "{message}");
    }

    #[test]
    fn an_empty_cluster_wide_listing_suggests_checking_the_cluster() {
        let message = render(&[], "prod (us-east-1)", &Scope::All, &unfiltered(), &[]);

        assert!(message.contains("prod (us-east-1)"), "{message}");
        assert!(message.contains("eks contexts"), "{message}");
        assert!(!message.contains("--all-namespaces"), "{message}");
    }

    #[test]
    fn an_empty_filtered_listing_blames_the_selector_not_the_namespace() {
        // A live namespace that a selector emptied must not read as an empty
        // namespace, or the user goes hunting for pods that are there.
        let filtered = Selectors {
            label: Some("app=api".to_owned()),
            field: None,
        };
        let message = render(
            &[],
            "prod (us-east-1)",
            &Scope::Namespace("payments".to_owned()),
            &filtered,
            &[],
        );

        assert!(message.contains("prod (us-east-1)"), "{message}");
        assert!(message.contains("label selector `app=api`"), "{message}");
        // The scope's own advice would be a red herring here.
        assert!(!message.contains("--all-namespaces"), "{message}");
    }

    #[test]
    fn an_empty_filtered_listing_names_both_selectors_when_both_are_set() {
        let filtered = Selectors {
            label: Some("app=api".to_owned()),
            field: Some("status.phase!=Running".to_owned()),
        };
        let message = render(&[], "prod (us-east-1)", &Scope::All, &filtered, &[]);

        assert!(message.contains("label selector `app=api`"), "{message}");
        assert!(
            message.contains("field selector `status.phase!=Running`"),
            "{message}"
        );
    }

    #[test]
    fn a_pod_that_has_never_restarted_shows_a_bare_count() {
        // Most rows in a healthy listing. `0 (— ago)` on every one of them
        // would be noise, and there is genuinely nothing to date.
        let row = PodRow::from_pod(&healthy(), None, now());

        assert_eq!(row.restarts, 0);
        assert_eq!(row.restart_age, None);
        assert_eq!(restarts_cell(&row), "0");
    }

    #[test]
    fn a_restarted_container_says_how_long_ago_it_happened() {
        // The distinction the column exists to make: nine crashes last Tuesday
        // and nine crashes in the last five minutes are not the same incident.
        let pod = pod(
            PodSpec {
                containers: vec![container("app")],
                ..Default::default()
            },
            PodStatus {
                phase: Some("Running".to_owned()),
                container_statuses: Some(vec![restarted(
                    "app",
                    waiting("CrashLoopBackOff"),
                    9,
                    Some(ago(5)),
                )]),
                ..Default::default()
            },
        );

        let row = PodRow::from_pod(&pod, None, now());
        assert_eq!(row.restarts, 9);
        assert_eq!(row.restart_age.as_deref(), Some("5m"));
        assert_eq!(restarts_cell(&row), "9 (5m ago)");
    }

    #[test]
    fn the_unformatted_restart_instant_is_carried_for_sorting() {
        // `restart_age` rounds — two pods that crashed forty seconds apart both
        // read `5m` — so ordering needs the moment itself. It has to be the
        // *same* moment the cell is rendered from, or the listing would be
        // sorted by one number and read as another.
        let pod = pod(
            PodSpec {
                containers: vec![container("app")],
                ..Default::default()
            },
            PodStatus {
                phase: Some("Running".to_owned()),
                container_statuses: Some(vec![restarted(
                    "app",
                    waiting("CrashLoopBackOff"),
                    9,
                    Some(ago(5)),
                )]),
                ..Default::default()
            },
        );

        let row = PodRow::from_pod(&pod, None, now());
        assert_eq!(row.last_restart, Some(ago(5).0));
    }

    #[test]
    fn a_pod_with_nothing_to_date_carries_no_instant_either() {
        // Both halves of the pair go missing together: a never-restarted pod,
        // and a restart the kubelet recorded no finishing time for.
        let never = PodRow::from_pod(&healthy(), None, now());
        assert_eq!(never.restarts, 0);
        assert_eq!(never.last_restart, None);

        let undated = pod(
            PodSpec {
                containers: vec![container("app")],
                ..Default::default()
            },
            PodStatus {
                phase: Some("Running".to_owned()),
                container_statuses: Some(vec![restarted(
                    "app",
                    waiting("CrashLoopBackOff"),
                    2,
                    None,
                )]),
                ..Default::default()
            },
        );

        let undated = PodRow::from_pod(&undated, None, now());
        assert_eq!(undated.restarts, 2);
        assert_eq!(undated.last_restart, None);
    }

    #[test]
    fn the_newest_restart_across_the_containers_is_the_one_dated() {
        // One container settled hours ago and another is still going; the
        // recent one is the news, whichever order the statuses arrive in.
        let pod = pod(
            PodSpec {
                containers: vec![container("app"), container("exporter")],
                ..Default::default()
            },
            PodStatus {
                phase: Some("Running".to_owned()),
                container_statuses: Some(vec![
                    restarted("app", waiting("CrashLoopBackOff"), 4, Some(ago(240))),
                    restarted("exporter", running(), 2, Some(ago(2))),
                ]),
                ..Default::default()
            },
        );

        let row = PodRow::from_pod(&pod, None, now());
        assert_eq!(row.restarts, 6);
        assert_eq!(row.restart_age.as_deref(), Some("2m"));
    }

    #[test]
    fn a_finished_init_containers_restart_time_is_forgotten_with_its_count() {
        // The rule that makes this worth a test: once initialisation is over,
        // only a sidecar's restarts survive — so only a sidecar's timestamp may
        // survive with them. A plain init container leaving its date behind
        // would date a count it is no longer part of.
        let mut proxy = restarted("proxy", running(), 3, Some(ago(20)));
        proxy.started = Some(true);
        proxy.ready = true;

        let pod = pod(
            PodSpec {
                containers: vec![container("app")],
                init_containers: Some(vec![sidecar("proxy"), container("migrate")]),
                ..Default::default()
            },
            PodStatus {
                phase: Some("Running".to_owned()),
                conditions: Some(vec![condition("Ready", "True", None)]),
                init_container_statuses: Some(vec![
                    proxy,
                    // Restarted far more recently, and entirely irrelevant.
                    restarted("migrate", terminated(Some("Completed"), 0), 5, Some(ago(1))),
                ]),
                container_statuses: Some(vec![status("app", running(), true, 0)]),
                ..Default::default()
            },
        );

        let row = PodRow::from_pod(&pod, None, now());
        assert_eq!(row.restarts, 3);
        assert_eq!(row.restart_age.as_deref(), Some("20m"));
    }

    #[test]
    fn while_initialising_the_init_containers_own_restart_time_is_shown() {
        // Before the pod comes up the init restarts are the count, so they are
        // also the recency — a pod stuck retrying its migration should say how
        // long ago the last attempt died.
        let pod = pod(
            PodSpec {
                containers: vec![container("app")],
                init_containers: Some(vec![container("migrate"), container("seed")]),
                ..Default::default()
            },
            PodStatus {
                phase: Some("Pending".to_owned()),
                init_container_statuses: Some(vec![restarted(
                    "migrate",
                    waiting("CrashLoopBackOff"),
                    3,
                    Some(ago(45)),
                )]),
                ..Default::default()
            },
        );

        let row = PodRow::from_pod(&pod, None, now());
        assert_eq!(row.status, "Init:CrashLoopBackOff");
        assert_eq!(row.restarts, 3);
        assert_eq!(row.restart_age.as_deref(), Some("45m"));
    }

    #[test]
    fn a_previous_run_with_no_finish_time_leaves_the_count_undated() {
        // A container killed before anything observed it stopping. The count is
        // still real; inventing a date for it would not be.
        let pod = pod(
            PodSpec {
                containers: vec![container("app")],
                ..Default::default()
            },
            PodStatus {
                phase: Some("Running".to_owned()),
                container_statuses: Some(vec![restarted(
                    "app",
                    waiting("CrashLoopBackOff"),
                    2,
                    None,
                )]),
                ..Default::default()
            },
        );

        let row = PodRow::from_pod(&pod, None, now());
        assert_eq!(row.restarts, 2);
        assert_eq!(row.restart_age, None);
        assert_eq!(restarts_cell(&row), "2");
    }

    #[test]
    fn a_restart_dated_in_the_future_reads_as_just_now_rather_than_negatively() {
        // Clock skew between the node and here, which is not the user's problem
        // and certainly not worth a `-3m`.
        let skewed = Time(now() + SignedDuration::from_mins(3));
        let pod = pod(
            PodSpec {
                containers: vec![container("app")],
                ..Default::default()
            },
            PodStatus {
                phase: Some("Running".to_owned()),
                container_statuses: Some(vec![restarted(
                    "app",
                    waiting("CrashLoopBackOff"),
                    1,
                    Some(skewed),
                )]),
                ..Default::default()
            },
        );

        assert_eq!(
            restarts_cell(&PodRow::from_pod(&pod, None, now())),
            "1 (0s ago)"
        );
    }

    #[test]
    fn the_restarts_column_widens_to_fit_the_recency_without_moving_the_others() {
        // The cell grows from `0` to `9 (5m ago)`; the table has to stay a
        // table, and AGE and NODE have to stay where `kubectl` leaves them.
        let mut crashing = healthy();
        crashing.metadata.name = Some("checkout-5d4b".to_owned());
        crashing.status = Some(PodStatus {
            phase: Some("Running".to_owned()),
            container_statuses: Some(vec![restarted(
                "app",
                waiting("CrashLoopBackOff"),
                9,
                Some(ago(5)),
            )]),
            ..Default::default()
        });

        let rows = vec![
            PodRow::from_pod(&healthy(), None, now()),
            PodRow::from_pod(&crashing, None, now()),
        ];
        let rendered = render(
            &rows,
            "prod (us-east-1)",
            &Scope::Namespace("payments".to_owned()),
            &unfiltered(),
            &[],
        );

        assert_eq!(
            rendered,
            "NAME           READY  STATUS            RESTARTS    AGE  NODE\n\
             api-7c9f       1/1    Running           0           90m  ip-10-0-1-9.ec2.internal\n\
             checkout-5d4b  0/1    CrashLoopBackOff  9 (5m ago)  90m  ip-10-0-1-9.ec2.internal"
        );
    }

    /// A `Usage` as metrics-server would have summed it for one pod.
    fn used(cpu: &str, memory: &str) -> Usage {
        Usage {
            cpu: Quantity::parse(cpu).ok(),
            memory: Quantity::parse(memory).ok(),
        }
    }

    /// The same two rows as [`rows`], with live usage on both.
    fn sampled_rows() -> Vec<PodRow> {
        let mut other = healthy();
        other.metadata.name = Some("checkout-5d4b".to_owned());
        other.metadata.namespace = Some("storefront".to_owned());
        other.metadata.creation_timestamp = Some(ago(3));

        vec![
            PodRow::from_pod(&healthy(), Some(used("250m", "512Mi")), now()),
            PodRow::from_pod(&other, Some(used("1200m", "3Gi")), now()),
        ]
    }

    #[test]
    fn a_sampled_pod_carries_its_usage_onto_the_row() {
        let row = PodRow::from_pod(&healthy(), Some(used("250m", "512Mi")), now());

        assert_eq!(row.cpu_used, Some(Quantity::parse("250m").unwrap()));
        assert_eq!(row.memory_used, Some(Quantity::parse("512Mi").unwrap()));
    }

    #[test]
    fn the_creation_instant_agrees_with_the_age_cell() {
        // The two are read from one field so they cannot describe different
        // moments — the pairing `k8s::pods::order` sorts on.
        let row = PodRow::from_pod(&healthy(), None, now());

        assert_eq!(row.age, "90m");
        assert_eq!(row.created_at, Some(now() - SignedDuration::from_mins(90)));
    }

    #[test]
    fn a_pod_with_no_creation_timestamp_has_no_instant_either() {
        // The API server always sets `creationTimestamp`, but a hand-written
        // fixture or a partial object from a cache need not, and the AGE cell
        // already falls back to `-` for it.
        let mut undated = healthy();
        undated.metadata.creation_timestamp = None;

        let row = PodRow::from_pod(&undated, None, now());

        assert_eq!(row.age, "-");
        assert_eq!(row.created_at, None);
    }

    #[test]
    fn a_pod_with_no_sample_has_no_usage_rather_than_zero() {
        // The whole reason these are `Option`: a pod nothing was sampled for
        // must not read as a pod doing nothing.
        let row = PodRow::from_pod(&healthy(), None, now());

        assert_eq!(row.cpu_used, None);
        assert_eq!(row.memory_used, None);

        // Half a sample is the same story for the half that is missing.
        let half = PodRow::from_pod(
            &healthy(),
            Some(Usage {
                cpu: Quantity::parse("250m").ok(),
                memory: None,
            }),
            now(),
        );
        assert_eq!(half.cpu_used, Some(Quantity::parse("250m").unwrap()));
        assert_eq!(half.memory_used, None);
    }

    #[test]
    fn usage_columns_sit_between_the_restarts_and_the_age() {
        let rendered = render(
            &sampled_rows(),
            "prod (us-east-1)",
            &Scope::Namespace("payments".to_owned()),
            &unfiltered(),
            &[],
        );

        assert_eq!(
            rendered,
            "NAME           READY  STATUS   RESTARTS  CPU    MEMORY  AGE  NODE\n\
             api-7c9f       1/1    Running  0         250m   512Mi   90m  ip-10-0-1-9.ec2.internal\n\
             checkout-5d4b  1/1    Running  0         1200m  3Gi     3m   ip-10-0-1-9.ec2.internal"
        );
    }

    #[test]
    fn a_cluster_wide_listing_keeps_the_namespace_first_with_usage_too() {
        let rendered = render(
            &sampled_rows(),
            "prod (us-east-1)",
            &Scope::All,
            &unfiltered(),
            &[],
        );

        assert!(
            rendered.starts_with("NAMESPACE   NAME           READY  STATUS   RESTARTS  CPU"),
            "{rendered}"
        );
    }

    #[test]
    fn a_cluster_with_no_metrics_server_gains_no_empty_columns() {
        // Two blank columns on every EKS cluster that has not installed the
        // add-on would be a worse table than the one we have today.
        let rendered = render(
            &rows(),
            "prod (us-east-1)",
            &Scope::Namespace("payments".to_owned()),
            &unfiltered(),
            &[],
        );

        assert!(!rendered.contains("CPU"), "{rendered}");
        assert!(!rendered.contains("MEMORY"), "{rendered}");
    }

    #[test]
    fn one_unsampled_pod_does_not_cost_the_others_their_columns() {
        // A pod started seconds ago has not been scraped yet; that is a `-` in
        // its own row, not a reason to hide everyone else's figures.
        let mut some = sampled_rows();
        some[1].cpu_used = None;
        some[1].memory_used = None;

        let rendered = render(
            &some,
            "prod (us-east-1)",
            &Scope::Namespace("payments".to_owned()),
            &unfiltered(),
            &[],
        );

        assert!(rendered.contains("CPU"), "{rendered}");
        // The column narrows to the one figure left in it; what matters is that
        // the unsampled row reads `-` in both halves rather than `0`.
        assert!(
            rendered.contains("checkout-5d4b  1/1    Running  0         -     -"),
            "{rendered}"
        );
    }

    #[test]
    fn half_a_sample_shows_the_half_it_has() {
        let mut half = sampled_rows();
        half[0].memory_used = None;

        let rendered = render(
            &half,
            "prod (us-east-1)",
            &Scope::Namespace("payments".to_owned()),
            &unfiltered(),
            &[],
        );

        assert!(
            rendered.contains("api-7c9f       1/1    Running  0         250m   -  "),
            "{rendered}"
        );
    }

    #[test]
    fn a_footnote_is_appended_under_the_table() {
        let rendered = render(
            &rows(),
            "prod (us-east-1)",
            &Scope::Namespace("payments".to_owned()),
            &unfiltered(),
            &[usage_unavailable(
                "prod (us-east-1) has no metrics.k8s.io API.",
            )],
        );

        assert!(rendered.contains("api-7c9f"), "{rendered}");
        assert!(
            rendered.contains("\n\nCPU and MEMORY are not shown"),
            "{rendered}"
        );
        assert!(rendered.contains("no metrics.k8s.io API"), "{rendered}");
    }

    #[test]
    fn the_sort_note_goes_under_the_table_with_the_footnotes() {
        let notes = [
            usage_unavailable("prod (us-east-1) has no metrics.k8s.io API."),
            crate::k8s::order::note(
                crate::k8s::pods::Order::Restarts,
                crate::k8s::order::Direction::Natural,
            )
            .expect("a reordered listing should say so"),
        ];

        let output = render(
            &rows(),
            "prod (us-east-1)",
            &Scope::Namespace("payments".to_owned()),
            &unfiltered(),
            &notes,
        );
        let paragraphs: Vec<&str> = output.split("\n\n").collect();

        // The table above the notes is exactly the table without them.
        assert_eq!(
            paragraphs[0],
            render(
                &rows(),
                "prod (us-east-1)",
                &Scope::Namespace("payments".to_owned()),
                &unfiltered(),
                &[],
            )
        );
        assert_eq!(paragraphs[2], "Sorted by restarts.");
    }

    #[test]
    fn an_ordering_that_ranked_no_pod_says_so_under_the_sort_note() {
        // `eks pods --sort cpu` where metrics-server has sampled nothing: the
        // table has no CPU column, and `Sorted by cpu.` on its own describes a
        // listing the alphabet arranged.
        let order = crate::k8s::pods::Order::Cpu;
        let notes: Vec<String> =
            crate::k8s::order::note(order, crate::k8s::order::Direction::Natural)
                .into_iter()
                .chain(crate::k8s::order::unranked_note(
                    order,
                    crate::k8s::pods::ranks_any(&rows(), order),
                ))
                .collect();

        let output = render(
            &rows(),
            "prod (us-east-1)",
            &Scope::Namespace("payments".to_owned()),
            &unfiltered(),
            &notes,
        );
        let paragraphs: Vec<&str> = output.split("\n\n").collect();

        assert_eq!(paragraphs[1], "Sorted by cpu.");
        assert_eq!(paragraphs[2], "Nothing here has cpu to sort by.");
    }

    #[test]
    fn an_empty_listing_says_nothing_about_an_ordering_that_ranked_nothing() {
        // Nothing ranked, because there is nothing at all. "No pods matched" is
        // the answer; a note about the sort would be noise on top of it.
        let order = crate::k8s::pods::Order::Cpu;
        let note = crate::k8s::order::unranked_note(order, crate::k8s::pods::ranks_any(&[], order))
            .expect("an ordering with no rows to rank ranked nothing");

        let message = render(
            &[],
            "prod (us-east-1)",
            &Scope::Namespace("payments".to_owned()),
            &unfiltered(),
            &[note],
        );

        assert!(!message.contains("sort by"), "{message}");
    }

    #[test]
    fn an_empty_listing_says_nothing_about_the_order_it_would_have_been_in() {
        // `eks pods --sort restarts` in an empty namespace: there is no table
        // for the note to describe, and "there is nothing here" is the answer.
        let note = crate::k8s::order::note(
            crate::k8s::pods::Order::Restarts,
            crate::k8s::order::Direction::Natural,
        )
        .expect("a reordered listing should say so");

        let message = render(
            &[],
            "prod (us-east-1)",
            &Scope::Namespace("payments".to_owned()),
            &unfiltered(),
            &[note],
        );

        assert!(!message.contains("Sorted by"), "{message}");
        assert!(message.contains("no pods in namespace"), "{message}");
    }

    #[test]
    fn a_footnote_is_dropped_when_there_are_no_pods_to_annotate() {
        // "Two columns are missing" is noise on top of "there is nothing here",
        // and the empty-listing message is the one worth reading.
        let message = render(
            &[],
            "prod (us-east-1)",
            &Scope::Namespace("payments".to_owned()),
            &unfiltered(),
            &[usage_unavailable(
                "prod (us-east-1) has no metrics.k8s.io API.",
            )],
        );

        assert!(!message.contains("CPU and MEMORY"), "{message}");
        assert!(message.contains("no pods in namespace"), "{message}");
    }

    #[test]
    fn the_usage_footnote_says_what_is_missing_and_why() {
        let note = usage_unavailable("prod (us-east-1) has no metrics.k8s.io API.");

        assert!(note.contains("CPU and MEMORY"), "{note}");
        assert!(note.contains("not shown"), "{note}");
        assert!(note.contains("no metrics.k8s.io API"), "{note}");
    }
}
