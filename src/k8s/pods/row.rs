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

use std::collections::{BTreeMap, BTreeSet};

use k8s_openapi::api::core::v1::{
    Container, ContainerState, ContainerStateTerminated, ContainerStatus, Pod, PodCondition,
    PodStatus,
};
use k8s_openapi::jiff::Timestamp;

use crate::format;
use crate::k8s::metrics::{self, Sample};
use crate::k8s::pods::is_sidecar;
use crate::k8s::quantity::{self, Quantity};
use crate::k8s::resource;
use crate::theme::{Palette, Severity};

/// Shown wherever the API server left a field empty, as elsewhere in the tool.
pub(crate) const UNKNOWN: &str = "-";

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
    /// Whether the sample behind [`Self::cpu_used`]/[`Self::memory_used`] is,
    /// on its own, old enough to call stale — [`Sample::is_stale`], not a
    /// second reading of "a couple of windows". `false` when there is no
    /// sample at all: an absent figure already reads `-`, and marking it
    /// stale too would say the same thing twice in two different words.
    pub usage_stale: bool,
    /// Cores the pod asked for — the denominator `cpu_used` is shown against.
    ///
    /// [`crate::k8s::pods::effective_requests`]'s figure, which is the same one
    /// `eks nodes` totals per node, rather than a second sum over the
    /// containers: two commands disagreeing about what one pod booked would be
    /// worse than either of them not saying.
    ///
    /// Zero — not `None` — where nothing in the pod set a request, because that
    /// pod really did ask for nothing. A zero denominator has no percentage, so
    /// the cell falls back to the bare usage figure.
    pub cpu_requested: Quantity,
    /// Memory the pod asked for, on the same terms as [`Self::cpu_requested`].
    pub memory_requested: Quantity,
    /// Extended resources the pod asked for — a GPU, a dongle, a licence count
    /// — keyed by their fully-qualified name.
    ///
    /// [`crate::k8s::pods::effective_requests`]'s own map, on the same terms
    /// as [`Self::cpu_requested`]: whatever this pod's containers did not ask
    /// for is simply absent, which is a real zero rather than an unknown.
    pub extended_requested: BTreeMap<String, Quantity>,
    /// The node the pod landed on, or `-` while it is still unscheduled.
    pub node: String,
    /// The address the pod answers on, or `-` before the CNI has assigned one.
    ///
    /// Only shown under `--wide`. On EKS this is a VPC address rather than an
    /// overlay one, so it is the address a load balancer target group holds and
    /// the one a security-group rule has to allow — which is why it is worth a
    /// column at all.
    pub ip: String,
    /// The node the scheduler has earmarked for this pod while it evicts
    /// something to make room, or `-` — which is nearly every pod.
    ///
    /// Only shown under `--wide`. A `Pending` pod with a nominated node is not
    /// stuck: preemption is under way and it has somewhere to go. That is the
    /// opposite conclusion from the one the `STATUS` column invites on its own.
    pub nominated_node: String,
    /// How many of the pod's readiness gates are satisfied, as `1/2`.
    ///
    /// Only shown under `--wide`, and `None` for a pod with no gates, which is
    /// nearly every pod. A gate is the one way `READY` can read `2/2` on a pod
    /// the cluster still calls unready — every container up, and some external
    /// controller withholding its condition — so the default table cannot
    /// explain that row and this column can.
    pub readiness_gates: Option<String>,
}

impl PodRow {
    /// Build a row from a `Pod`, as of `now`.
    ///
    /// `now` is a parameter rather than a call to the clock so the age column
    /// is testable and so every row in one listing shares a single instant.
    ///
    /// `sample` is what metrics-server last reported for this pod: the usage
    /// already summed across its containers, and the two stamps that say how
    /// old it is. `None` covers every reason there is no figure — no
    /// metrics-server, or a pod it has not reached — and all of them render
    /// the same way, because to a reader they mean the same thing.
    #[must_use]
    pub fn from_pod(pod: &Pod, sample: Option<Sample>, now: Timestamp) -> Self {
        let derived = derive(pod);
        // The scheduler's own arithmetic, not a fresh sum over the containers:
        // `eks nodes` totals exactly this number per node, and the two commands
        // must not be able to disagree about what one pod asked for.
        let requested = super::effective_requests(pod);
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
            cpu_used: sample.and_then(|sample| sample.usage.cpu),
            memory_used: sample.and_then(|sample| sample.usage.memory),
            usage_stale: sample.is_some_and(|sample| sample.is_stale(now)),
            cpu_requested: requested.cpu,
            memory_requested: requested.memory,
            extended_requested: requested.extended,
            node: pod
                .spec
                .as_ref()
                .and_then(|spec| spec.node_name.as_deref())
                .filter(|name| !name.is_empty())
                .map_or_else(|| UNKNOWN.to_owned(), str::to_owned),
            ip: pod_ip(pod),
            nominated_node: nominated_node(pod),
            readiness_gates: readiness_gates(pod),
        }
    }
}

/// The pod's address, preferring the dual-stack list `kubectl -o wide` reads.
///
/// `podIPs` and `podIP` are two spellings of the same thing and the kubelet
/// fills both, but only the list can hold the IPv6 address of a dual-stack pod,
/// and its first entry is the one matching the pod's primary IP family. Falling
/// back to `podIP` covers the pod whose kubelet is older than the list field.
///
/// `pub(crate)` so the pod-containers pane can read the same fact directly off
/// the one `Pod` it fetches, rather than a second reading of the status —
/// see [`crate::ui::containers`].
pub(crate) fn pod_ip(pod: &Pod) -> String {
    let status = pod.status.as_ref();
    let from_list = status
        .and_then(|status| status.pod_ips.as_ref())
        .and_then(|ips| ips.first())
        .map(|entry| entry.ip.as_str());
    let legacy = status.and_then(|status| status.pod_ip.as_deref());

    from_list
        .or(legacy)
        .filter(|ip| !ip.is_empty())
        .map_or_else(|| UNKNOWN.to_owned(), str::to_owned)
}

/// The node the scheduler has earmarked for this pod while it evicts
/// something to make room, or `-` — which is nearly every pod.
///
/// `pub(crate)` for the same reason [`pod_ip`] is.
pub(crate) fn nominated_node(pod: &Pod) -> String {
    pod.status
        .as_ref()
        .and_then(|status| status.nominated_node_name.as_deref())
        .filter(|name| !name.is_empty())
        .map_or_else(|| UNKNOWN.to_owned(), str::to_owned)
}

