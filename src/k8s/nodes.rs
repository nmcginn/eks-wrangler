//! Nodes: fetching them, and turning them into rows.
//!
//! The split here is the one the whole project is built around. [`fetch`] does
//! nothing but I/O. [`NodeRow::from_node`] does nothing but computation, over a
//! `Node` value and an explicit `now`, which is why every awkward case below —
//! a node with no conditions at all, a cordoned node, a kubelet version the API
//! server never filled in — is a test rather than a cluster you have to break
//! on purpose.

use k8s_openapi::api::core::v1::Node;
use k8s_openapi::jiff::Timestamp;
use kube::Client;
use kube::api::{Api, ListParams};

use crate::format;
use crate::k8s::quantity::{self, Quantity};
use crate::theme::Severity;

/// Ask the API server for every node in the cluster.
///
/// The only function in this module that touches the network.
pub async fn fetch(client: Client) -> Result<Vec<Node>, kube::Error> {
    let api: Api<Node> = Api::all(client);
    Ok(api.list(&ListParams::default()).await?.items)
}

/// One node, reduced to what a person wants to see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRow {
    pub name: String,
    /// `kubectl`'s wording: `Ready`, `NotReady`, `Unknown`, each optionally
    /// suffixed with `,SchedulingDisabled`.
    pub status: String,
    /// How alarming that status is. Carried here rather than derived at the
    /// call site so the CLI table and the dashboard cannot disagree about it.
    pub severity: Severity,
    pub version: String,
    /// Cores the node has, and cores pods may actually ask for.
    pub cpu: Capacity,
    /// Bytes of memory the node has, and bytes pods may actually ask for.
    pub memory: Capacity,
    pub age: String,
}

/// What a node has of one resource, and how much of it is left for pods.
///
/// The two are not the same number and the gap is not small: the kubelet
/// reserves memory and CPU for itself, for the OS, and for eviction headroom,
/// which on a 2 GiB EKS node can be a quarter of the machine. Showing capacity
/// alone invites "why will nothing schedule on a node with free memory?", so
/// both are shown.
///
/// Either half is `None` when the node has not reported it — a node still
/// registering has no `status` at all — and the renderer says so rather than
/// inventing a zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capacity {
    pub allocatable: Option<Quantity>,
    pub capacity: Option<Quantity>,
}

impl Capacity {
    /// Read one resource out of a node's `capacity` and `allocatable` maps.
    #[must_use]
    pub fn read(node: &Node, resource: &str) -> Self {
        let status = node.status.as_ref();
        Self {
            allocatable: Quantity::lookup(status.and_then(|s| s.allocatable.as_ref()), resource),
            capacity: Quantity::lookup(status.and_then(|s| s.capacity.as_ref()), resource),
        }
    }

    /// The fraction of capacity the kubelet has *not* reserved, if both halves
    /// are known. Not shown yet; the dashboard will want it.
    #[must_use]
    pub fn allocatable_ratio(self) -> Option<f64> {
        self.allocatable?.ratio_of(self.capacity?)
    }

    /// One table cell: `allocatable/capacity`, formatted by `show`.
    ///
    /// A node reporting only one of the two prints just that number. It is
    /// ambiguous, but it happens only mid-registration and only for a second,
    /// and a cell that says `-/4` reads like a bug.
    fn cell(self, show: fn(Quantity) -> String) -> String {
        match (self.allocatable, self.capacity) {
            (Some(allocatable), Some(capacity)) => {
                format!("{}/{}", show(allocatable), show(capacity))
            }
            (Some(only), None) | (None, Some(only)) => show(only),
            (None, None) => UNKNOWN.to_owned(),
        }
    }
}

/// Shown wherever the API server left a field empty. Matches the placeholder
/// `eks contexts` uses for an unknown region.
const UNKNOWN: &str = "-";

