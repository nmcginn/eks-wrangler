//! Pods: fetching them, and working out what they have booked on a node.
//!
//! A node's allocatable capacity says what the scheduler *may* hand out. What
//! it has *already* handed out is the sum of the requests of the pods placed on
//! it, and that sum is the number that decides whether the next deployment gets
//! a pod or a `Pending` and an `Insufficient cpu` event. Capacity without it
//! answers half the question.
//!
//! The arithmetic is fiddlier than "add up the containers", and getting it
//! wrong is invisible — a total that is quietly 200m light still looks like a
//! plausible number. So [`effective_requests`] follows what the scheduler
//! actually does, and every clause of it is a test:
//!
//! - Init containers run one at a time and then stop, so they contribute the
//!   largest single one rather than their sum.
//! - A *sidecar* — an init container with `restartPolicy: Always` — keeps
//!   running once the app containers start, so it counts in the sum instead,
//!   and is also part of the footprint of every init container after it.
//! - Pod overhead, which a `RuntimeClass` adds for the sandbox itself, is charged
//!   on top of both.
//!
//! [`fetch`] is the only function here that touches the network.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{Container, Pod, PodSpec};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity as ApiQuantity;
use kube::Client;
use kube::api::{Api, ListParams};

use crate::k8s::quantity::Quantity;

pub mod order;
pub mod row;

pub use order::{Direction, Order, sort};
pub use row::{PodRow, render, usage_unavailable};

/// Pods that have finished linger in the API server until something collects
/// them, and they hold nothing on the node. Excluding them server-side keeps a
/// cluster full of completed Jobs from being paid for over the wire; the same
/// rule is applied again in [`by_node`], because a field selector is a request
/// and not a guarantee.
const LIVE_PODS: &str = "status.phase!=Succeeded,status.phase!=Failed";

/// Ask the API server for every pod that is still occupying a node.
///
/// The only function in this module that touches the network.
pub async fn fetch(client: Client) -> Result<Vec<Pod>, kube::Error> {
    let api: Api<Pod> = Api::all(client);
    let params = ListParams::default().fields(LIVE_PODS);
    Ok(api.list(&params).await?.items)
}

/// Which pods a listing is about.
///
/// The two cases are not interchangeable at the API level — one namespace is a
/// different endpoint from all of them — and they are not interchangeable to a
/// reader either, since only the cluster-wide listing needs a `NAMESPACE`
/// column. Carrying the choice as a value keeps the fetch and the rendering
/// from disagreeing about which listing this is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// One namespace, named.
    Namespace(String),
    /// Every namespace the user is allowed to see.
    All,
}

impl Scope {
    /// Whether a listing of this scope needs a `NAMESPACE` column.
    ///
    /// Repeating one namespace down every row of a namespaced listing is noise;
    /// leaving it out of a cluster-wide one loses the only thing that
    /// disambiguates two pods with the same name.
    #[must_use]
    pub fn needs_namespace_column(&self) -> bool {
        matches!(self, Self::All)
    }
}

/// Ask the API server for the pods in `scope`, finished ones included.
///
/// Deliberately unlike [`fetch`]: that one exists to total what is *booked* on
/// a node, so it filters the terminal phases out server-side. A person reading
/// `eks pods` wants to see the `Completed` Job that ran an hour ago and the
/// `Evicted` pod that explains their morning, so nothing is filtered here.
pub async fn fetch_scope(
    client: Client,
    scope: &Scope,
    selectors: &Selectors,
) -> Result<Vec<Pod>, kube::Error> {
    let api: Api<Pod> = match scope {
        Scope::Namespace(name) => Api::namespaced(client, name),
        Scope::All => Api::all(client),
    };

    Ok(api.list(&selectors.to_params()).await?.items)
}

/// Server-side selectors for a pod listing, already validated.
///
/// Carried as canonical selector strings rather than raw input so the only
/// thing that reaches the API server is a selector [`crate::k8s::selector`] has
/// vouched for. Both are `None` by default, which lists everything in scope.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selectors {
    /// A label selector, e.g. `app=api,tier notin (canary)`.
    pub label: Option<String>,
    /// A field selector, e.g. `status.phase!=Running`.
    pub field: Option<String>,
}

impl Selectors {
    /// Turn the selectors into the list parameters `kube` sends.
    ///
    /// A `None` leaves that selector unset — an empty string would be sent as
    /// an empty selector, which is not quite the same thing to every server —
    /// so the two are kept distinct all the way to the wire.
    #[must_use]
    pub fn to_params(&self) -> ListParams {
        let mut params = ListParams::default();
        if let Some(label) = &self.label {
            params = params.labels(label);
        }
        if let Some(field) = &self.field {
            params = params.fields(field);
        }
        params
    }
}

