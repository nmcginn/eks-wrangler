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
//! [`fetch`] and [`fetch_scope`] are the only functions here that touch the
//! network, and both read their listing in pages — see [`crate::k8s::page`].

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{Container, Pod, PodSpec};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity as ApiQuantity;
use kube::Client;
use kube::api::{Api, ListParams};

use crate::k8s::page;
use crate::k8s::quantity::Quantity;
use crate::k8s::resource;

pub mod order;
pub mod row;

pub use order::{Missing, Order, cause, ranks_any, sort};
pub use row::{PodRow, render, shows_usage, usage_unavailable, usage_unsampled};

/// Pods that have finished linger in the API server until something collects
/// them, and they hold nothing on the node. Excluding them server-side keeps a
/// cluster full of completed Jobs from being paid for over the wire; the same
/// rule is applied again in [`by_node`], because a field selector is a request
/// and not a guarantee.
const LIVE_PODS: &str = "status.phase!=Succeeded,status.phase!=Failed";

/// Ask the API server for every pod that is still occupying a node.
///
/// One of two functions here that touch the network, and the bigger of the two
/// on any real cluster: every pod on every node, to total what each has booked.
/// It is read in pages — see [`crate::k8s::page`] — with `budget` limiting how
/// long each page may take.
pub async fn fetch(client: Client, budget: page::Budget) -> Result<Vec<Pod>, page::Error> {
    let api: Api<Pod> = Api::all(client);
    let params = ListParams::default().fields(LIVE_PODS);
    page::collect(&api, &params, budget).await
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
    budget: page::Budget,
) -> Result<Vec<Pod>, page::Error> {
    let api: Api<Pod> = match scope {
        Scope::Namespace(name) => Api::namespaced(client, name),
        Scope::All => Api::all(client),
    };

    page::collect(&api, &selectors.to_params(), budget).await
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

/// What a pod or a container asked for, as one value, so the resources are
/// never added up in different orders in different places.
///
/// `cpu` and `memory` are fields because every caller wants them and every
/// container may have them. Everything else a manifest can ask for — a GPU, a
/// dongle, a licence count — arrives under a name the cluster invented, so it
/// lives in a map keyed by that name. A resource absent from the map was not
/// asked for, which is a real zero rather than an unknown: the scheduler
/// reserves nothing for a request nobody made.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Requests {
    pub cpu: Quantity,
    pub memory: Quantity,
    /// Extended resources, keyed by their fully-qualified name — see
    /// [`crate::k8s::resource::is_extended`] for what counts as one.
    pub extended: BTreeMap<String, Quantity>,
}

impl Requests {
    /// Componentwise sum, over the union of the extended resources asked for.
    #[must_use]
    pub fn plus(mut self, other: Self) -> Self {
        self.cpu = self.cpu + other.cpu;
        self.memory = self.memory + other.memory;
        for (name, amount) in other.extended {
            self.combine(name, amount, std::ops::Add::add);
        }
        self
    }

    /// Componentwise maximum.
    ///
    /// Per-resource rather than picking the single "largest" container, which
    /// is what the scheduler does: a pod with a memory-hungry init container
    /// and a CPU-hungry one needs the peak of each, not the peak of whichever
    /// happened to look bigger. Extended resources take part on the same terms,
    /// so a GPU asked for by one init container and not the next does not
    /// disappear from the pod's footprint.
    #[must_use]
    pub fn max(mut self, other: Self) -> Self {
        self.cpu = self.cpu.max(other.cpu);
        self.memory = self.memory.max(other.memory);
        for (name, amount) in other.extended {
            self.combine(name, amount, Quantity::max);
        }
        self
    }

    /// Fold one of `other`'s extended resources into ours.
    ///
    /// A name only one side carries needs no special case in either caller: an
    /// absent entry is a zero, and both `+` and `max` leave the other operand
    /// alone when handed one.
    fn combine(
        &mut self,
        name: String,
        amount: Quantity,
        fold: fn(Quantity, Quantity) -> Quantity,
    ) {
        let entry = self.extended.entry(name).or_default();
        *entry = fold(*entry, amount);
    }

    /// How much of one extended resource was asked for, zero if none was.
    #[must_use]
    pub fn extended(&self, resource: &str) -> Quantity {
        self.extended.get(resource).copied().unwrap_or_default()
    }