impl NodeRow {
    /// Build a row from a `Node`, as of `now`.
    ///
    /// `now` is a parameter rather than a call to the clock so the age column
    /// is testable and so every row in one listing shares a single instant.
    #[must_use]
    pub fn from_node(node: &Node, now: Timestamp) -> Self {
        let name = node
            .metadata
            .name
            .clone()
            .unwrap_or_else(|| UNKNOWN.to_owned());

        let ready = ready_condition(node);
        let cordoned = node.spec.as_ref().and_then(|spec| spec.unschedulable) == Some(true);

        Self {
            name,
            status: status_text(ready, cordoned),
            severity: severity(ready, cordoned),
            cpu: Capacity::read(node, "cpu"),
            memory: Capacity::read(node, "memory"),
            version: node
                .status
                .as_ref()
                .and_then(|status| status.node_info.as_ref())
                .map_or_else(|| UNKNOWN.to_owned(), |info| info.kubelet_version.clone()),
            age: node.metadata.creation_timestamp.as_ref().map_or_else(
                || UNKNOWN.to_owned(),
                |created| format::human_duration(now.duration_since(created.0)),
            ),
        }
    }
}

/// The `Ready` condition's status, if the node reports one at all.
///
/// `Some(true)` is a healthy node, `Some(false)` covers both `False` and
/// `Unknown` — which is what a node whose kubelet stopped reporting looks like,
/// and `kubectl` calls both `NotReady`. `None` means the condition is absent,
/// usually a node that has only just registered.
fn ready_condition(node: &Node) -> Option<bool> {
    node.status
        .as_ref()?
        .conditions
        .as_ref()?
        .iter()
        .find(|condition| condition.type_ == "Ready")
        .map(|condition| condition.status == "True")
}

fn status_text(ready: Option<bool>, cordoned: bool) -> String {
    let base = match ready {
        Some(true) => "Ready",
        Some(false) => "NotReady",
        None => "Unknown",
    };

    if cordoned {
        format!("{base},SchedulingDisabled")
    } else {
        base.to_owned()
    }
}

fn severity(ready: Option<bool>, cordoned: bool) -> Severity {
    match (ready, cordoned) {
        // A cordoned node is working but deliberately out of service — worth
        // noticing during an incident, not worth the alarm colour.
        (Some(true), true) => Severity::Warn,
        (Some(true), false) => Severity::Ok,
        (Some(false), _) => Severity::Critical,
        (None, _) => Severity::Unknown,
    }
}