/// How many of the pod's readiness gates its conditions satisfy, as `1/2`.
///
/// `None` when the pod declares no gates, which reads as `-`: `0/0` would
/// suggest something unsatisfied on the majority of rows, where in fact there
/// is nothing to satisfy. A gate whose condition the API server has not
/// recorded at all counts as unsatisfied, which is what the pod's own readiness
/// does with it.
///
/// `pub(crate)` for the same reason [`pod_ip`] is.
pub(crate) fn readiness_gates(pod: &Pod) -> Option<String> {
    let gates = pod.spec.as_ref()?.readiness_gates.as_deref()?;
    if gates.is_empty() {
        return None;
    }

    let satisfied = gates
        .iter()
        .filter(|gate| {
            condition(pod.status.as_ref(), &gate.condition_type)
                .is_some_and(|found| found.status == "True")
        })
        .count();
    Some(format!("{satisfied}/{}", gates.len()))
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
///
/// `pub(super)` rather than private: [`super::containers`] words a
/// container's *current* state the same way, and a `Completed` this function
/// names must be the same word that module names for the same termination.
pub(super) fn exit_reason(terminated: &ContainerStateTerminated) -> String {
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
///
/// Public for the reason [`crate::k8s::nodes::shows_usage`] is: the command
/// layer owes the reader a footnote when the columns are gone, and the rows are
/// what decide whether they are.
#[must_use]
pub fn shows_usage(rows: &[PodRow]) -> bool {
    rows.iter()
        .any(|row| row.cpu_used.is_some() || row.memory_used.is_some())
}

/// Whether `CPU REQ` and `MEMORY REQ` are worth two columns.
///
/// `any`, not `all` — the same rule [`shows_usage`] follows, so a namespace
/// where nobody set a request does not grow two columns of `0`. Paired rather
/// than asked per resource, for the reason the usage pair is: a request is one
/// question — "what did this book?" — and a pod that set a memory request and
/// left CPU unbounded still belongs beside a `CPU REQ` reading `0`, its own
/// honest answer, rather than losing the column a neighbouring row earned.
#[must_use]
fn shows_requests(rows: &[PodRow]) -> bool {
    rows.iter().any(|row| {
        row.cpu_requested != Quantity::default() || row.memory_requested != Quantity::default()
    })
}

/// Every extended resource some pod in this listing asked for, in name order.
///
/// The pod table's twin of [`crate::k8s::nodes::device_names`], and the same
/// `any`-not-`all` reasoning: one pod on a GPU node group must not cost every
/// other pod's listing the column. Unlike a node, a pod has no hardware to
/// report — every name here came from some container's own resource request —
/// so there is no "does not have one" case for the cell to tell apart from a
/// real zero the way [`crate::k8s::nodes::Device`]'s does; a pod that did not
/// ask reads `0`, not `-`.
#[must_use]
fn device_names(rows: &[PodRow]) -> BTreeSet<&str> {
    rows.iter()
        .flat_map(|row| row.extended_requested.keys().map(String::as_str))
        .collect()
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

/// One usage cell: what the pod is burning, and what share of its own request
/// that is.
///
/// `262m (52%)`. A bare `262m` cannot be read: it is over a quarter of a core,
/// and whether that is fine, throttled, or about to be OOM-killed depends
/// entirely on the number the pod asked for. A pod has no allocatable of its
/// own to be a share of — the node table's denominator — so its request is the
/// one honest denominator there is.
///
/// The request itself has its own column, `CPU REQ`, so this cell shows only
/// the percentage rather than repeating the number beside it — `262m/500m
/// (52%)` would print `500m` a second time now that it is not the only place
/// to find it.
///
/// A pod that asked for nothing, or has not been sampled, keeps the bare
/// figure: `ratio_of` declines a zero denominator, which is exactly the pod
/// that asked for nothing, so the two cases are one branch rather than a
/// second check for zero that could come to disagree with it.
fn usage_cell(used: Option<Quantity>, requested: Quantity, show: fn(Quantity) -> String) -> String {
    let Some(used) = used else {
        return UNKNOWN.to_owned();
    };

    match used.ratio_of(requested) {
        Some(ratio) => format!("{} ({})", show(used), format::percentage(ratio)),
        None => show(used),
    }
}

/// One column of the pod table.
///
/// The column set is a value rather than two parallel lists of headers and
/// cells, because the two lists drifting apart is a bug that type-checks: a
/// header added under one condition and a cell under a subtly different one
/// puts every figure to the right of it under the wrong heading, and the table
/// still renders. With this, each column answers for both halves of itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Column<'a> {
    Namespace,
    Name,
    Ready,
    Status,
    Restarts,
    /// What the pod asked for — [`PodRow::cpu_requested`] — on its own,
    /// regardless of whether anything has measured it yet.
    CpuRequested,
    /// Live CPU usage, against [`Self::CpuRequested`].
    Cpu,
    /// What the pod asked for in memory, on the same terms as
    /// [`Self::CpuRequested`].
    MemoryRequested,
    /// Live memory usage, on the same terms as [`Self::Cpu`].
    Memory,
    /// One extended resource some pod in this listing asked for, by its
    /// fully-qualified name.
    ///
    /// The only column whose identity is not known until the pods arrive, so
    /// it borrows the name from the rows it was computed from rather than
    /// owning a copy per column — the node table's own device column does the
    /// same.
    Device(&'a str),
    Age,
    Ip,
    Node,
    NominatedNode,
    ReadinessGates,
}

impl Column<'_> {
    /// The heading, spelled as `kubectl get pods` spells it.
    ///
    /// A `String` rather than a `&'static str`, for the reason the node
    /// table's twin is: a device column is headed by a name the cluster
    /// invented.
    fn header(self) -> String {
        match self {
            Self::Namespace => "NAMESPACE".to_owned(),
            Self::Name => "NAME".to_owned(),
            Self::Ready => "READY".to_owned(),
            Self::Status => "STATUS".to_owned(),
            Self::Restarts => "RESTARTS".to_owned(),
            Self::CpuRequested => "CPU REQ".to_owned(),
            Self::Cpu => "CPU".to_owned(),
            Self::MemoryRequested => "MEMORY REQ".to_owned(),
            Self::Memory => "MEMORY".to_owned(),
            Self::Device(name) => resource::heading(name),
            Self::Age => "AGE".to_owned(),
            Self::Ip => "IP".to_owned(),
            Self::Node => "NODE".to_owned(),
            Self::NominatedNode => "NOMINATED NODE".to_owned(),
            Self::ReadinessGates => "READINESS GATES".to_owned(),
        }
    }

    /// This column's cell for one row: the text, and how alarming it is.
    fn cell(self, row: &PodRow) -> format::Cell {
        match self.severity(row) {
            Some(severity) => format::Cell::graded(self.text(row), severity),
            None => format::Cell::plain(self.text(row)),
        }
    }

    /// This column's text for one row.
    fn text(self, row: &PodRow) -> String {
        match self {
            Self::Namespace => row.namespace.clone(),
            Self::Name => row.name.clone(),
            Self::Ready => row.ready.clone(),
            Self::Status => row.status.clone(),
            Self::Restarts => restarts_cell(row),
            Self::CpuRequested => quantity::cpu(row.cpu_requested),
            Self::Cpu => metrics::mark_stale(
                usage_cell(row.cpu_used, row.cpu_requested, quantity::cpu),
                row.usage_stale,
            ),
            Self::MemoryRequested => quantity::memory(row.memory_requested),
            Self::Memory => metrics::mark_stale(
                usage_cell(row.memory_used, row.memory_requested, quantity::memory),
                row.usage_stale,
            ),
            // A pod that never named this resource reads `0`, not `-`: unlike
            // a node, which either has a device or does not, every pod could
            // in principle have asked for any resource, and not asking is
            // itself a real, honest zero.
            Self::Device(name) => quantity::count(
                row.extended_requested
                    .get(name)
                    .copied()
                    .unwrap_or_default(),
            ),
            Self::Age => row.age.clone(),
            Self::Ip => row.ip.clone(),
            Self::Node => row.node.clone(),
            Self::NominatedNode => row.nominated_node.clone(),
            Self::ReadinessGates => row
                .readiness_gates
                .clone()
                .unwrap_or_else(|| UNKNOWN.to_owned()),
        }
    }

    /// How alarming this column's value is on this row, or `None` for a column
    /// that carries no judgement.
    ///
    /// `STATUS` is the only graded column in this table, and it takes the
    /// severity [`PodRow`] already carries — so `CrashLoopBackOff` reads as
    /// alarming in the CLI for exactly the reason it will in a dashboard pane,
    /// rather than because a second rule here agreed with the first.
    ///
    /// `READY` is deliberately not a second one. `0/1` is what makes a
    /// `Running` pod's severity `Warn` in the first place — see
    /// [`severity`] — so colouring it too would paint one
    /// judgement across two columns and say nothing new in the second.
    ///
    /// `CPU` and `MEMORY` are not graded either, and that is a gap rather than
    /// a rule: those cells carry a percentage, and there is a perfectly good
    /// [`Severity`] waiting to be applied to it — but not
    /// [`Severity::from_utilisation`]'s. Its thresholds are about a *node's*
    /// allocatable, where 90% booked is nearly full; a pod at 90% of the CPU
    /// it asked for is a well-sized pod, and one at 400% of a 10m request is
    /// burning 40m and is nobody's emergency. Colouring them on the node's
    /// thresholds would tell the reader something untrue, in red, on most of
    /// their rows. What "hot" means for a pod against its own request is a
    /// decision, and it is the reviewer's to make before this column takes a
    /// colour. `CPU REQ`, `MEMORY REQ`, and the device columns carry no
    /// percentage at all — they are a plain fact about the pod, like `AGE` —
    /// so there is nothing pending for them to grade.
    ///
    /// [`Severity::from_utilisation`]: crate::theme::Severity::from_utilisation
    fn severity(self, row: &PodRow) -> Option<Severity> {
        match self {
            Self::Status => Some(row.severity),
            Self::Namespace
            | Self::Name
            | Self::Ready
            | Self::Restarts
            | Self::CpuRequested
            | Self::Cpu
            | Self::MemoryRequested
            | Self::Memory
            | Self::Device(_)
            | Self::Age
            | Self::Ip
            | Self::Node
            | Self::NominatedNode
            | Self::ReadinessGates => None,
        }
    }
}

/// Which columns this listing gets, in order.
///
/// A pure function over the things that decide it — the scope, whether any
/// row has live usage, whether any row has a request worth two more columns,
/// which extended resources some row asked for, and `--wide` — so the whole
/// layout is settled by a test rather than by reading a table in a terminal.
///
/// Usage, requests, and devices are all `any`: the columns appear unasked for,
/// so a cluster with no metrics-server, or a namespace where nobody set a
/// request, must not gain empty ones. `--wide` was asked for, so its columns
/// appear whatever is in them — a table of `-` under `NOMINATED NODE` is the
/// answer "nothing is being preempted", and dropping the column would leave
/// the user unable to tell that from a flag that did nothing.
///
/// [`Width::Narrow`] then drops columns from that set until the row fits its
/// target — see [`DROP_ORDER`]. `Wide` is never narrowed: the user typed
/// `--wide` for the extra columns, not for a table that keeps away from them.
///
/// [`Width::Narrow`]: format::Width::Narrow
pub(crate) fn columns<'a>(
    scope: &super::Scope,
    rows: &'a [PodRow],
    width: format::Width,
) -> Vec<Column<'a>> {
    let mut columns = Vec::with_capacity(14);
    if scope.needs_namespace_column() {
        columns.push(Column::Namespace);
    }
    columns.extend([
        Column::Name,
        Column::Ready,
        Column::Status,
        Column::Restarts,
    ]);
    // The request, usage, and device columns sit with STATUS and RESTARTS, the
    // other columns about how the pod is doing, rather than at the end: a pod
    // that is unhappy and one that is burning a core are usually the same
    // investigation. CPU and its request sit together, then MEMORY and its
    // request, so the comparison a reader wants — booked against burnt — is a
    // glance rather than a scan across the row, the same grouping the node
    // table uses.
    if shows_requests(rows) {
        columns.push(Column::CpuRequested);
    }
    if shows_usage(rows) {
        columns.push(Column::Cpu);
    }
    if shows_requests(rows) {
        columns.push(Column::MemoryRequested);
    }
    if shows_usage(rows) {
        columns.push(Column::Memory);
    }
    columns.extend(device_names(rows).into_iter().map(Column::Device));
    // AGE, IP, NODE, NOMINATED NODE, READINESS GATES is `kubectl -o wide`'s own
    // tail order, kept to the letter. NODE is in this table by default where
    // `kubectl` holds it back for wide, so `--wide` adds the three columns
    // around it rather than the four `kubectl` adds.
    columns.push(Column::Age);
    if width.is_wide() {
        columns.push(Column::Ip);
    }
    columns.push(Column::Node);
    if width.is_wide() {
        columns.extend([Column::NominatedNode, Column::ReadinessGates]);
    }
    match width {
        format::Width::Default | format::Width::Wide => columns,
        format::Width::Narrow(target) => narrow_to_fit(&columns, rows, target),
    }
}