    /// Read a container's requests out of a resource map, treating an absent or
    /// unreadable entry as nothing asked for.
    ///
    /// A container with no requests really has asked for nothing — the
    /// scheduler places it anywhere — so zero is the honest answer here rather
    /// than a missing value.
    ///
    /// Every extended resource in the map is carried through, rather than a
    /// list of names this tool knows: the point of an extended resource is that
    /// the cluster invented it, so the node's own capacity map is the only
    /// authority on which ones exist, and it is the node table that decides
    /// which of them earn a column.
    fn read(map: Option<&BTreeMap<String, ApiQuantity>>) -> Self {
        Self {
            cpu: Quantity::lookup(map, "cpu").unwrap_or_default(),
            memory: Quantity::lookup(map, "memory").unwrap_or_default(),
            extended: map
                .into_iter()
                .flatten()
                .filter(|(name, _)| resource::is_extended(name))
                .filter_map(|(name, _)| {
                    Quantity::lookup(map, name).map(|amount| (name.clone(), amount))
                })
                .collect(),
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
        init_peak = init_peak.max(sidecars.clone().plus(requests.clone()));
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

/// What the pods on one node add up to.
///
/// Two facts from one walk, deliberately. A node's pod count and its request
/// totals answer the same question — will the next pod fit here — and they are
/// both derived by deciding, pod by pod, which ones are still occupying the
/// node. Counting in a second pass would be a second chance to answer that
/// differently, and a `PODS` cell that disagreed with the `CPU REQ` beside it
/// about how many pods it had added up would be invisible on screen and wrong.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Placed {
    /// How many pods are on the node.
    ///
    /// A `u32` because it is counted rather than measured: `maxPods` is 737 on
    /// the largest instance type EKS offers, and the arithmetic that reaches a
    /// cell divides it by that limit rather than adding it to anything.
    pub pods: u32,
    /// What those pods have booked between them.
    pub requests: Requests,
}

/// Total what every pod has placed on the node it is on.
///
/// Pods that have finished, and pods the scheduler has not placed yet, are
/// left out: neither is holding anything on a node, and neither counts against
/// the node's pod limit. A node with no pods is simply absent from the map —
/// the caller knows its own node list and can tell "nothing running here" from
/// "no such node" better than this function can.
#[must_use]
pub fn by_node(pods: &[Pod]) -> BTreeMap<String, Placed> {
    let mut totals: BTreeMap<String, Placed> = BTreeMap::new();

    for pod in pods {
        let Some(node) = occupied_node(pod) else {
            continue;
        };
        let total = totals.entry(node.to_owned()).or_default();
        // Saturating for the reason `Quantity`'s addition is: a count that pins
        // at the maximum is a visibly absurd number, where a wrapped one is a
        // small plausible lie. Nothing real gets within four billion of it.
        total.pods = total.pods.saturating_add(1);
        // Taken rather than copied: a total carries a map of extended resources
        // now, so it is moved through the sum instead of being cloned into it.
        total.requests = std::mem::take(&mut total.requests).plus(effective_requests(pod));
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
            extended: BTreeMap::new(),
        }
    }

    /// `pods` pods on one node, having booked `cpu` and `memory` between them.
    fn placed(pods: u32, cpu: &str, memory: &str) -> Placed {
        Placed {
            pods,
            requests: requests(cpu, memory),
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
        assert_eq!(totals["node-a"], placed(2, "1250m", "1536Mi"));
        assert_eq!(totals["node-b"], placed(1, "100m", "64Mi"));
    }

    #[test]
    fn the_count_and_the_totals_come_out_of_one_walk_over_the_pods() {
        // The property the `PODS` column depends on: whatever rule decides a
        // pod is occupying a node decides both numbers, so a cell saying 3 and
        // a cell saying what 3 pods booked cannot be about different threes.
        let mut finished = pod("node-a", spec(vec![container("app", "1", "1Gi")]));
        finished.status = Some(PodStatus {
            phase: Some("Succeeded".to_owned()),
            ..Default::default()
        });
        let pods = [
            pod("node-a", spec(vec![container("app", "250m", "512Mi")])),
            finished,
            pod("node-a", spec(vec![container("app", "1", "1Gi")])),
        ];

        // The completed Job counts against neither half.
        assert_eq!(by_node(&pods)["node-a"], placed(2, "1250m", "1536Mi"));
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

        assert_eq!(by_node(&[terminating])["node-a"], placed(1, "1", "1Gi"));
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

    /// A container asking for `count` of one extended resource and nothing else.
    fn device_container(name: &str, resource: &str, count: &str) -> Container {
        Container {
            name: name.to_owned(),
            resources: Some(ResourceRequirements {
                requests: Some(
                    [(resource.to_owned(), ApiQuantity(count.to_owned()))]
                        .into_iter()
                        .collect(),
                ),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn a_pods_devices_are_summed_across_its_containers() {
        let pod = pod(
            "node-a",
            spec(vec![
                device_container("trainer", "nvidia.com/gpu", "1"),
                device_container("sidecar-trainer", "nvidia.com/gpu", "1"),
            ]),
        );

        assert_eq!(
            effective_requests(&pod).extended("nvidia.com/gpu"),
            quantity("2")
        );
    }

    #[test]
    fn a_device_nobody_asked_for_reads_as_zero_rather_than_as_missing() {
        // The scheduler reserves nothing for a request nobody made, so this is
        // a real zero and the node table renders it as one.
        let pod = pod("node-a", spec(vec![container("app", "250m", "512Mi")]));

        assert_eq!(
            effective_requests(&pod).extended("nvidia.com/gpu"),
            Quantity::default()
        );
        assert_eq!(
            Requests::default().extended("nvidia.com/gpu"),
            Quantity::default()
        );
    }

    #[test]
    fn a_device_follows_the_same_init_peak_rule_cpu_does() {
        // An init container holding a GPU while it warms a model cache is the
        // pod's footprint even though no app container asks for one; charging
        // only the app containers would show the node as having a card free
        // that the scheduler has already reserved.
        let pod = pod(
            "node-a",
            PodSpec {
                init_containers: Some(vec![device_container("warm", "nvidia.com/gpu", "1")]),
                ..spec(vec![container("app", "250m", "512Mi")])
            },
        );

        assert_eq!(
            effective_requests(&pod).extended("nvidia.com/gpu"),
            quantity("1")
        );
    }

    #[test]
    fn a_device_asked_for_by_a_sidecar_is_added_to_the_running_containers() {
        // The sum branch rather than the max one, on the same `restartPolicy`
        // rule the CPU total follows.
        let mut proxy = device_container("proxy", "nvidia.com/gpu", "1");
        proxy.restart_policy = Some("Always".to_owned());

        let pod = pod(
            "node-a",
            PodSpec {
                init_containers: Some(vec![proxy]),
                ..spec(vec![device_container("app", "nvidia.com/gpu", "2")])
            },
        );

        assert_eq!(
            effective_requests(&pod).extended("nvidia.com/gpu"),
            quantity("3")
        );
    }

    #[test]
    fn the_resources_kubernetes_defines_itself_stay_out_of_the_extended_map() {
        // `hugepages-2Mi` and `ephemeral-storage` are native resources with a
        // native meaning; treating them as devices would put a column headed
        // `HUGEPAGES-2MI` on a table that has no idea what one is.
        let mut app = container("app", "250m", "512Mi");
        app.resources = Some(ResourceRequirements {
            requests: Some(
                [
                    ("cpu", "250m"),
                    ("hugepages-2Mi", "128Mi"),
                    ("ephemeral-storage", "1Gi"),
                    ("kubernetes.io/something", "1"),
                ]
                .into_iter()
                .map(|(name, value)| (name.to_owned(), ApiQuantity(value.to_owned())))
                .collect(),
            ),
            ..Default::default()
        });

        let requests = effective_requests(&pod("node-a", spec(vec![app])));

        assert!(requests.extended.is_empty(), "{:?}", requests.extended);
        assert_eq!(requests.cpu, quantity("250m"));
    }

    #[test]
    fn node_totals_add_up_the_devices_of_every_pod_on_the_node() {
        let pods = [
            pod(
                "node-a",
                spec(vec![device_container("a", "nvidia.com/gpu", "1")]),
            ),
            pod(
                "node-a",
                spec(vec![device_container("b", "nvidia.com/gpu", "2")]),
            ),
            pod(
                "node-b",
                spec(vec![device_container("c", "amd.com/gpu", "1")]),
            ),
        ];

        let totals = by_node(&pods);

        assert_eq!(
            totals["node-a"].requests.extended("nvidia.com/gpu"),
            quantity("3")
        );
        // A device booked on one node must not turn up on another.
        assert_eq!(
            totals["node-a"].requests.extended("amd.com/gpu"),
            Quantity::default()
        );
        assert_eq!(
            totals["node-b"].requests.extended("amd.com/gpu"),
            quantity("1")
        );
    }

    #[test]
    fn two_different_devices_both_survive_the_sum_and_the_peak() {
        // The union, in both folds: a resource only one side of `plus` or `max`
        // carries must not be dropped by the one that does not know about it.
        let pod = pod(
            "node-a",
            PodSpec {
                init_containers: Some(vec![device_container("warm", "amd.com/gpu", "1")]),
                ..spec(vec![device_container("app", "nvidia.com/gpu", "2")])
            },
        );

        let requests = effective_requests(&pod);
        assert_eq!(requests.extended("amd.com/gpu"), quantity("1"));
        assert_eq!(requests.extended("nvidia.com/gpu"), quantity("2"));
    }

    #[test]
    fn a_finished_pod_releases_the_devices_it_held() {
        // The rule the CPU total already follows, asserted for devices too: a
        // completed training Job must not make a GPU node look full.
        let mut finished = pod(
            "node-a",
            spec(vec![device_container("trainer", "nvidia.com/gpu", "4")]),
        );
        finished.status = Some(PodStatus {
            phase: Some("Succeeded".to_owned()),
            ..Default::default()
        });

        assert!(by_node(&[finished]).is_empty());
    }

    #[test]
    fn only_a_cluster_wide_scope_needs_a_namespace_column() {
        assert!(Scope::All.needs_namespace_column());
        assert!(!Scope::Namespace("payments".to_owned()).needs_namespace_column());
    }
}
