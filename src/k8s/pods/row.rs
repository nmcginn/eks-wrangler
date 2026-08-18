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
use crate::k8s::pods::is_sidecar;
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
    pub age: String,
    /// The node the pod landed on, or `-` while it is still unscheduled.
    pub node: String,
}

impl PodRow {
    /// Build a row from a `Pod`, as of `now`.
    ///
    /// `now` is a parameter rather than a call to the clock so the age column
    /// is testable and so every row in one listing shares a single instant.
    #[must_use]
    pub fn from_pod(pod: &Pod, now: Timestamp) -> Self {
        let derived = derive(pod);

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
            age: pod.metadata.creation_timestamp.as_ref().map_or_else(
                || UNKNOWN.to_owned(),
                |created| format::human_duration(now.duration_since(created.0)),
            ),
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
        // restarted before the pod came up is history, not a live warning.
        restarts = init.sidecar_restarts.saturating_add(steady.restarts);
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

        init.restarts = init.restarts.saturating_add(container.restart_count);
        if sidecar {
            init.sidecar_restarts = init
                .sidecar_restarts
                .saturating_add(container.restart_count);
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

/// Render the `eks pods` table.
///
/// `cluster` is the human label used in the empty-list message, so a user who
/// typed the wrong `--context` or the wrong namespace finds out from the answer
/// rather than from a bare header.
#[must_use]
pub fn render(
    rows: &[PodRow],
    cluster: &str,
    scope: &super::Scope,
    selectors: &super::Selectors,
) -> String {
    if rows.is_empty() {
        return empty(cluster, scope, selectors);
    }

    let namespaced = scope.needs_namespace_column();
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            let mut cells = Vec::with_capacity(7);
            if namespaced {
                cells.push(row.namespace.clone());
            }
            cells.extend([
                row.name.clone(),
                row.ready.clone(),
                row.status.clone(),
                row.restarts.to_string(),
                row.age.clone(),
                row.node.clone(),
            ]);
            cells
        })
        .collect();

    let mut headers = vec!["NAME", "READY", "STATUS", "RESTARTS", "AGE", "NODE"];
    if namespaced {
        headers.insert(0, "NAMESPACE");
    }

    format::table(&headers, &cells)
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
        let row = PodRow::from_pod(&healthy(), now());

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

        let row = PodRow::from_pod(&pod, now());
        assert_eq!(row.status, "CrashLoopBackOff");
        assert_eq!(row.ready, "0/1");
        assert_eq!(row.restarts, 7);
        assert_eq!(row.severity, Severity::Critical);
    }

    #[test]
    fn a_pod_being_deleted_reads_as_terminating() {
        let mut terminating = healthy();
        terminating.metadata.deletion_timestamp = Some(ago(1));

        let row = PodRow::from_pod(&terminating, now());
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

        assert_eq!(PodRow::from_pod(&finished, now()).status, "Completed");
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

        let row = PodRow::from_pod(&lost, now());
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

        let row = PodRow::from_pod(&pod, now());
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

        assert_eq!(PodRow::from_pod(&pod, now()).status, "Init:1/3");
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

            let row = PodRow::from_pod(&pod, now());
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

        assert_eq!(PodRow::from_pod(&pod, now()).status, "Signal:9");
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

        let row = PodRow::from_pod(&pod, now());
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
        assert_eq!(PodRow::from_pod(&not_ready, now()).status, "NotReady");

        let ready = pod(
            spec,
            PodStatus {
                phase: Some("Running".to_owned()),
                conditions: Some(vec![condition("Ready", "True", None)]),
                container_statuses: Some(statuses),
                ..Default::default()
            },
        );
        assert_eq!(PodRow::from_pod(&ready, now()).status, "Running");
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

        let row = PodRow::from_pod(&pod, now());
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
            PodRow::from_pod(&pod, now()).status,
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

        let row = PodRow::from_pod(&pod, now());
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

        let row = PodRow::from_pod(&pod, now());
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

        let row = PodRow::from_pod(&pod, now());
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

        let row = PodRow::from_pod(&pod, now());
        assert_eq!(row.node, "-");
        assert_eq!(row.status, "Pending");
        assert_eq!(row.ready, "0/1");
        assert_eq!(row.severity, Severity::Warn);
    }

    #[test]
    fn a_pod_with_nothing_filled_in_still_produces_a_row() {
        // Every field under `status` is optional, and a pod caught between
        // admission and its first kubelet report really can arrive like this.
        let row = PodRow::from_pod(&Pod::default(), now());

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

        let row = PodRow::from_pod(&pod, now());
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
            PodRow::from_pod(&healthy(), now()),
            PodRow::from_pod(&other, now()),
        ]
    }

    #[test]
    fn a_namespaced_listing_does_not_repeat_the_namespace_on_every_row() {
        let rendered = render(
            &rows(),
            "prod (us-east-1)",
            &Scope::Namespace("payments".to_owned()),
            &unfiltered(),
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
        let rendered = render(&rows(), "prod (us-east-1)", &Scope::All, &unfiltered());

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
        );

        assert!(message.contains("prod (us-east-1)"), "{message}");
        assert!(message.contains("\"payments\""), "{message}");
        assert!(message.contains("--all-namespaces"), "{message}");
        assert!(!message.contains("NAME"), "{message}");
    }

    #[test]
    fn an_empty_cluster_wide_listing_suggests_checking_the_cluster() {
        let message = render(&[], "prod (us-east-1)", &Scope::All, &unfiltered());

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
        let message = render(&[], "prod (us-east-1)", &Scope::All, &filtered);

        assert!(message.contains("label selector `app=api`"), "{message}");
        assert!(
            message.contains("field selector `status.phase!=Running`"),
            "{message}"
        );
    }
}