/// CPU and memory asked for, as a pair, so the two are never added up in
/// different orders in different places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Requests {
    pub cpu: Quantity,
    pub memory: Quantity,
}

impl Requests {
    /// Componentwise sum.
    #[must_use]
    pub fn plus(self, other: Self) -> Self {
        Self {
            cpu: self.cpu + other.cpu,
            memory: self.memory + other.memory,
        }
    }

    /// Componentwise maximum.
    ///
    /// Per-resource rather than picking the single "largest" container, which
    /// is what the scheduler does: a pod with a memory-hungry init container
    /// and a CPU-hungry one needs the peak of each, not the peak of whichever
    /// happened to look bigger.
    #[must_use]
    pub fn max(self, other: Self) -> Self {
        Self {
            cpu: self.cpu.max(other.cpu),
            memory: self.memory.max(other.memory),
        }
    }

    /// Read `cpu` and `memory` out of a resource map, treating an absent or
    /// unreadable entry as nothing asked for.
    ///
    /// A container with no requests really has asked for nothing — the
    /// scheduler places it anywhere — so zero is the honest answer here rather
    /// than a missing value.
    fn read(map: Option<&BTreeMap<String, ApiQuantity>>) -> Self {
        Self {
            cpu: Quantity::lookup(map, "cpu").unwrap_or_default(),
            memory: Quantity::lookup(map, "memory").unwrap_or_default(),
        }
    }
}

/// What one pod has booked on the node it landed on.
///
/// `max(sum of containers and sidecars, peak init container)` plus pod
/// overhead. See the module docs for why each term is there.
#[must_use]
pub fn effective_requests(pod: &Pod) -> Requests {
    let Some(spec) = pod.spec.as_ref() else {
        return Requests::default();
    };

    // Init containers run in order, one at a time, so the init phase's peak is
    // whichever single one is largest — plus the sidecars started before it,
    // which are still running while it does.
    let mut sidecars = Requests::default();
    let mut init_peak = Requests::default();
    for container in spec.init_containers.iter().flatten() {
        let requests = container_requests(container);
        init_peak = init_peak.max(sidecars.plus(requests));
        if is_sidecar(container) {
            sidecars = sidecars.plus(requests);
        }
    }

    // Sidecars are still up once the app containers start, so they are part of
    // the steady-state sum too — starting the fold from them rather than zero.
    let running = spec.containers.iter().fold(sidecars, |total, container| {
        total.plus(container_requests(container))
    });

    running.max(init_peak).plus(overhead(spec))
}

/// An init container that never exits, so it is charged like an app container.
///
/// `restartPolicy: Always` on an init container is the only thing that makes it
/// a sidecar; the field is meaningless on an app container, which is why this
/// is only ever asked about init containers.
fn is_sidecar(container: &Container) -> bool {
    container.restart_policy.as_deref() == Some("Always")
}

fn container_requests(container: &Container) -> Requests {
    // Kubernetes defaults a missing request to the matching limit when it
    // admits the pod, so what the API server hands back already accounts for a
    // pod that only set limits.
    Requests::read(
        container
            .resources
            .as_ref()
            .and_then(|resources| resources.requests.as_ref()),
    )
}

/// What the runtime sandbox itself costs, when a `RuntimeClass` declares it.
fn overhead(spec: &PodSpec) -> Requests {
    Requests::read(spec.overhead.as_ref())
}

/// Total the effective requests of every pod, keyed by the node it is on.
///
/// Pods that have finished, and pods the scheduler has not placed yet, are
/// left out: neither is holding anything on a node. A node with no pods is
/// simply absent from the map — the caller knows its own node list and can tell
/// "nothing running here" from "no such node" better than this function can.
#[must_use]
pub fn by_node(pods: &[Pod]) -> BTreeMap<String, Requests> {
    let mut totals: BTreeMap<String, Requests> = BTreeMap::new();

    for pod in pods {
        let Some(node) = occupied_node(pod) else {
            continue;
        };
        let total = totals.entry(node.to_owned()).or_default();
        *total = total.plus(effective_requests(pod));
    }

    totals
}