/// The order columns get dropped in when [`Width::Narrow`] cannot fit them all.
///
/// A list of predicates rather than a ranking, like the node table's, because
/// the request and usage columns want to leave in their own pairs: `CPU` beside
/// `AGE` with no `MEMORY` between them is half an answer to "what is this
/// burning", and the eye reading a row of pairs pairs the wrong ones.
///
/// The steps, and why they are in this order:
///
/// 1. `AGE` — the cheapest column on the row and the least of it. It is also
///    the one fact the table says twice: `RESTARTS` carries `9 (5m ago)`, so
///    "when did this last change" survives `AGE` leaving.
/// 2. `NODE` — the widest cell in the table on EKS, where a node is a
///    forty-character DNS name, and a follow-up question rather than a first
///    one: which machine a pod is on matters once you know which pod you are
///    looking at, and every column that stays is there to find that pod.
///    Dropping it lands on `kubectl get pods`'s own column set, which is where
///    a reader's habits are.
/// 3. `CPU REQ` and `MEMORY REQ`, together — dropping the pair leaves usage
///    on its own, which is the "what is this doing right now" question.
/// 4. `CPU` and `MEMORY`, together — the pair the tool exists for, so late,
///    and the same "both or neither" rule the node table's pairs follow.
/// 5. Every device column, together, for the node table's reason: a listing
///    on a GPU cluster wants them all or none, and the alphabet is a bad rule
///    for "which resource is important". A no-op on the overwhelming majority
///    of listings, which ask for no extended resource at all.
/// 6. `RESTARTS` — the first of the three health columns to go, because it is
///    the widest of them and because a pod restarting is usually a pod
///    `STATUS` has something to say about.
/// 7. `READY` — five characters, and the refinement of `STATUS` rather than a
///    fact of its own: `0/1` is the detail under `CrashLoopBackOff`.
/// 8. `STATUS` — the last thing to go, as on the node table. A listing down to
///    a name and one word keeps the word that names a problem.
///
/// `NAME` never drops, for the node table's reason: a row we cannot fit is
/// still a row about something, and a listing with no names is about nothing.
/// `NAMESPACE` never drops either, which is the pod table's own rule: it is in
/// this table only under `-A`, where a name is not an identity — `coredns-xyz`
/// in `kube-system` and in a copy of it somewhere else are two pods, and the
/// column the user widened the scope to get is the only thing telling them
/// apart. It is the second half of `NAME` here, not a column beside it.
///
/// The `--wide` columns are not in the list because they cannot be in the
/// table: [`format::Width::for_terminal`] answers `Wide` when `--wide` was
/// typed, so a `Narrow` listing never carried `IP`, `NOMINATED NODE`, or
/// `READINESS GATES` in the first place. A step for them would be a step that
/// never fires.
///
/// [`Width::Narrow`]: format::Width::Narrow
const DROP_ORDER: &[fn(&Column<'_>) -> bool] = &[
    |c| matches!(c, Column::Age),
    |c| matches!(c, Column::Node),
    |c| matches!(c, Column::CpuRequested | Column::MemoryRequested),
    |c| matches!(c, Column::Cpu | Column::Memory),
    |c| matches!(c, Column::Device(_)),
    |c| matches!(c, Column::Restarts),
    |c| matches!(c, Column::Ready),
    |c| matches!(c, Column::Status),
];

/// Drop columns from `columns` in [`DROP_ORDER`] until the row fits `target`.
///
/// Stops as soon as the row fits, so on a wide-enough terminal a
/// `Narrow(target)` returns exactly what `Default` does, byte for byte —
/// narrowing is subtraction, and a table that already fits has nothing to
/// subtract. When even the columns that never drop are too wide — a
/// one-column terminal, or a namespace and a pod name that do not fit
/// together — the last step leaves `NAME` (and `NAMESPACE` under `-A`) and the
/// row prints wider than the target. That is the terminal's problem to wrap,
/// rather than ours to solve by printing rows nobody can identify.
fn narrow_to_fit<'a>(columns: &[Column<'a>], rows: &[PodRow], target: u16) -> Vec<Column<'a>> {
    // Measured once: a column is as wide as its own widest cell whatever its
    // neighbours do, so dropping one changes which widths are in the sum and
    // not what any of them are. Rendering every cell in the listing again at
    // each step would be the same answer for a listing's worth of work.
    let mut measured: Vec<(Column<'a>, usize)> =
        columns.iter().copied().zip(widths(columns, rows)).collect();

    let target = usize::from(target);
    for step in DROP_ORDER {
        if row_width(&measured) <= target {
            break;
        }
        measured.retain(|(column, _)| !step(column));
    }

    measured.into_iter().map(|(column, _)| column).collect()
}