/// Render the `eks nodes` table.
///
/// `cluster` is the human label used in the empty-list message, so a user who
/// typed the wrong `--context` finds out from the answer.
#[must_use]
pub fn render(rows: &[NodeRow], cluster: &str) -> String {
    if rows.is_empty() {
        return format!(
            "{cluster} reports no nodes.\n\
             If you expected some, check that its node groups are scaled above zero."
        );
    }

    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            vec![
                row.name.clone(),
                row.status.clone(),
                row.version.clone(),
                row.cpu.cell(quantity::cpu),
                row.memory.cell(quantity::memory),
                row.age.clone(),
            ]
        })
        .collect();

    // AGE stays last because that is where every `kubectl get` puts it, and the
    // new columns are the ones people will be scanning for.
    format::table(
        &["NAME", "STATUS", "VERSION", "CPU", "MEMORY", "AGE"],
        &cells,
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::BTreeMap;

    use k8s_openapi::api::core::v1::{NodeCondition, NodeSpec, NodeStatus, NodeSystemInfo};
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity as ApiQuantity;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
    use k8s_openapi::jiff::SignedDuration;

    use super::*;

    fn now() -> Timestamp {
        "2026-08-17T12:00:00Z".parse().unwrap()
    }

    fn ago(hours: i64) -> Time {
        Time(now() - SignedDuration::from_hours(hours))
    }

    /// A plausible, healthy EKS node. Tests mutate one field at a time from
    /// here so it is obvious what each is actually about.
    fn healthy_node() -> Node {
        Node {
            metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                name: Some("ip-10-0-1-9.ec2.internal".to_owned()),
                creation_timestamp: Some(ago(50)),
                ..Default::default()
            },
            spec: Some(NodeSpec::default()),
            status: Some(NodeStatus {
                conditions: Some(vec![condition("Ready", "True")]),
                // What an m5.xlarge actually reports.
                capacity: Some(quantities(&[("cpu", "4"), ("memory", "16374624Ki")])),
                allocatable: Some(quantities(&[("cpu", "3920m"), ("memory", "15525152Ki")])),
                node_info: Some(NodeSystemInfo {
                    kubelet_version: "v1.33.1-eks-1a2b3c4".to_owned(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        }
    }

    fn quantities(pairs: &[(&str, &str)]) -> BTreeMap<String, ApiQuantity> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), ApiQuantity((*value).to_owned())))
            .collect()
    }

    fn condition(kind: &str, status: &str) -> NodeCondition {
        NodeCondition {
            type_: kind.to_owned(),
            status: status.to_owned(),
            ..Default::default()
        }
    }

    fn with_status(node: &Node, conditions: Vec<NodeCondition>) -> Node {
        let mut node = node.clone();
        if let Some(status) = node.status.as_mut() {
            status.conditions = Some(conditions);
        }
        node
    }

    #[test]
    fn a_healthy_node_reads_as_ready() {
        let row = NodeRow::from_node(&healthy_node(), now());

        assert_eq!(row.name, "ip-10-0-1-9.ec2.internal");
        assert_eq!(row.status, "Ready");
        assert_eq!(row.severity, Severity::Ok);
        assert_eq!(row.version, "v1.33.1-eks-1a2b3c4");
        assert_eq!(row.age, "2d2h");
    }

    #[test]
    fn capacity_and_allocatable_are_read_as_separate_numbers() {
        let row = NodeRow::from_node(&healthy_node(), now());

        assert_eq!(row.cpu.capacity, Some(Quantity::parse("4").unwrap()));
        assert_eq!(row.cpu.allocatable, Some(Quantity::parse("3920m").unwrap()));
        assert_eq!(
            row.memory.capacity.map(Quantity::units),
            Some(16_767_614_976)
        );
        assert_eq!(row.cpu.cell(quantity::cpu), "3920m/4");
        assert_eq!(row.memory.cell(quantity::memory), "14.8Gi/15.6Gi");
    }

    #[test]
    fn the_reserved_slice_of_a_node_is_available_as_a_ratio() {
        // Not rendered yet, but it is what the dashboard's bars will divide by,
        // so it is worth pinning down now.
        let row = NodeRow::from_node(&healthy_node(), now());
        let ratio = row.cpu.allocatable_ratio().unwrap();

        assert!((ratio - 0.98).abs() < 1e-9, "{ratio}");
        assert_eq!(Capacity::default().allocatable_ratio(), None);
    }

    #[test]
    fn a_node_that_has_not_reported_capacity_shows_a_placeholder() {
        let mut node = healthy_node();
        if let Some(status) = node.status.as_mut() {
            status.capacity = None;
            status.allocatable = None;
        }

        let row = NodeRow::from_node(&node, now());
        assert_eq!(row.cpu.cell(quantity::cpu), "-");
        assert_eq!(row.memory.cell(quantity::memory), "-");
    }

    #[test]
    fn a_node_reporting_only_one_half_shows_the_half_it_has() {
        let mut node = healthy_node();
        if let Some(status) = node.status.as_mut() {
            status.allocatable = None;
        }

        let row = NodeRow::from_node(&node, now());
        assert_eq!(row.cpu.cell(quantity::cpu), "4");
        assert_eq!(row.memory.cell(quantity::memory), "15.6Gi");
    }

    #[test]
    fn a_capacity_we_cannot_parse_does_not_take_out_the_listing() {
        // An extended resource from a broken device plugin should cost the user
        // one cell, not the whole table.
        let mut node = healthy_node();
        if let Some(status) = node.status.as_mut() {
            status.capacity = Some(quantities(&[("cpu", "four"), ("memory", "16374624Ki")]));
        }

        let row = NodeRow::from_node(&node, now());
        assert_eq!(row.cpu.capacity, None);
        // Only the unreadable half is lost.
        assert_eq!(row.cpu.cell(quantity::cpu), "3920m");
        assert_eq!(row.memory.cell(quantity::memory), "14.8Gi/15.6Gi");
    }

    #[test]
    fn a_node_whose_kubelet_stopped_reporting_is_not_ready() {
        // The API server flips Ready to Unknown after the node lease expires;
        // kubectl still calls that NotReady, and so do we.
        for status in ["False", "Unknown"] {
            let node = with_status(&healthy_node(), vec![condition("Ready", status)]);
            let row = NodeRow::from_node(&node, now());

            assert_eq!(row.status, "NotReady", "Ready={status}");
            assert_eq!(row.severity, Severity::Critical, "Ready={status}");
        }
    }

    #[test]
    fn a_node_with_no_ready_condition_is_unknown_rather_than_broken() {
        let node = with_status(&healthy_node(), vec![condition("MemoryPressure", "False")]);
        let row = NodeRow::from_node(&node, now());

        assert_eq!(row.status, "Unknown");
        assert_eq!(row.severity, Severity::Unknown);
    }

    #[test]
    fn a_cordoned_node_says_so_after_its_status() {
        let mut node = healthy_node();
        node.spec = Some(NodeSpec {
            unschedulable: Some(true),
            ..Default::default()
        });

        let row = NodeRow::from_node(&node, now());
        assert_eq!(row.status, "Ready,SchedulingDisabled");
        // Deliberately out of service is a warning, not a failure.
        assert_eq!(row.severity, Severity::Warn);
    }

    #[test]
    fn a_cordoned_unhealthy_node_keeps_both_facts() {
        let mut node = with_status(&healthy_node(), vec![condition("Ready", "False")]);
        node.spec = Some(NodeSpec {
            unschedulable: Some(true),
            ..Default::default()
        });

        let row = NodeRow::from_node(&node, now());
        assert_eq!(row.status, "NotReady,SchedulingDisabled");
        assert_eq!(row.severity, Severity::Critical);
    }

    #[test]
    fn a_node_with_nothing_filled_in_still_produces_a_row() {
        // Everything under `status` is optional in the API, and a node caught
        // mid-registration really can arrive like this.
        let row = NodeRow::from_node(&Node::default(), now());

        assert_eq!(row.name, "-");
        assert_eq!(row.status, "Unknown");
        assert_eq!(row.version, "-");
        assert_eq!(row.age, "-");
        assert_eq!(row.severity, Severity::Unknown);
        assert_eq!(row.cpu, Capacity::default());
        assert_eq!(row.memory, Capacity::default());
    }

    #[test]
    fn rendering_lays_the_rows_out_in_columns() {
        let mut second = healthy_node();
        second.metadata.name = Some("ip-10-0-11-200.ec2.internal".to_owned());
        second.metadata.creation_timestamp = Some(ago(1));
        let second = with_status(&second, vec![condition("Ready", "False")]);

        let rows = [
            NodeRow::from_node(&healthy_node(), now()),
            NodeRow::from_node(&second, now()),
        ];

        assert_eq!(
            render(&rows, "prod (us-east-1)"),
            "NAME                         STATUS    VERSION              CPU      MEMORY         AGE\n\
             ip-10-0-1-9.ec2.internal     Ready     v1.33.1-eks-1a2b3c4  3920m/4  14.8Gi/15.6Gi  2d2h\n\
             ip-10-0-11-200.ec2.internal  NotReady  v1.33.1-eks-1a2b3c4  3920m/4  14.8Gi/15.6Gi  60m"
        );
    }

    #[test]
    fn an_empty_cluster_explains_itself_instead_of_printing_a_bare_header() {
        let message = render(&[], "prod (us-east-1)");

        assert!(message.contains("prod (us-east-1)"), "{message}");
        assert!(message.contains("node groups"), "{message}");
        assert!(!message.contains("NAME"), "{message}");
    }
}