/// The node a pod is currently occupying, if it is occupying one.
fn occupied_node(pod: &Pod) -> Option<&str> {
    // A pod being deleted is still on its node and still holding its requests
    // until the kubelet confirms it is gone, so `Terminating` deliberately
    // counts. Only the two terminal phases do not.
    let phase = pod
        .status
        .as_ref()
        .and_then(|status| status.phase.as_deref());
    if matches!(phase, Some("Succeeded" | "Failed")) {
        return None;
    }

    pod.spec
        .as_ref()?
        .node_name
        .as_deref()
        .filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use k8s_openapi::api::core::v1::{PodStatus, ResourceRequirements};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    use super::*;

    fn quantity(text: &str) -> Quantity {
        Quantity::parse(text).unwrap()
    }

    fn requests(cpu: &str, memory: &str) -> Requests {
        Requests {
            cpu: quantity(cpu),
            memory: quantity(memory),
        }
    }

    fn resources(cpu: &str, memory: &str) -> BTreeMap<String, ApiQuantity> {
        [("cpu", cpu), ("memory", memory)]
            .into_iter()
            .map(|(name, value)| (name.to_owned(), ApiQuantity(value.to_owned())))
            .collect()
    }

    /// A container asking for `cpu` and `memory`.
    fn container(name: &str, cpu: &str, memory: &str) -> Container {
        Container {
            name: name.to_owned(),
            resources: Some(ResourceRequirements {
                requests: Some(resources(cpu, memory)),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn sidecar(name: &str, cpu: &str, memory: &str) -> Container {
        Container {
            restart_policy: Some("Always".to_owned()),
            ..container(name, cpu, memory)
        }
    }

    /// A scheduled, running pod with the given spec.
    fn pod(node: &str, spec: PodSpec) -> Pod {
        Pod {
            metadata: ObjectMeta {
                name: Some("api-7c9f".to_owned()),
                namespace: Some("payments".to_owned()),
                ..Default::default()
            },
            spec: Some(PodSpec {
                node_name: Some(node.to_owned()),
                ..spec
            }),
            status: Some(PodStatus {
                phase: Some("Running".to_owned()),
                ..Default::default()
            }),
        }
    }

    fn spec(containers: Vec<Container>) -> PodSpec {
        PodSpec {
            containers,
            ..Default::default()
        }
    }

    #[test]
    fn a_pods_requests_are_the_sum_of_its_containers() {
        let pod = pod(
            "node-a",
            spec(vec![
                container("app", "250m", "512Mi"),
                container("log-shipper", "50m", "64Mi"),
            ]),
        );

        assert_eq!(effective_requests(&pod), requests("300m", "576Mi"));
    }

    #[test]
    fn a_container_asking_for_nothing_contributes_nothing() {
        let pod = pod(
            "node-a",
            spec(vec![Container {
                name: "no-requests".to_owned(),
                ..Default::default()
            }]),
        );

        assert_eq!(effective_requests(&pod), Requests::default());
    }

    #[test]
    fn only_one_half_of_a_containers_request_may_be_set() {
        // Asking for CPU and leaving memory to the LimitRange is common; the
        // missing half is zero, not a reason to skip the container.
        let mut only_cpu = container("app", "250m", "512Mi");
        only_cpu.resources = Some(ResourceRequirements {
            requests: Some(
                [("cpu".to_owned(), ApiQuantity("250m".to_owned()))]
                    .into_iter()
                    .collect(),
            ),
            ..Default::default()
        });

        let pod = pod("node-a", spec(vec![only_cpu]));
        assert_eq!(effective_requests(&pod), requests("250m", "0"));
    }

    #[test]
    fn init_containers_count_as_the_largest_one_not_as_a_sum() {
        // They run one at a time and then exit, so two 1-core init containers
        // never need two cores at once.
        let pod = pod(
            "node-a",
            PodSpec {
                init_containers: Some(vec![
                    container("migrate", "1", "256Mi"),
                    container("seed", "1", "128Mi"),
                ]),
                ..spec(vec![container("app", "250m", "512Mi")])
            },
        );

        // max(app 250m/512Mi, peak init 1/256Mi) — componentwise.
        assert_eq!(effective_requests(&pod), requests("1", "512Mi"));
    }

    #[test]
    fn the_init_peak_is_taken_per_resource_not_per_container() {
        // One init container is CPU-hungry and the other memory-hungry; the pod
        // needs the peak of each, which is what the scheduler reserves.
        let pod = pod(
            "node-a",
            PodSpec {
                init_containers: Some(vec![
                    container("cpu-heavy", "2", "64Mi"),
                    container("memory-heavy", "100m", "4Gi"),
                ]),
                ..spec(vec![container("app", "100m", "128Mi")])
            },
        );

        assert_eq!(effective_requests(&pod), requests("2", "4Gi"));
    }

    #[test]
    fn a_sidecar_is_added_to_the_running_containers_rather_than_maxed() {
        // `restartPolicy: Always` means it never exits, so it is charged
        // alongside the app container for the whole life of the pod.
        let pod = pod(
            "node-a",
            PodSpec {
                init_containers: Some(vec![sidecar("proxy", "100m", "128Mi")]),
                ..spec(vec![container("app", "250m", "512Mi")])
            },
        );

        assert_eq!(effective_requests(&pod), requests("350m", "640Mi"));
    }

    #[test]
    fn an_init_container_after_a_sidecar_is_charged_alongside_it() {
        // The sidecar is already running while the later init container works,
        // so the init-phase peak includes both — and here that beats the
        // steady-state sum.
        let pod = pod(
            "node-a",
            PodSpec {
                init_containers: Some(vec![
                    sidecar("proxy", "100m", "128Mi"),
                    container("migrate", "2", "1Gi"),
                ]),
                ..spec(vec![container("app", "250m", "512Mi")])
            },
        );

        // init peak: proxy 100m/128Mi + migrate 2/1Gi = 2100m/1152Mi
        // steady state: proxy + app = 350m/640Mi
        assert_eq!(effective_requests(&pod), requests("2100m", "1152Mi"));
    }

    #[test]
    fn an_init_container_before_a_sidecar_is_not_charged_alongside_it() {
        // Ordering matters: this init container has finished before the sidecar
        // is ever started, so the two never overlap.
        let pod = pod(
            "node-a",
            PodSpec {
                init_containers: Some(vec![
                    container("migrate", "2", "1Gi"),
                    sidecar("proxy", "100m", "128Mi"),
                ]),
                ..spec(vec![container("app", "250m", "512Mi")])
            },
        );

        // init peak: max(migrate 2/1Gi, proxy 100m/128Mi) = 2/1Gi
        assert_eq!(effective_requests(&pod), requests("2", "1Gi"));
    }

    #[test]
    fn pod_overhead_is_charged_on_top_of_everything_else() {
        // A RuntimeClass with a sandbox — Firecracker, gVisor — declares what
        // the sandbox itself costs, and the scheduler reserves it too.
        let pod = pod(
            "node-a",
            PodSpec {
                overhead: Some(resources("50m", "64Mi")),
                init_containers: Some(vec![container("migrate", "1", "128Mi")]),
                ..spec(vec![container("app", "250m", "512Mi")])
            },
        );

        // max(250m/512Mi, 1/128Mi) + 50m/64Mi
        assert_eq!(effective_requests(&pod), requests("1050m", "576Mi"));
    }

    #[test]
    fn a_pod_with_no_spec_at_all_asks_for_nothing() {
        assert_eq!(effective_requests(&Pod::default()), Requests::default());
    }

    #[test]
    fn totals_are_grouped_by_the_node_a_pod_landed_on() {
        let pods = [
            pod("node-a", spec(vec![container("app", "250m", "512Mi")])),
            pod("node-a", spec(vec![container("app", "1", "1Gi")])),
            pod("node-b", spec(vec![container("app", "100m", "64Mi")])),
        ];

        let totals = by_node(&pods);

        assert_eq!(totals.len(), 2);
        assert_eq!(totals["node-a"], requests("1250m", "1536Mi"));
        assert_eq!(totals["node-b"], requests("100m", "64Mi"));
    }

    #[test]
    fn a_pod_that_has_finished_holds_nothing_on_its_node() {
        // A cluster full of completed Jobs must not read as a full cluster.
        for phase in ["Succeeded", "Failed"] {
            let mut finished = pod("node-a", spec(vec![container("app", "1", "1Gi")]));
            finished.status = Some(PodStatus {
                phase: Some(phase.to_owned()),
                ..Default::default()
            });

            assert!(by_node(&[finished]).is_empty(), "phase {phase}");
        }
    }

    #[test]
    fn a_pod_being_deleted_still_holds_its_requests() {
        // Terminating pods keep their place until the kubelet confirms they are
        // gone; pretending otherwise makes a draining node look emptier than it
        // is.
        let mut terminating = pod("node-a", spec(vec![container("app", "1", "1Gi")]));
        terminating.metadata.deletion_timestamp =
            Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                "2026-08-18T12:00:00Z".parse().unwrap(),
            ));

        assert_eq!(by_node(&[terminating])["node-a"], requests("1", "1Gi"));
    }

    #[test]
    fn an_unscheduled_pod_is_not_charged_to_any_node() {
        // A Pending pod nothing will fit is exactly the case these numbers are
        // meant to explain; counting it against a node would hide the problem.
        let mut pending = pod("", spec(vec![container("app", "1", "1Gi")]));
        pending.status = Some(PodStatus {
            phase: Some("Pending".to_owned()),
            ..Default::default()
        });
        if let Some(spec) = pending.spec.as_mut() {
            spec.node_name = None;
        }

        assert!(by_node(&[pending]).is_empty());

        // An empty string reads the same as an absent one.
        let blank = pod("", spec(vec![container("app", "1", "1Gi")]));
        assert!(by_node(&[blank]).is_empty());
    }

    #[test]
    fn no_pods_at_all_is_an_empty_map_rather_than_an_error() {
        assert!(by_node(&[]).is_empty());
    }

    #[test]
    fn only_a_cluster_wide_scope_needs_a_namespace_column() {
        assert!(Scope::All.needs_namespace_column());
        assert!(!Scope::Namespace("payments".to_owned()).needs_namespace_column());
    }
}