/// How wide each of these columns will be when rendered.
///
/// The node table's twin, and asks the same question of the same function:
/// [`format::column_widths`], over the headers and cells [`render`] is about
/// to hand [`format::table`]. Measuring any other way would let the drop rule
/// stop at a width the renderer does not print at.
fn widths(columns: &[Column<'_>], rows: &[PodRow]) -> Vec<usize> {
    let headings: Vec<String> = columns.iter().map(|column| column.header()).collect();
    let headers: Vec<&str> = headings.iter().map(String::as_str).collect();
    let cells: Vec<Vec<format::Cell>> = rows
        .iter()
        .map(|row| columns.iter().map(|column| column.cell(row)).collect())
        .collect();

    format::column_widths(&headers, &cells)
}

/// How wide the row of the columns still standing will be.
fn row_width(measured: &[(Column<'_>, usize)]) -> usize {
    let widths: Vec<usize> = measured.iter().map(|(_, width)| *width).collect();
    format::row_width(&widths)
}

/// Render the `eks pods` table.
///
/// `cluster` is the human label used in the empty-list message, so a user who
/// typed the wrong `--context` or the wrong namespace finds out from the answer
/// rather than from a bare header.
///
/// `width` is `--wide`. It changes only which columns are printed, never what
/// was fetched: everything the extra columns show came back with the pods.
///
/// `notes` are appended under the table — see [`usage_unavailable`]. They are
/// dropped when there are no pods, where a footnote about missing columns would
/// only be noise on top of a bigger problem.
///
/// `palette` decides whether `STATUS` is written in ink, and `STATUS` is the
/// only column here that is: `READY` is the detail under it rather than a
/// second judgement, and the usage pair is waiting on a decision about what
/// "hot" means for a pod against its own request. It changes no column, no
/// width, and no footnote.
#[must_use]
pub fn render(
    rows: &[PodRow],
    cluster: &str,
    scope: &super::Scope,
    selectors: &super::Selectors,
    notes: &[String],
    width: format::Width,
    palette: Palette,
) -> String {
    if rows.is_empty() {
        return empty(cluster, scope, selectors);
    }

    let columns = columns(scope, rows, width);
    let headings: Vec<String> = columns.iter().map(|column| column.header()).collect();
    let headers: Vec<&str> = headings.iter().map(String::as_str).collect();
    let cells: Vec<Vec<format::Cell>> = rows
        .iter()
        .map(|row| columns.iter().map(|column| column.cell(row)).collect())
        .collect();

    let table = format::table(&headers, &cells, palette);

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

/// The footnote shown when the metrics read succeeded and had nothing in it.
///
/// The pod half of [`crate::k8s::nodes::usage_unsampled`], and there for the
/// same reason: the columns vanish exactly as they do when the read fails, and
/// a successful request that quietly costs two columns leaves the user unable
/// to tell a cluster with no metrics-server from one whose metrics-server has
/// not reached these pods yet.
///
/// The headings are named without the `/REQ` suffix a listing with requests
/// behind it gets, because a listing with no usage never shows that heading —
/// the denominator is only printed where there is a figure to divide.
#[must_use]
pub fn usage_unsampled(explanation: &str) -> String {
    format!(
        "CPU and MEMORY are not shown because nothing here has been sampled yet.\n{explanation}"
    )
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
///
/// Shared with the dashboard's pod-drilldown pane, which asks the same
/// question of its own selectors when a node's pods come back empty — see
/// [`crate::commands::pods::PodsFetch::selector_note`].
#[must_use]
pub fn selector_note(selectors: &super::Selectors) -> Option<String> {
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

    use crate::format::Width;

    use k8s_openapi::api::core::v1::{
        ContainerStateRunning, ContainerStateWaiting, PodIP, PodReadinessGate, PodSpec,
        ResourceRequirements,
    };
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity as ApiQuantity;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};
    use k8s_openapi::jiff::SignedDuration;

    use super::super::{Scope, Selectors};
    use super::*;
    use crate::k8s::metrics::Usage;

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

    fn gate(kind: &str) -> PodReadinessGate {
        PodReadinessGate {
            condition_type: kind.to_owned(),
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
            Width::Default,
            Palette::Plain,
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
        let rendered = render(
            &rows(),
            "prod (us-east-1)",
            &Scope::All,
            &unfiltered(),
            &[],
            Width::Default,
            Palette::Plain,
        );

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
            Width::Default,
            Palette::Plain,
        );

        assert!(message.contains("prod (us-east-1)"), "{message}");
        assert!(message.contains("\"payments\""), "{message}");
        assert!(message.contains("--all-namespaces"), "{message}");
        assert!(!message.contains("NAME"), "{message}");
    }

    #[test]
    fn an_empty_cluster_wide_listing_suggests_checking_the_cluster() {
        let message = render(
            &[],
            "prod (us-east-1)",
            &Scope::All,
            &unfiltered(),
            &[],
            Width::Default,
            Palette::Plain,
        );

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
            Width::Default,
            Palette::Plain,
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
        let message = render(
            &[],
            "prod (us-east-1)",
            &Scope::All,
            &filtered,
            &[],
            Width::Default,
            Palette::Plain,
        );

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
            Width::Default,
            Palette::Plain,
        );

        assert_eq!(
            rendered,
            "NAME           READY  STATUS            RESTARTS    AGE  NODE\n\
             api-7c9f       1/1    Running           0           90m  ip-10-0-1-9.ec2.internal\n\
             checkout-5d4b  0/1    CrashLoopBackOff  9 (5m ago)  90m  ip-10-0-1-9.ec2.internal"
        );
    }

    /// A `Sample` as metrics-server would have summed it for one pod, with no
    /// timestamp or window — the ordinary case for these tests, which are not
    /// about staleness. See `stale_sample` below for the one that is.
    fn used(cpu: &str, memory: &str) -> Sample {
        Sample {
            usage: Usage {
                cpu: Quantity::parse(cpu).ok(),
                memory: Quantity::parse(memory).ok(),
            },
            taken_at: None,
            window: None,
        }
    }

    /// A sample old enough that [`Sample::is_stale`] calls it stale: two
    /// sampling windows behind `now()`, with a one-second margin so the
    /// comparison is not exactly on the boundary.
    fn stale_sample(cpu: &str, memory: &str, now: Timestamp) -> Sample {
        let window = SignedDuration::from_secs(20);
        Sample {
            usage: Usage {
                cpu: Quantity::parse(cpu).ok(),
                memory: Quantity::parse(memory).ok(),
            },
            taken_at: Some(now - window.checked_mul(2).unwrap() - SignedDuration::from_secs(1)),
            window: Some(window),
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
    fn a_fresh_sample_is_not_stale() {
        let row = PodRow::from_pod(&healthy(), Some(used("250m", "512Mi")), now());

        assert!(!row.usage_stale);
    }

    #[test]
    fn a_sample_two_windows_old_is_stale() {
        let now = now();
        let row = PodRow::from_pod(&healthy(), Some(stale_sample("250m", "512Mi", now)), now);

        assert!(row.usage_stale);
    }

    #[test]
    fn a_pod_with_no_sample_is_not_marked_stale() {
        // No sample is a different fact from a stale one, and reads as `-`
        // rather than as a figure carrying a warning nobody measured.
        let row = PodRow::from_pod(&healthy(), None, now());

        assert!(!row.usage_stale);
    }

    #[test]
    fn a_stale_sample_marks_the_cpu_and_memory_cells() {
        let now = now();
        let row = PodRow::from_pod(&healthy(), Some(stale_sample("250m", "512Mi", now)), now);

        assert_eq!(Column::Cpu.text(&row), "250m (stale)");
        assert_eq!(Column::Memory.text(&row), "512Mi (stale)");
    }

    #[test]
    fn a_fresh_sample_leaves_the_usage_cells_unmarked() {
        let row = PodRow::from_pod(&healthy(), Some(used("250m", "512Mi")), now());

        assert_eq!(Column::Cpu.text(&row), "250m");
        assert_eq!(Column::Memory.text(&row), "512Mi");
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
            Some(Sample {
                usage: Usage {
                    cpu: Quantity::parse("250m").ok(),
                    memory: None,
                },
                taken_at: None,
                window: None,
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
            Width::Default,
            Palette::Plain,
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
            Width::Default,
            Palette::Plain,
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
            Width::Default,
            Palette::Plain,
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
            Width::Default,
            Palette::Plain,
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
            Width::Default,
            Palette::Plain,
        );

        assert!(
            rendered.contains("api-7c9f       1/1    Running  0         250m   -  "),
            "{rendered}"
        );
    }

    /// The healthy pod, with its container asking for the given resources.
    ///
    /// Takes the entries as pairs rather than two `&str`s so a container that
    /// sets a memory request and leaves CPU unbounded — a very common shape,
    /// and the one that gives a listing a pair in one column and not the other
    /// — is expressible.
    fn asking(entries: &[(&str, &str)]) -> Pod {
        let mut pod = healthy();
        if let Some(spec) = pod.spec.as_mut() {
            for container in &mut spec.containers {
                container.resources = Some(ResourceRequirements {
                    requests: Some(
                        entries
                            .iter()
                            .map(|(name, value)| {
                                ((*name).to_owned(), ApiQuantity((*value).to_owned()))
                            })
                            .collect(),
                    ),
                    ..Default::default()
                });
            }
        }
        pod
    }

    /// The two rows of [`sampled_rows`], each pod asking for something — the
    /// shape nearly every real deployment has.
    fn requesting_rows() -> Vec<PodRow> {
        let mut other = asking(&[("cpu", "2"), ("memory", "4Gi")]);
        other.metadata.name = Some("checkout-5d4b".to_owned());
        other.metadata.namespace = Some("storefront".to_owned());
        other.metadata.creation_timestamp = Some(ago(3));

        vec![
            PodRow::from_pod(
                &asking(&[("cpu", "500m"), ("memory", "1Gi")]),
                Some(used("250m", "512Mi")),
                now(),
            ),
            PodRow::from_pod(&other, Some(used("1200m", "3Gi")), now()),
        ]
    }

    #[test]
    fn a_pod_carries_what_it_asked_for_onto_the_row() {
        let row = PodRow::from_pod(
            &asking(&[("cpu", "500m"), ("memory", "1Gi")]),
            Some(used("250m", "512Mi")),
            now(),
        );

        assert_eq!(row.cpu_requested, Quantity::parse("500m").unwrap());
        assert_eq!(row.memory_requested, Quantity::parse("1Gi").unwrap());
    }

    #[test]
    fn the_request_on_a_row_is_the_one_eks_nodes_totals() {
        // The interesting half of `effective_requests`: an init container that
        // dwarfs the app container decides the pod's footprint, and pod
        // overhead is charged on top. A second sum written here would get this
        // wrong quietly, and the two commands would disagree about one pod.
        let mut pod = healthy();
        if let Some(spec) = pod.spec.as_mut() {
            spec.containers = vec![Container {
                resources: Some(ResourceRequirements {
                    requests: Some(
                        [("cpu", "200m")]
                            .into_iter()
                            .map(|(name, value)| (name.to_owned(), ApiQuantity(value.to_owned())))
                            .collect(),
                    ),
                    ..Default::default()
                }),
                ..container("app")
            }];
            spec.init_containers = Some(vec![Container {
                resources: Some(ResourceRequirements {
                    requests: Some(
                        [("cpu", "1")]
                            .into_iter()
                            .map(|(name, value)| (name.to_owned(), ApiQuantity(value.to_owned())))
                            .collect(),
                    ),
                    ..Default::default()
                }),
                ..container("migrate")
            }]);
            spec.overhead = Some(
                [("cpu", "50m")]
                    .into_iter()
                    .map(|(name, value)| (name.to_owned(), ApiQuantity(value.to_owned())))
                    .collect(),
            );
        }

        let row = PodRow::from_pod(&pod, Some(used("250m", "512Mi")), now());

        // max(200m app, 1 init) + 50m overhead, which is what `eks nodes` puts
        // in this pod's share of its node.
        assert_eq!(row.cpu_requested, Quantity::parse("1050m").unwrap());
        assert_eq!(
            row.cpu_requested,
            crate::k8s::pods::effective_requests(&pod).cpu
        );
    }

    #[test]
    fn usage_is_shown_against_what_the_pod_asked_for() {
        // 250m on its own is unreadable: a fifth of a core is fine, throttled,
        // or a mistake depending entirely on the number beside it. The request
        // itself has its own column now, so the usage cell carries only the
        // percentage rather than the request a second time.
        let rendered = render(
            &requesting_rows(),
            "prod (us-east-1)",
            &Scope::Namespace("payments".to_owned()),
            &unfiltered(),
            &[],
            Width::Default,
            Palette::Plain,
        );

        assert_eq!(
            rendered,
            "NAME           READY  STATUS   RESTARTS  CPU REQ  CPU          MEMORY REQ  MEMORY       AGE  NODE\n\
             api-7c9f       1/1    Running  0         500m     250m (50%)   1Gi         512Mi (50%)  90m  ip-10-0-1-9.ec2.internal\n\
             checkout-5d4b  1/1    Running  0         2        1200m (60%)  4Gi         3Gi (75%)    3m   ip-10-0-1-9.ec2.internal"
        );
    }

    #[test]
    fn a_pod_that_asked_for_nothing_keeps_the_bare_usage_figure_and_earns_no_req_column() {
        // The honest reading: such a pod has no denominator, and a percentage
        // of zero is not a cell. `CPU REQ`/`MEMORY REQ` do not appear at all —
        // nobody here asked for anything, so two columns of `0` would be noise
        // rather than the fact this listing has to report.
        let rendered = render(
            &sampled_rows(),
            "prod (us-east-1)",
            &Scope::Namespace("payments".to_owned()),
            &unfiltered(),
            &[],
            Width::Default,
            Palette::Plain,
        );

        assert_eq!(
            rendered,
            "NAME           READY  STATUS   RESTARTS  CPU    MEMORY  AGE  NODE\n\
             api-7c9f       1/1    Running  0         250m   512Mi   90m  ip-10-0-1-9.ec2.internal\n\
             checkout-5d4b  1/1    Running  0         1200m  3Gi     3m   ip-10-0-1-9.ec2.internal"
        );
    }

    #[test]
    fn usage_past_the_request_is_reported_rather_than_capped() {
        // The pod being throttled, or the one about to be OOM-killed. Capping
        // at 100% would hide the only moment anybody reads this column for.
        let row = PodRow::from_pod(
            &asking(&[("cpu", "100m"), ("memory", "256Mi")]),
            Some(used("450m", "1Gi")),
            now(),
        );

        assert_eq!(Column::Cpu.text(&row), "450m (450%)");
        assert_eq!(Column::Memory.text(&row), "1Gi (400%)");
        // The number the percentage is a share of lives in its own column now.
        assert_eq!(Column::CpuRequested.text(&row), "100m");
        assert_eq!(Column::MemoryRequested.text(&row), "256Mi");
    }

    #[test]
    fn a_pod_using_nothing_reads_as_zero_of_its_request() {
        // A real zero, unlike the `-` of a pod nobody has sampled: this one was
        // measured and is idle, which is a fact about the pod rather than about
        // the scraper, and the two must not render alike.
        let row = PodRow::from_pod(
            &asking(&[("cpu", "500m"), ("memory", "1Gi")]),
            Some(used("0", "0")),
            now(),
        );

        assert_eq!(Column::Cpu.text(&row), "0 (0%)");
    }

    #[test]
    fn a_request_is_known_even_when_nothing_has_measured_it() {
        // A request is not a measurement, so it does not wait on one:
        // `CPU REQ`/`MEMORY REQ` come straight from `effective_requests` and
        // read the same whether or not metrics-server has ever sampled this
        // pod. Rendering `-/500m` in the old paired cell would have put a
        // figure in a place that has none, and `0/500m (0%)` would have
        // invented an idle pod — this pod was never measured at all.
        let row = PodRow::from_pod(&asking(&[("cpu", "500m"), ("memory", "1Gi")]), None, now());

        assert_eq!(Column::Cpu.text(&row), "-");
        assert_eq!(Column::Memory.text(&row), "-");
        assert_eq!(Column::CpuRequested.text(&row), "500m");
        assert_eq!(Column::MemoryRequested.text(&row), "1Gi");
    }

    #[test]
    fn a_request_on_one_resource_still_shows_both_req_columns_paired() {
        // Setting a memory request and leaving CPU unbounded is a common
        // shape. Unlike the usage pair, `CPU REQ`/`MEMORY REQ` are not asked
        // per resource — a pod that requested memory and not CPU reads a
        // real `0` under `CPU REQ` rather than losing the column, so the
        // request question is answered by one pair rather than two rules.
        let row = PodRow::from_pod(
            &asking(&[("memory", "1Gi")]),
            Some(used("250m", "512Mi")),
            now(),
        );
        let rows = vec![row.clone()];

        assert_eq!(
            columns(
                &Scope::Namespace("payments".to_owned()),
                &rows,
                Width::Default
            ),
            vec![
                Column::Name,
                Column::Ready,
                Column::Status,
                Column::Restarts,
                Column::CpuRequested,
                Column::Cpu,
                Column::MemoryRequested,
                Column::Memory,
                Column::Age,
                Column::Node,
            ]
        );
        // CPU has no request of its own, so its usage cell falls back to the
        // bare figure exactly as an entirely unrequested pod's would.
        assert_eq!(Column::CpuRequested.text(&row), "0");
        assert_eq!(Column::Cpu.text(&row), "250m");
        assert_eq!(Column::MemoryRequested.text(&row), "1Gi");
        assert_eq!(Column::Memory.text(&row), "512Mi (50%)");
    }

    #[test]
    fn one_pod_with_a_request_earns_the_column_for_every_row() {
        // `any`, like the usage columns themselves: one pod that asked for
        // something earns `CPU REQ`/`MEMORY REQ` for the whole listing, and
        // the rows around it that asked for nothing read their own honest `0`
        // under the same columns rather than losing them.
        let mut rows = requesting_rows();
        rows[1] = PodRow::from_pod(&healthy(), Some(used("1200m", "3Gi")), now());

        let rendered = render(
            &rows,
            "prod (us-east-1)",
            &Scope::Namespace("payments".to_owned()),
            &unfiltered(),
            &[],
            Width::Default,
            Palette::Plain,
        );

        assert!(
            rendered.starts_with("NAME      READY  STATUS   RESTARTS  CPU REQ"),
            "{rendered}"
        );
        assert!(rendered.contains("500m     250m (50%)"), "{rendered}");
        assert!(rendered.contains("0        1200m"), "{rendered}");
    }

    #[test]
    fn a_request_shows_on_a_cluster_with_no_metrics_server() {
        // The point of the feature: a pod's request reaches the table without
        // ever needing metrics-server, so `eks pods` on the EKS default — no
        // add-on installed — still says what was booked. `CPU`/`MEMORY`, the
        // usage columns, stay absent, exactly as they do today with no
        // metrics-server at all: nothing here was sampled.
        let rows = vec![PodRow::from_pod(
            &asking(&[("cpu", "500m"), ("memory", "1Gi")]),
            None,
            now(),
        )];
        let scope = Scope::Namespace("payments".to_owned());

        let cols = columns(&scope, &rows, Width::Default);
        assert!(cols.contains(&Column::CpuRequested), "{cols:?}");
        assert!(cols.contains(&Column::MemoryRequested), "{cols:?}");
        assert!(!cols.contains(&Column::Cpu), "{cols:?}");
        assert!(!cols.contains(&Column::Memory), "{cols:?}");

        let rendered = render(
            &rows,
            "prod (us-east-1)",
            &scope,
            &unfiltered(),
            &[],
            Width::Default,
            Palette::Plain,
        );
        assert!(rendered.contains("CPU REQ"), "{rendered}");
        assert!(rendered.contains("MEMORY REQ"), "{rendered}");
        assert!(rendered.contains("500m"), "{rendered}");
        assert!(rendered.contains("1Gi"), "{rendered}");
    }

    /// The healthy pod, asking for one extended resource.
    fn asking_device(name: &str, count: &str) -> Pod {
        let mut pod = healthy();
        if let Some(spec) = pod.spec.as_mut() {
            for container in &mut spec.containers {
                container.resources = Some(ResourceRequirements {
                    requests: Some(
                        [(name.to_owned(), ApiQuantity(count.to_owned()))]
                            .into_iter()
                            .collect(),
                    ),
                    ..Default::default()
                });
            }
        }
        pod
    }

    #[test]
    fn a_device_request_earns_its_own_column() {
        let rows = vec![PodRow::from_pod(
            &asking_device("nvidia.com/gpu", "2"),
            None,
            now(),
        )];
        let scope = Scope::Namespace("payments".to_owned());

        let cols = columns(&scope, &rows, Width::Default);
        assert!(cols.contains(&Column::Device("nvidia.com/gpu")), "{cols:?}");

        let rendered = render(
            &rows,
            "prod (us-east-1)",
            &scope,
            &unfiltered(),
            &[],
            Width::Default,
            Palette::Plain,
        );
        assert!(rendered.contains("NVIDIA.COM/GPU"), "{rendered}");
        assert!(
            rendered.contains("api-7c9f  1/1    Running  0         2"),
            "{rendered}"
        );
    }

    #[test]
    fn a_pod_that_did_not_ask_for_a_device_reads_a_real_zero_not_unknown() {
        // Unlike the node table, where `-` tells "no such hardware" apart from
        // "none free": every pod could in principle ask for any resource, so
        // not asking is itself a real zero rather than an absence.
        let rows = [
            PodRow::from_pod(&asking_device("nvidia.com/gpu", "2"), None, now()),
            PodRow::from_pod(&healthy(), None, now()),
        ];

        assert_eq!(
            Column::Device("nvidia.com/gpu").text(&rows[1]),
            "0",
            "a pod that never asked reads 0, not -"
        );
    }

    #[test]
    fn a_cluster_with_no_extended_requests_gains_no_device_column() {
        let rows = requesting_rows();
        let cols = columns(&one_namespace(), &rows, Width::Default);

        assert!(
            !cols.iter().any(|c| matches!(c, Column::Device(_))),
            "{cols:?}"
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
            Width::Default,
            Palette::Plain,
        );

        assert!(rendered.contains("api-7c9f"), "{rendered}");
        assert!(
            rendered.contains("\n\nCPU and MEMORY are not shown"),
            "{rendered}"
        );
        assert!(rendered.contains("no metrics.k8s.io API"), "{rendered}");
    }

    #[test]
    fn an_unsampled_namespace_is_told_apart_from_a_cluster_with_no_metrics_server() {
        // Same two missing columns, opposite advice. `eks pods` meets this more
        // often than `eks nodes` does, because a namespace whose pods have only
        // just been created is scraped a moment after they start.
        let rendered = render(
            &rows(),
            "prod (us-east-1)",
            &Scope::Namespace("payments".to_owned()),
            &unfiltered(),
            &[usage_unsampled(&crate::k8s::metrics::unsampled(
                "prod (us-east-1)",
            ))],
            Width::Default,
            Palette::Plain,
        );

        assert!(rendered.contains("api-7c9f"), "{rendered}");
        assert!(
            rendered.contains(
                "\n\nCPU and MEMORY are not shown because nothing here has been sampled yet."
            ),
            "{rendered}"
        );
        assert!(rendered.contains("kube-system"), "{rendered}");
        assert!(
            !rendered.contains("github.com/kubernetes-sigs/metrics-server"),
            "advice to install what is already installed: {rendered}"
        );
    }

    #[test]
    fn a_sampled_listing_says_how_old_its_figures_are() {
        // The same line `eks nodes` prints, in the same words, because it is a
        // fact about metrics-server rather than about either table.
        let sample = crate::k8s::metrics::Sample {
            usage: crate::k8s::metrics::Usage::default(),
            taken_at: Some(now() - SignedDuration::from_secs(12)),
            window: Some(SignedDuration::from_secs(20)),
        };
        let freshness = crate::k8s::metrics::freshness(&[sample], now())
            .expect("a stamped sample dates the listing");

        let rendered = render(
            &rows(),
            "prod (us-east-1)",
            &Scope::Namespace("payments".to_owned()),
            &unfiltered(),
            &[crate::k8s::metrics::freshness_note(freshness)],
            Width::Default,
            Palette::Plain,
        );

        assert!(
            rendered.ends_with("\n\nUsage is up to 12s old, averaged over 20s."),
            "{rendered}"
        );
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
            Width::Default,
            Palette::Plain,
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
                Width::Default,
                Palette::Plain,
            )
        );
        assert_eq!(paragraphs[2], "Sorted by restarts.");
    }

    #[test]
    fn an_ordering_that_ranked_no_pod_says_so_under_the_sort_note() {
        // `eks pods --sort cpu` where metrics-server could not be read: the
        // table has no CPU column, and `Sorted by cpu.` on its own describes a
        // listing the alphabet arranged. The footnote above already says what to
        // install, so the note points at it and spends its own second line on
        // the orderings that would have worked here.
        let order = crate::k8s::pods::Order::Cpu;
        let missing = crate::k8s::pods::Missing { usage: true };
        let notes: Vec<String> =
            crate::k8s::order::note(order, crate::k8s::order::Direction::Natural)
                .into_iter()
                .chain(crate::k8s::order::unranked_note(
                    order,
                    crate::k8s::pods::cause(order, missing),
                    |candidate| crate::k8s::pods::ranks_any(&rows(), candidate),
                    |candidate| crate::k8s::pods::distinguishes(&rows(), candidate),
                ))
                .collect();

        let output = render(
            &rows(),
            "prod (us-east-1)",
            &Scope::Namespace("payments".to_owned()),
            &unfiltered(),
            &notes,
            Width::Default,
            Palette::Plain,
        );
        let paragraphs: Vec<&str> = output.split("\n\n").collect();

        assert_eq!(paragraphs[1], "Sorted by cpu.");
        assert_eq!(
            paragraphs[2],
            "Nothing here has cpu to sort by, for the reason above.\n\
             Sort by age instead."
        );
    }

    #[test]
    fn a_healthy_namespace_sorted_by_restarts_is_told_what_else_to_try() {
        // The other half of the roadmap entry behind these notes: nothing has
        // crashed, so nothing ranked, and there is no failure above the table to
        // point at — because nothing failed. All the note has to offer is the
        // flag that would reorder this listing, so it had better offer it, and
        // this listing has three of them.
        let order = crate::k8s::pods::Order::Restarts;
        let note = crate::k8s::order::unranked_note(
            order,
            crate::k8s::pods::cause(order, crate::k8s::pods::Missing::default()),
            |candidate| crate::k8s::pods::ranks_any(&sampled_rows(), candidate),
            |candidate| crate::k8s::pods::distinguishes(&sampled_rows(), candidate),
        );

        assert_eq!(
            note.as_deref(),
            Some(
                "Nothing here has restarts to sort by.\n\
                 Sort by age, cpu, or memory instead."
            )
        );
    }

    #[test]
    fn an_empty_listing_says_nothing_about_an_ordering_that_ranked_nothing() {
        // Nothing ranked, because there is nothing at all. "No pods matched" is
        // the answer; a note about the sort would be noise on top of it.
        let order = crate::k8s::pods::Order::Cpu;
        let note = crate::k8s::order::unranked_note(
            order,
            crate::k8s::pods::cause(order, crate::k8s::pods::Missing { usage: true }),
            |candidate| crate::k8s::pods::ranks_any(&[], candidate),
            |candidate| crate::k8s::pods::distinguishes(&[], candidate),
        )
        .expect("an ordering with no rows to rank ranked nothing");

        let message = render(
            &[],
            "prod (us-east-1)",
            &Scope::Namespace("payments".to_owned()),
            &unfiltered(),
            &[note],
            Width::Default,
            Palette::Plain,
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
            Width::Default,
            Palette::Plain,
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
            Width::Default,
            Palette::Plain,
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

    /// The healthy pod with the fields only `--wide` shows filled in.
    fn wide_pod() -> Pod {
        let mut pod = healthy();
        if let Some(status) = pod.status.as_mut() {
            status.pod_ip = Some("10.0.1.42".to_owned());
            status.pod_ips = Some(vec![PodIP {
                ip: "10.0.1.42".to_owned(),
            }]);
        }
        pod
    }

    #[test]
    fn the_default_pod_table_holds_the_wide_columns_back() {
        let rows = [PodRow::from_pod(&wide_pod(), None, now())];

        let table = render(
            &rows,
            "prod (us-east-1)",
            &Scope::Namespace("payments".to_owned()),
            &unfiltered(),
            &[],
            Width::Default,
            Palette::Plain,
        );

        for held_back in ["IP", "NOMINATED NODE", "READINESS GATES"] {
            assert!(!table.contains(held_back), "{held_back} in {table}");
        }
    }

    #[test]
    fn wide_adds_the_address_the_nominated_node_and_the_readiness_gates() {
        let rows = [PodRow::from_pod(&wide_pod(), None, now())];

        assert_eq!(
            render(
                &rows,
                "prod (us-east-1)",
                &Scope::Namespace("payments".to_owned()),
                &unfiltered(),
                &[],
                Width::Wide,
                Palette::Plain,
            ),
            "NAME      READY  STATUS   RESTARTS  AGE  IP         NODE                      NOMINATED NODE  READINESS GATES\n\
             api-7c9f  1/1    Running  0         90m  10.0.1.42  ip-10-0-1-9.ec2.internal  -               -"
        );
    }

    #[test]
    fn the_wide_pod_columns_sit_where_kubectl_puts_them() {
        // `kubectl -o wide` ends AGE, IP, NODE, NOMINATED NODE, READINESS
        // GATES. NODE is in this table by default, so `--wide` fills in the
        // three around it rather than appending four.
        let rows = [PodRow::from_pod(&wide_pod(), None, now())];
        let scope = Scope::Namespace("payments".to_owned());

        let narrow = columns(&scope, &rows, Width::Default);
        let wide = columns(&scope, &rows, Width::Wide);

        assert_eq!(
            narrow,
            [
                Column::Name,
                Column::Ready,
                Column::Status,
                Column::Restarts,
                Column::Age,
                Column::Node,
            ]
        );
        assert_eq!(
            wide,
            [
                Column::Name,
                Column::Ready,
                Column::Status,
                Column::Restarts,
                Column::Age,
                Column::Ip,
                Column::Node,
                Column::NominatedNode,
                Column::ReadinessGates,
            ]
        );
    }

    #[test]
    fn the_wide_pod_columns_compose_with_the_namespace_and_usage_ones() {
        // Three independent conditions decide this layout, and `--wide` must
        // disturb neither of the other two.
        let rows = [PodRow::from_pod(
            &wide_pod(),
            Some(used("250m", "512Mi")),
            now(),
        )];

        let headers: Vec<String> = columns(&Scope::All, &rows, Width::Wide)
            .iter()
            .map(|column| column.header())
            .collect();

        assert_eq!(
            headers,
            [
                "NAMESPACE",
                "NAME",
                "READY",
                "STATUS",
                "RESTARTS",
                "CPU",
                "MEMORY",
                "AGE",
                "IP",
                "NODE",
                "NOMINATED NODE",
                "READINESS GATES",
            ]
        );
    }

    #[test]
    fn the_wide_columns_appear_even_when_every_one_of_them_is_empty() {
        // Deliberately unlike the usage columns, which are dropped when a
        // cluster has nothing to put in them. Those arrive unasked for; this
        // flag was typed, and a column of `-` is the answer "nothing here is
        // being preempted" rather than a flag that did nothing.
        let rows = [PodRow::from_pod(&healthy(), None, now())];

        let table = render(
            &rows,
            "prod (us-east-1)",
            &Scope::Namespace("payments".to_owned()),
            &unfiltered(),
            &[],
            Width::Wide,
            Palette::Plain,
        );

        assert!(table.contains("NOMINATED NODE"), "{table}");
        assert!(table.contains("READINESS GATES"), "{table}");
    }

    #[test]
    fn a_pod_with_no_address_yet_reads_as_a_dash() {
        // Scheduled but not networked: the CNI has not handed out an address.
        assert_eq!(PodRow::from_pod(&healthy(), None, now()).ip, "-");

        let mut blank = wide_pod();
        if let Some(status) = blank.status.as_mut() {
            status.pod_ip = Some(String::new());
            status.pod_ips = Some(Vec::new());
        }
        assert_eq!(PodRow::from_pod(&blank, None, now()).ip, "-");
    }

    #[test]
    fn a_dual_stack_pod_shows_the_first_of_its_addresses() {
        let mut pod = wide_pod();
        if let Some(status) = pod.status.as_mut() {
            status.pod_ips = Some(vec![
                PodIP {
                    ip: "2600:1f13::42".to_owned(),
                },
                PodIP {
                    ip: "10.0.1.42".to_owned(),
                },
            ]);
            // The legacy field agrees with the list on a real cluster; it is
            // set to something else here so the test can say which one is read.
            status.pod_ip = Some("10.0.1.42".to_owned());
        }

        assert_eq!(PodRow::from_pod(&pod, None, now()).ip, "2600:1f13::42");
    }

    #[test]
    fn a_pod_reporting_only_the_older_address_field_still_shows_one() {
        let mut pod = wide_pod();
        if let Some(status) = pod.status.as_mut() {
            status.pod_ips = None;
        }

        assert_eq!(PodRow::from_pod(&pod, None, now()).ip, "10.0.1.42");
    }

    #[test]
    fn a_pod_awaiting_preemption_names_the_node_it_is_promised() {
        // The one case where `Pending` is not a stuck pod: the scheduler is
        // evicting something to make room, and this is where it will land.
        let mut pod = wide_pod();
        if let Some(status) = pod.status.as_mut() {
            status.phase = Some("Pending".to_owned());
            status.nominated_node_name = Some("ip-10-0-2-7.ec2.internal".to_owned());
        }

        let row = PodRow::from_pod(&pod, None, now());
        assert_eq!(row.nominated_node, "ip-10-0-2-7.ec2.internal");
    }

    #[test]
    fn readiness_gates_count_only_the_conditions_that_are_true() {
        let mut pod = wide_pod();
        if let Some(spec) = pod.spec.as_mut() {
            spec.readiness_gates = Some(vec![
                gate("target-health.elbv2.k8s.aws/pod-readiness"),
                gate("example.com/feature-flag"),
                // Declared, but the controller has recorded nothing for it —
                // which is exactly as unsatisfied as a False.
                gate("example.com/never-reported"),
            ]);
        }
        if let Some(status) = pod.status.as_mut() {
            status.conditions = Some(vec![
                condition("Ready", "True", None),
                condition("target-health.elbv2.k8s.aws/pod-readiness", "True", None),
                condition("example.com/feature-flag", "False", None),
            ]);
        }

        assert_eq!(
            PodRow::from_pod(&pod, None, now()).readiness_gates,
            Some("1/3".to_owned())
        );
    }

    #[test]
    fn a_pod_with_no_readiness_gates_reads_as_a_dash_rather_than_zero_of_zero() {
        // `0/0` on the overwhelming majority of rows would suggest something
        // unsatisfied where there is nothing to satisfy.
        assert_eq!(
            PodRow::from_pod(&wide_pod(), None, now()).readiness_gates,
            None
        );

        let mut empty = wide_pod();
        if let Some(spec) = empty.spec.as_mut() {
            spec.readiness_gates = Some(Vec::new());
        }
        assert_eq!(PodRow::from_pod(&empty, None, now()).readiness_gates, None);

        let rows = [PodRow::from_pod(&empty, None, now())];
        let table = render(
            &rows,
            "prod (us-east-1)",
            &Scope::Namespace("payments".to_owned()),
            &unfiltered(),
            &[],
            Width::Wide,
            Palette::Plain,
        );
        assert!(table.ends_with("-               -"), "{table}");
    }

    #[test]
    fn a_pod_with_nothing_filled_in_still_produces_the_wide_cells() {
        let row = PodRow::from_pod(&Pod::default(), None, now());

        assert_eq!(row.ip, "-");
        assert_eq!(row.nominated_node, "-");
        assert_eq!(row.readiness_gates, None);
    }

    #[test]
    fn an_empty_wide_listing_still_says_where_the_pods_went() {
        // `--wide` changes columns, and there are no columns here to change.
        let scope = Scope::Namespace("payments".to_owned());
        assert_eq!(
            render(
                &[],
                "prod (us-east-1)",
                &scope,
                &unfiltered(),
                &[],
                Width::Wide,
                Palette::Plain
            ),
            render(
                &[],
                "prod (us-east-1)",
                &scope,
                &unfiltered(),
                &[],
                Width::Default,
                Palette::Plain,
            )
        );
    }

    #[test]
    fn a_wide_pod_table_keeps_its_footnotes() {
        let rows = [PodRow::from_pod(&wide_pod(), None, now())];
        let note = "Sorted by cpu.".to_owned();

        let output = render(
            &rows,
            "prod (us-east-1)",
            &Scope::All,
            &unfiltered(),
            std::slice::from_ref(&note),
            Width::Wide,
            Palette::Plain,
        );

        assert!(output.ends_with(&format!("\n\n{note}")), "{output}");
    }

    // --- Narrow mode --------------------------------------------------------
    //
    // The width tests measure `requesting_rows`: two pods, each asking for
    // something and each sampled, which is the shape nearly every real
    // deployment has and the widest the default table gets without `--wide`.

    fn headings_at(scope: &Scope, rows: &[PodRow], target: u16) -> Vec<String> {
        columns(scope, rows, Width::Narrow(target))
            .iter()
            .map(|column| column.header())
            .collect()
    }

    fn one_namespace() -> Scope {
        Scope::Namespace("payments".to_owned())
    }

    #[test]
    fn a_wide_enough_narrow_pod_table_is_the_default_one_byte_for_byte() {
        // Narrowing is subtraction, and a table that already fits has nothing
        // to subtract: a terminal roomier than the row must leave it alone,
        // columns and rendered bytes alike. 200 cols is wider than any table
        // this file produces.
        let rows = requesting_rows();
        let scope = one_namespace();

        assert_eq!(
            columns(&scope, &rows, Width::Narrow(200)),
            columns(&scope, &rows, Width::Default)
        );
        assert_eq!(
            render(
                &rows,
                "prod (us-east-1)",
                &scope,
                &unfiltered(),
                &[],
                Width::Narrow(200),
                Palette::Plain
            ),
            render(
                &rows,
                "prod (us-east-1)",
                &scope,
                &unfiltered(),
                &[],
                Width::Default,
                Palette::Plain
            ),
        );
    }

    #[test]
    fn a_row_that_barely_overflows_drops_age_first() {
        // The fixture's default row is 118 characters now that the request
        // pair has its own two columns — `NAME(13) READY(5) STATUS(7)
        // RESTARTS(8) CPU REQ(7) CPU(12) MEMORY REQ(10) MEMORY(11) AGE(3)
        // NODE(24)` with nine two-space separators — and dropping `AGE` alone
        // gets it to 113: three characters of information the table already
        // carries in `RESTARTS`'s `9 (5m ago)`.
        let rows = requesting_rows();

        assert_eq!(
            headings_at(&one_namespace(), &rows, 115),
            [
                "NAME",
                "READY",
                "STATUS",
                "RESTARTS",
                "CPU REQ",
                "CPU",
                "MEMORY REQ",
                "MEMORY",
                "NODE"
            ],
        );
    }

    #[test]
    fn ninety_columns_keep_both_pairs_and_let_the_node_go() {
        // 90 cols is past `AGE` and `NODE` both — `NODE` is the widest cell in
        // the table on EKS, a forty-character DNS name, and it answers the
        // question you ask *after* you have found the pod, so it goes before
        // the columns that find it — and short of where the request pair has
        // to leave too.
        let rows = requesting_rows();
        let scope = one_namespace();

        assert_eq!(
            headings_at(&scope, &rows, 90),
            [
                "NAME",
                "READY",
                "STATUS",
                "RESTARTS",
                "CPU REQ",
                "CPU",
                "MEMORY REQ",
                "MEMORY",
            ],
        );
        // And the columns it reported really do fit: the assertion is over the
        // rendered table rather than over the arithmetic that chose it, so a
        // drop rule measuring rows the renderer disagreed with would fail here.
        let table = render(
            &rows,
            "prod (us-east-1)",
            &scope,
            &unfiltered(),
            &[],
            Width::Narrow(90),
            Palette::Plain,
        );
        for line in table.lines() {
            assert!(line.chars().count() <= 90, "{line:?} is wider than 90");
        }
    }

    #[test]
    fn eighty_columns_let_the_request_pair_go_too() {
        // 80 cols is the width every laptop lid narrows to under a docked
        // browser, and it is past where `CPU REQ`/`MEMORY REQ` fit beside the
        // usage pair: dropping the request leaves capacity to burn, which is
        // the "what is this pod doing right now" question — the pair the
        // tool exists for, so it is the last of the four resource columns to
        // go.
        let rows = requesting_rows();
        let scope = one_namespace();

        assert_eq!(
            headings_at(&scope, &rows, 80),
            ["NAME", "READY", "STATUS", "RESTARTS", "CPU", "MEMORY"],
        );
        let table = render(
            &rows,
            "prod (us-east-1)",
            &scope,
            &unfiltered(),
            &[],
            Width::Narrow(80),
            Palette::Plain,
        );
        for line in table.lines() {
            assert!(line.chars().count() <= 80, "{line:?} is wider than 80");
        }
    }

    #[test]
    fn a_row_narrower_than_the_name_still_prints_the_name() {
        // `--width 1`, the acceptance-test extreme: every droppable column is
        // gone and `NAME` stays, even though the row is still wider than one
        // character. A listing of rows nobody can identify is worse than a
        // listing the terminal wraps.
        assert_eq!(
            headings_at(&one_namespace(), &requesting_rows(), 1),
            ["NAME"]
        );
    }

    #[test]
    fn a_terminal_reporting_no_columns_at_all_is_the_same_as_one_column() {
        // A `Narrow(0)` is a terminal-size query that answered nonsense rather
        // than a width anybody has. Nothing fits it, so the drop rule runs to
        // the end and leaves what it never drops — the same answer as 1, and
        // not an underflow or an empty row.
        assert_eq!(
            headings_at(&one_namespace(), &requesting_rows(), 0),
            ["NAME"]
        );
    }

    #[test]
    fn the_columns_that_fit_are_measured_from_the_cells_not_the_headings() {
        // `NODE` is four characters of heading over a forty-character node
        // name, and the drop rule has to read the cells to know that. The same
        // listing on a cluster with short node names keeps the column at a
        // width where the EKS-shaped one loses it.
        let mut short = healthy();
        if let Some(spec) = short.spec.as_mut() {
            spec.node_name = Some("node-1".to_owned());
        }
        let short = [PodRow::from_pod(&short, None, now())];
        let long = [PodRow::from_pod(&healthy(), None, now())];
        let scope = one_namespace();

        assert!(headings_at(&scope, &short, 45).iter().any(|h| h == "NODE"));
        assert!(!headings_at(&scope, &long, 45).iter().any(|h| h == "NODE"));
    }

    #[test]
    fn a_cluster_wide_listing_keeps_the_namespace_beside_the_name() {
        // Under `-A` a pod's identity is the pair: `coredns-abc` in
        // `kube-system` and a copy of it elsewhere are two different pods, and
        // `NAMESPACE` is the column the user widened the scope to get. So it
        // drops when `NAME` does, which is never.
        let rows = requesting_rows();

        assert_eq!(headings_at(&Scope::All, &rows, 1), ["NAMESPACE", "NAME"]);
        // And it is still the first column at a width that drops nothing else,
        // rather than having been shuffled to the end by the retain.
        assert_eq!(
            columns(&Scope::All, &rows, Width::Narrow(200)),
            columns(&Scope::All, &rows, Width::Default)
        );
    }

    #[test]
    fn the_request_and_usage_columns_each_leave_together_rather_than_singly() {
        // `CPU REQ` beside `AGE` with no `MEMORY REQ` between them is half an
        // answer, and an eye reading a row of pairs pairs the wrong ones. The
        // same rule the node table's REQ and USE pairs follow — and the same
        // rule for each of the two pairs here, which now leave at different
        // widths from each other.
        let rows = requesting_rows();
        let scope = one_namespace();

        // A width tight enough to lose one of a pair loses both, at every step
        // small enough to force the question. A loop rather than one number,
        // because the invariant is the pairing and not the fixture's exact
        // cell widths.
        for target in [1_u16, 20, 40, 60, 73, 80, 85, 90, 100, 200] {
            let cols = headings_at(&scope, &rows, target);
            let has = |heading: &str| cols.iter().any(|h| h == heading);
            assert_eq!(has("CPU REQ"), has("MEMORY REQ"), "{target}: {cols:?}");
            assert_eq!(has("CPU"), has("MEMORY"), "{target}: {cols:?}");
        }
    }

    #[test]
    fn the_health_columns_go_last_and_status_goes_after_ready() {
        // What survives when almost nothing does: the word that names the
        // problem. `READY`'s `0/1` is the detail under `CrashLoopBackOff`, so
        // it goes first of the two, and `RESTARTS` — the widest of the three
        // health columns — goes before either.
        let rows = requesting_rows();
        let scope = one_namespace();

        assert_eq!(
            headings_at(&scope, &rows, 50),
            ["NAME", "READY", "STATUS", "RESTARTS"]
        );
        assert_eq!(headings_at(&scope, &rows, 30), ["NAME", "READY", "STATUS"]);
        assert_eq!(headings_at(&scope, &rows, 22), ["NAME", "STATUS"]);
    }

    #[test]
    fn wide_beats_narrow_when_both_could_apply() {
        // `--wide` was typed; the terminal was not. `Width::for_terminal`
        // makes that choice, and this asserts the listing agrees: a `Wide` is
        // the wide set even where the row is far past any terminal.
        let rows = requesting_rows();
        let wide = columns(&one_namespace(), &rows, Width::Wide);

        assert!(wide.contains(&Column::Ip));
        assert!(wide.contains(&Column::NominatedNode));
        assert!(wide.contains(&Column::ReadinessGates));
    }

    #[test]
    fn an_empty_listing_says_the_same_thing_at_any_width() {
        // There are no columns to drop and no table to fit them in; a terminal
        // width must not change the sentence explaining why the listing is
        // blank.
        let scope = one_namespace();
        let empty = render(
            &[],
            "prod (us-east-1)",
            &scope,
            &unfiltered(),
            &[],
            Width::Default,
            Palette::Plain,
        );

        for width in [Width::Narrow(1), Width::Narrow(80), Width::Narrow(200)] {
            assert_eq!(
                render(
                    &[],
                    "prod (us-east-1)",
                    &scope,
                    &unfiltered(),
                    &[],
                    width,
                    Palette::Plain
                ),
                empty
            );
        }
    }

    #[test]
    fn a_narrowed_table_still_renders_its_footnotes() {
        // The notes are the reason a missing column is not a mystery, and a
        // narrow terminal is where columns go missing. Dropping them to save
        // two lines would take away the explanation exactly where it is most
        // needed.
        let rows = requesting_rows();
        let note = usage_unavailable("Install metrics-server to see live usage.");

        let table = render(
            &rows,
            "prod (us-east-1)",
            &one_namespace(),
            &unfiltered(),
            std::slice::from_ref(&note),
            Width::Narrow(60),
            Palette::Plain,
        );

        assert!(table.ends_with(&note), "{table}");
    }

    #[test]
    fn a_listing_with_no_metrics_narrows_from_its_own_shorter_row() {
        // The usage columns are absent on a cluster with no metrics-server, so
        // the row starts shorter and the same terminal drops less. The drop
        // rule reads the columns the listing actually has rather than the ones
        // it might have had.
        let rows = [
            PodRow::from_pod(&healthy(), None, now()),
            PodRow::from_pod(&healthy(), None, now()),
        ];
        let scope = one_namespace();

        assert_eq!(
            headings_at(&scope, &rows, 80),
            ["NAME", "READY", "STATUS", "RESTARTS", "AGE", "NODE"]
        );
        assert_eq!(
            columns(&scope, &rows, Width::Narrow(80)),
            columns(&scope, &rows, Width::Default)
        );
    }

    // ---- Severity colour ----------------------------------------------------

    /// A palette that paints, without asking a terminal anything.
    fn colour() -> Palette {
        Palette::choose(crate::theme::ColourChoice::Always, false, None, None)
    }

    /// A pod stuck in a crash loop: the row this whole feature is for.
    fn crashing() -> Pod {
        let mut pod = pod(
            PodSpec {
                containers: vec![container("app")],
                node_name: Some(NODE.to_owned()),
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
        pod.metadata.name = Some("checkout-5d4b".to_owned());
        pod
    }

    #[test]
    fn a_status_cell_is_graded_by_the_row_the_dashboard_would_grade() {
        // The CLI takes the severity `PodRow` already carries rather than
        // re-deciding what `CrashLoopBackOff` means, so the two surfaces
        // cannot come to disagree about a pod.
        for pod in [healthy(), crashing()] {
            let row = PodRow::from_pod(&pod, None, now());
            assert_eq!(Column::Status.severity(&row), Some(row.severity));
        }

        assert_eq!(
            Column::Status.severity(&PodRow::from_pod(&crashing(), None, now())),
            Some(Severity::Critical)
        );
    }

    #[test]
    fn status_is_the_only_graded_column_in_this_table() {
        // `READY` is not a second one: `0/1` is *why* a `Running` pod grades
        // `Warn`, so colouring it too would paint one judgement across two
        // columns. And the usage pair is not a third — see `Column::severity`
        // for why it is waiting on a decision rather than on an implementation.
        let row = PodRow::from_pod(
            &asking(&[("cpu", "500m"), ("memory", "1Gi")]),
            Some(used("450m", "990Mi")),
            now(),
        );

        assert_eq!(Column::Status.severity(&row), Some(row.severity));
        for column in [
            Column::Namespace,
            Column::Name,
            Column::Ready,
            Column::Restarts,
            Column::CpuRequested,
            Column::Cpu,
            Column::MemoryRequested,
            Column::Memory,
            Column::Device("nvidia.com/gpu"),
            Column::Age,
            Column::Ip,
            Column::Node,
            Column::NominatedNode,
            Column::ReadinessGates,
        ] {
            assert_eq!(
                column.severity(&row),
                None,
                "{} should not be graded",
                column.header()
            );
        }
    }

    #[test]
    fn a_pod_at_ninety_percent_of_its_own_request_is_not_painted_as_a_full_node() {
        // The reason the usage columns are ungraded, stated as an assertion:
        // this pod is at 90% of what it asked for, which
        // `Severity::from_utilisation` calls `Critical` for a node's
        // allocatable and which is a well-sized pod. Until "hot" is defined
        // for a request, colouring it would be telling the reader something
        // untrue in red on most of their rows.
        let row = PodRow::from_pod(&asking(&[("cpu", "500m")]), Some(used("450m", "0")), now());

        assert_eq!(Column::Cpu.text(&row), "450m (90%)");
        assert_eq!(Column::Cpu.severity(&row), None);
    }

    #[test]
    fn a_plain_table_is_unchanged_to_the_byte() {
        // `eks pods | grep` gets what it always got. Asserted with the ink
        // stripped back off the coloured table, so it covers every column.
        let rows = [
            PodRow::from_pod(&healthy(), Some(used("250m", "512Mi")), now()),
            PodRow::from_pod(&crashing(), Some(used("10m", "32Mi")), now()),
        ];
        let table = |palette| {
            render(
                &rows,
                "prod (us-east-1)",
                &Scope::Namespace("payments".to_owned()),
                &unfiltered(),
                &[],
                Width::Default,
                palette,
            )
        };

        assert_ne!(table(Palette::Plain), table(colour()));
        assert_eq!(table(Palette::Plain), strip_ansi(&table(colour())));
    }

    #[test]
    fn only_the_unhappy_rows_carry_ink() {
        // A namespace where everything is `Running` prints exactly the table
        // it printed before colour existed — which is the point of
        // `Theme::severity_ink` leaving `Ok` alone.
        let happy = [PodRow::from_pod(&healthy(), None, now())];
        let table = render(
            &happy,
            "prod (us-east-1)",
            &Scope::Namespace("payments".to_owned()),
            &unfiltered(),
            &[],
            Width::Default,
            colour(),
        );

        assert!(!table.contains('\u{1b}'), "{table:?}");
    }

    #[test]
    fn the_crashing_row_is_the_only_line_with_a_colour_on_it() {
        let rows = [
            PodRow::from_pod(&healthy(), None, now()),
            PodRow::from_pod(&crashing(), None, now()),
        ];

        let table = render(
            &rows,
            "prod (us-east-1)",
            &Scope::Namespace("payments".to_owned()),
            &unfiltered(),
            &[],
            Width::Default,
            colour(),
        );

        let inked: Vec<&str> = table
            .lines()
            .filter(|line| line.contains('\u{1b}'))
            .collect();
        assert_eq!(inked.len(), 1, "{table:?}");
        assert!(inked[0].contains("checkout-5d4b"), "{table:?}");
    }

    #[test]
    fn narrowing_drops_the_same_columns_whether_or_not_there_is_ink() {
        // Widths are measured from a cell's text, so a coloured `STATUS`
        // cannot make the drop rule take a column it would otherwise keep.
        let rows = [
            PodRow::from_pod(&healthy(), Some(used("250m", "512Mi")), now()),
            PodRow::from_pod(&crashing(), Some(used("10m", "32Mi")), now()),
        ];
        let table = |width, palette| {
            render(
                &rows,
                "prod (us-east-1)",
                &Scope::Namespace("payments".to_owned()),
                &unfiltered(),
                &[],
                width,
                palette,
            )
        };

        for target in [1, 40, 80, 100] {
            let width = Width::Narrow(target);
            assert_eq!(
                table(width, Palette::Plain),
                strip_ansi(&table(width, colour())),
                "at {target} columns"
            );
        }
    }

    #[test]
    fn an_empty_listing_says_the_same_thing_in_either_palette() {
        // There is no table to colour, and the message is advice rather than a
        // reading.
        let empty = |palette| {
            render(
                &[],
                "prod (us-east-1)",
                &Scope::Namespace("payments".to_owned()),
                &unfiltered(),
                &[],
                Width::Default,
                palette,
            )
        };

        assert_eq!(empty(Palette::Plain), empty(colour()));
    }

    /// Every escape sequence removed from a rendered table.
    fn strip_ansi(rendered: &str) -> String {
        let mut out = String::new();
        let mut chars = rendered.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\u{1b}' {
                out.push(c);
                continue;
            }
            if chars.peek() == Some(&'[') {
                chars.next();
            }
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
        out
    }
}
