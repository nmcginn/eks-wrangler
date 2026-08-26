//! Nodes: fetching them, and turning them into rows.
//!
//! The split here is the one the whole project is built around. [`fetch`] does
//! nothing but I/O. [`NodeRow::from_node`] does nothing but computation, over a
//! `Node` value and an explicit `now`, which is why every awkward case below —
//! a node with no conditions at all, a cordoned node, a kubelet version the API
//! server never filled in — is a test rather than a cluster you have to break
//! on purpose.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use k8s_openapi::api::core::v1::{Node, NodeSystemInfo};
use k8s_openapi::jiff::Timestamp;
use kube::Client;
use kube::api::{Api, ListParams};

pub mod order;

pub use order::{Missing, Order, cause, distinguishes, ranks_any, sort};

use crate::format;
use crate::k8s::metrics::{self, Sample, Usage};
use crate::k8s::page;
use crate::k8s::pods::{Placed, Requests};
use crate::k8s::quantity::{self, Quantity};
use crate::k8s::resource;
use crate::theme::{Palette, Severity};

/// Ask the API server for every node in the cluster.
///
/// The only function in this module that touches the network. A cluster with
/// more nodes than one response should carry is read in pages — see
/// [`crate::k8s::page`] — and `budget` limits how long each of those pages may
/// take.
pub async fn fetch(client: Client, budget: page::Budget) -> Result<Vec<Node>, page::Error> {
    let api: Api<Node> = Api::all(client);
    page::collect(&api, &ListParams::default(), budget).await
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
    /// Cores the pods already on this node have booked.
    pub cpu_requested: Share,
    /// Memory the pods already on this node have booked.
    pub memory_requested: Share,
    /// Cores the node is actually burning, from metrics-server.
    pub cpu_used: Share,
    /// Memory the node is actually holding, from metrics-server.
    pub memory_used: Share,
    /// How many pods are on the node, against how many it will accept.
    ///
    /// The third reason a pod will not schedule, and the one no amount of
    /// spare CPU fixes: on EKS the limit is usually the number of addresses
    /// the VPC CNI can hand out on that instance type, so a node with two
    /// cores idle and 58 pods on it is full.
    pub pods: Share,
    pub age: String,
    /// When the node joined, if it reported it — what `age` was rendered from.
    ///
    /// Carried beside the rendered string so `--sort age` ranks on an instant
    /// rather than on a rounded, human-readable one: `3d` and `3d` are the same
    /// text and nearly a day apart. The same pairing `PodRow` has.
    pub created_at: Option<Timestamp>,
    /// The node's address inside the VPC, or `-` if it reports none.
    ///
    /// Only shown under `--wide`. This is the address in an ALB target group
    /// and in a security-group rule, and the one an EC2 console search finds
    /// the instance by, none of which the Kubernetes node name will do.
    pub internal_ip: String,
    /// The node's public address, or `-` — which is the healthy answer for a
    /// node in a private subnet, and most EKS nodes are.
    ///
    /// Only shown under `--wide`.
    pub external_ip: String,
    /// The AMI's own description of itself, e.g. `Amazon Linux 2023.9.20260714`.
    ///
    /// Only shown under `--wide`. The one column that says a node group is
    /// running an AMI a release behind the rest of the cluster.
    pub os_image: String,
    /// The kernel the node booted, only shown under `--wide`.
    pub kernel_version: String,
    /// The container runtime and its version, e.g. `containerd://1.7.28`.
    ///
    /// Only shown under `--wide`.
    pub container_runtime: String,
    /// The extended resources this node advertises — GPUs, and anything else a
    /// device plugin or an administrator put on it — keyed by their
    /// fully-qualified name.
    ///
    /// Empty for the overwhelming majority of nodes, and a column each for the
    /// rest. A node that reports none of a resource another node in the listing
    /// does is simply absent from this map rather than carrying a zero: it has
    /// no GPUs, which is a different fact from having zero of them free.
    pub devices: BTreeMap<String, Device>,
    /// The node's ephemeral storage — the disk pods' writable layers, `emptyDir`
    /// volumes, and logs share — read the same way `cpu` and `memory` are.
    ///
    /// Unlike a device, every node reports this; there is simply no request
    /// tracked against it yet, so it gets a capacity pair and no `REQ` column
    /// beside it, the same shape [`cpu`](Self::cpu) had before pod requests were
    /// summed.
    pub ephemeral_storage: Capacity,
    /// Huge-page pools by size, e.g. `hugepages-2Mi`, keyed by their bare name.
    ///
    /// Every node reports every size the kernel supports, at `0` unless an
    /// administrator reserved some — so unlike [`devices`](Self::devices), which
    /// is absent for hardware a node does not have, this map can hold an entry
    /// that is honestly zero. What decides whether a size becomes a column is
    /// the *table's* business, not the row's — see `hugepage_names` below.
    pub hugepages: BTreeMap<String, Capacity>,
}

/// One extended resource on one node: how many of them it will hand out, and
/// how many the pods already there have booked.
///
/// The reason this is not a third [`Share`] is the denominator question a
/// device asks and CPU does not. A node's CPU column shows allocatable over
/// capacity because the gap between them is the kubelet's own reservation, on
/// every node, always. A device's gap is not routine — it is a device the
/// kubelet has and will not schedule onto, usually one its plugin marked
/// unhealthy — so it belongs in a sentence under the table rather than in a
/// pair of numbers a reader has to notice are different.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Device {
    /// What the node has of it, and how many of those it will hand out.
    pub capacity: Capacity,
    /// What the pods already on this node have booked, or `None` when the pods
    /// could not be listed at all.
    ///
    /// Never `None` for a device nobody asked for: that is a real zero, and a
    /// node with four idle GPUs must not read like a node whose pod listing
    /// failed.
    pub booked: Option<Quantity>,
}

impl Device {
    /// What is booked, against what the scheduler will hand out.
    ///
    /// The same shape and the same denominator the CPU and memory columns use,
    /// so a percentage means one thing across a row and the dashboard can take
    /// a bar's width from it without inventing a second rule.
    #[must_use]
    pub fn share(self) -> Share {
        Share {
            amount: self.booked,
            allocatable: self.capacity.allocatable,
        }
    }

    /// `(offered, held)` when the node has more of this device than it will
    /// hand out, and `None` when it offers everything it has.
    ///
    /// The cell shows what can be handed out, because that is the number that
    /// decides whether the next pod fits — which leaves the difference
    /// invisible, and the difference is exactly why a GPU pod that should fit
    /// is sitting `Pending`. See [`devices_withheld`], which says so.
    #[must_use]
    pub fn withheld(self) -> Option<(Quantity, Quantity)> {
        let allocatable = self.capacity.allocatable?;
        let capacity = self.capacity.capacity?;
        (allocatable < capacity).then_some((allocatable, capacity))
    }

    /// One table cell: what is booked, out of what the node will hand out.
    ///
    /// `2/4 (50%)` — [`Share::pair`], for the reason that method gives: for a
    /// countable thing the total is the fact people came for, and "this node
    /// has eight A100s" is not something to work back out of a percentage.
    fn cell(self) -> String {
        self.share().pair(quantity::count)
    }
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

/// One measurement of a resource on a node, against what the node can give out.
///
/// Two different questions share this shape, and the tool answers both in the
/// same table. What the pods on a node have *booked* decides whether the next
/// pod schedules: a node can idle at 20% CPU and still refuse work because its
/// pods requested everything it has. What the node is actually *using* decides
/// whether the workload on it is healthy, and the gap between the two is the
/// whole of capacity planning. Neither number substitutes for the other, so
/// both get a column and one type.
///
/// `amount` is `None` only when the figure could not be obtained: the pod
/// listing was refused, or metrics-server is not installed, or the sampler has
/// not reached this node yet. That is a different thing from a node with
/// nothing on it, which is a real zero, and the two must not render the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Share {
    pub amount: Option<Quantity>,
    pub allocatable: Option<Quantity>,
}

impl Share {
    /// The measured fraction of an explicit denominator, if both halves are
    /// known.
    ///
    /// Can exceed 1.0 either way round. [`ratio`](Self::ratio) always divides
    /// by `allocatable`, which is the right denominator for "will another pod
    /// fit". A caller asking a different question of the same measurement —
    /// the dashboard's utilisation bar asks "is this machine busy", and wants
    /// the node's raw capacity, kubelet reserve included — passes the
    /// denominator it means instead of the one `Share` happens to carry.
    #[must_use]
    pub fn ratio_of(self, denominator: Option<Quantity>) -> Option<f64> {
        self.amount?.ratio_of(denominator?)
    }

    /// The measured fraction of allocatable, if both halves are known.
    ///
    /// Can exceed 1.0 either way round. Allocatable is what the scheduler
    /// honours, and pods placed before a kubelet revised its reservation
    /// downwards really can add up to more than it; usage can pass it too,
    /// because the kubelet's reservation is a promise to the system and not a
    /// cap on the containers. Showing `104%` is the honest answer, and it is
    /// exactly the moment someone wants to know.
    #[must_use]
    pub fn ratio(self) -> Option<f64> {
        self.ratio_of(self.allocatable)
    }

    /// How alarming an explicit denominator's fraction is, on the shared
    /// `theme` thresholds — see [`ratio_of`](Self::ratio_of) for why a caller
    /// would want one.
    #[must_use]
    pub fn severity_of(self, denominator: Option<Quantity>) -> Severity {
        self.ratio_of(denominator)
            .map_or(Severity::Unknown, Severity::from_utilisation)
    }

    /// How alarming that fraction is, on the shared `theme` thresholds, so the
    /// CLI table and the dashboard cannot disagree about what counts as hot.
    #[must_use]
    pub fn severity(self) -> Severity {
        self.severity_of(self.allocatable)
    }

    /// One table cell: the figure, and what share of the node it is.
    ///
    /// A node that has not reported allocatable still shows the absolute
    /// figure — it is the percentage that is unknown, not the measurement.
    fn cell(self, show: fn(Quantity) -> String) -> String {
        let Some(amount) = self.amount else {
            return UNKNOWN.to_owned();
        };

        match self.ratio() {
            // Rounded by `format::percentage`, which the pod table's shares go
            // through too, so the two tables cannot round a percentage
            // differently.
            Some(ratio) => format!("{} ({})", show(amount), format::percentage(ratio)),
            None => show(amount),
        }
    }

    /// The same cell with the denominator spelled out: `12/58 (21%)`.
    ///
    /// [`cell`](Self::cell) is right where the denominator is already on the
    /// row — `CPU REQ` sits beside the `CPU` it is a share of, and printing
    /// `3920m` twice would be noise. The columns that have no such neighbour
    /// use this one instead: a device count and a pod count are each alone in
    /// the table, and for a countable thing the total is half of what the
    /// reader came for. "Four cards, three busy" is not something to work back
    /// out of `3 (75%)`.
    ///
    /// A node that reported no allocatable leaves the figure standing alone —
    /// there is nothing for it to be out of. A `None` amount still prints its
    /// denominator, unlike [`cell`](Self::cell): `-/4` says the node has four
    /// of something and we could not find out how many are spoken for, which
    /// is strictly more than `-` says.
    fn pair(self, show: fn(Quantity) -> String) -> String {
        let amount = self.amount.map_or_else(|| UNKNOWN.to_owned(), show);

        let Some(allocatable) = self.allocatable else {
            return amount;
        };

        let total = show(allocatable);
        match self.ratio() {
            Some(ratio) => format!("{amount}/{total} ({})", format::percentage(ratio)),
            // No ratio means either no figure to divide or a node offering none
            // of what it reports; `-/4` and `0/0` are both better than a
            // percentage of nothing.
            None => format!("{amount}/{total}"),
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
    ///
    /// `placed` is what the pods on this node add up to — `Some(zero)` for a
    /// node running nothing, and `None` only when the pods could not be listed
    /// at all.
    ///
    /// `used` is what metrics-server last sampled for this node. `None` covers
    /// both "there is no metrics-server" and "there is one and it has not
    /// reached this node yet", which read the same in a table and lead to the
    /// same footnote.
    #[must_use]
    pub fn from_node(
        node: &Node,
        placed: Option<&Placed>,
        used: Option<Usage>,
        now: Timestamp,
    ) -> Self {
        let name = node
            .metadata
            .name
            .clone()
            .unwrap_or_else(|| UNKNOWN.to_owned());

        let ready = ready_condition(node);
        let cordoned = node.spec.as_ref().and_then(|spec| spec.unschedulable) == Some(true);
        let cpu = Capacity::read(node, "cpu");
        let memory = Capacity::read(node, "memory");
        let requested = placed.map(|placed| &placed.requests);
        // Read once and cloned into the three cells, so a node still
        // registering — which has no `nodeInfo` at all — cannot end up with
        // some of the three filled in and the rest not.
        let info = node
            .status
            .as_ref()
            .and_then(|status| status.node_info.as_ref());

        Self {
            name,
            status: status_text(ready, cordoned),
            severity: severity(ready, cordoned),
            cpu,
            memory,
            cpu_requested: Share {
                amount: requested.map(|total| total.cpu),
                allocatable: cpu.allocatable,
            },
            memory_requested: Share {
                amount: requested.map(|total| total.memory),
                allocatable: memory.allocatable,
            },
            cpu_used: Share {
                amount: used.and_then(|usage| usage.cpu),
                allocatable: cpu.allocatable,
            },
            memory_used: Share {
                amount: used.and_then(|usage| usage.memory),
                allocatable: memory.allocatable,
            },
            pods: Share {
                // Counted from the pod listing rather than read from the node's
                // own status, which reports no such figure — and unknown, not
                // zero, when that listing failed. A node drawn as empty is an
                // invitation to schedule onto it.
                amount: placed.map(|placed| Quantity::from_count(placed.pods)),
                // `allocatable`, not `capacity`: the kubelet's `--max-pods`, and
                // on EKS the VPC CNI's address budget, both land here, and it is
                // the number the scheduler actually enforces.
                allocatable: Capacity::read(node, "pods").allocatable,
            },
            version: info.map_or_else(|| UNKNOWN.to_owned(), |info| info.kubelet_version.clone()),
            age: node.metadata.creation_timestamp.as_ref().map_or_else(
                || UNKNOWN.to_owned(),
                |created| format::human_duration(now.duration_since(created.0)),
            ),
            created_at: node
                .metadata
                .creation_timestamp
                .as_ref()
                .map(|created| created.0),
            internal_ip: address(node, "InternalIP"),
            external_ip: address(node, "ExternalIP"),
            os_image: system_field(info, |info| &info.os_image),
            kernel_version: system_field(info, |info| &info.kernel_version),
            container_runtime: system_field(info, |info| &info.container_runtime_version),
            devices: devices(node, requested),
            ephemeral_storage: Capacity::read(node, "ephemeral-storage"),
            hugepages: hugepages(node),
        }
    }
}

/// The prefix every huge-page resource name carries: `hugepages-2Mi`,
/// `hugepages-1Gi`. Kubernetes' own naming, not one this tool invented.
const HUGEPAGE_PREFIX: &str = "hugepages-";

/// The extended resources one node advertises, and what is booked of each.
///
/// The names are the union of the node's `capacity` and `allocatable` maps, not
/// either one alone: a node that reports a device under one and not the other
/// is mid-registration or mid-failure, and that is the state this column exists
/// to make visible rather than one to drop a node's device for.
///
/// Which names count is [`crate::k8s::resource::is_extended`]'s answer, so
/// `cpu`, `memory`, `pods`, and the `hugepages-*` entries beside them never
/// arrive here — they either have a column already or want one with a heading
/// of their own.
fn devices(node: &Node, requested: Option<&Requests>) -> BTreeMap<String, Device> {
    let status = node.status.as_ref();
    let names: BTreeSet<&str> = [
        status.and_then(|status| status.capacity.as_ref()),
        status.and_then(|status| status.allocatable.as_ref()),
    ]
    .into_iter()
    .flatten()
    .flat_map(|map| map.keys().map(String::as_str))
    .filter(|name| resource::is_extended(name))
    .collect();

    names
        .into_iter()
        .map(|name| {
            let device = Device {
                capacity: Capacity::read(node, name),
                // A device absent from the pod total is one nobody asked for,
                // which is a real zero. Only a failed pod listing — `None` all
                // the way from the command layer — leaves it unknown.
                booked: requested.map(|total| total.extended(name)),
            };
            (name.to_owned(), device)
        })
        .collect()
}

/// A node's huge-page pools, by size.
///
/// The union of `capacity` and `allocatable`, exactly as [`devices`] reads
/// them, and for the same mid-registration reason. Unlike `devices`, this is
/// not filtered to a rare condition — every node in a real cluster reports
/// every size the kernel was built with, almost always at `0` — so the
/// question of which sizes are worth a column is answered later, by
/// [`hugepage_names`], over the rows rather than here per node.
fn hugepages(node: &Node) -> BTreeMap<String, Capacity> {
    let status = node.status.as_ref();
    let names: BTreeSet<&str> = [
        status.and_then(|status| status.capacity.as_ref()),
        status.and_then(|status| status.allocatable.as_ref()),
    ]
    .into_iter()
    .flatten()
    .flat_map(|map| map.keys().map(String::as_str))
    .filter(|name| name.starts_with(HUGEPAGE_PREFIX))
    .collect();

    names
        .into_iter()
        .map(|name| (name.to_owned(), Capacity::read(node, name)))
        .collect()
}

/// Whether a capacity pair is worth a reader's attention: some huge-page pool
/// actually reserved, rather than the kernel merely supporting the size.
///
/// `false` when neither half is known, same as an unset pool.
fn is_nonzero(capacity: Capacity) -> bool {
    capacity.allocatable.is_some_and(|q| q.units() > 0)
        || capacity.capacity.is_some_and(|q| q.units() > 0)
}

/// One of a node's reported addresses, by type, or `-` if it has none of it.
///
/// The first match wins, as `kubectl` does it. A node can report several
/// addresses of one type — a machine on two subnets — and the API server lists
/// the primary one first.
fn address(node: &Node, kind: &str) -> String {
    node.status
        .as_ref()
        .and_then(|status| status.addresses.as_ref())
        .and_then(|addresses| {
            addresses
                .iter()
                .find(|entry| entry.type_ == kind)
                .map(|entry| entry.address.as_str())
        })
        .filter(|address| !address.is_empty())
        .map_or_else(|| UNKNOWN.to_owned(), str::to_owned)
}

/// One string out of a node's `nodeInfo`, or `-` where the node has not
/// reported it.
///
/// The fields are plain `String`s rather than `Option`s in the API type, so
/// "not reported" arrives as an empty string and has to be caught here — an
/// empty cell would read as a rendering fault rather than as a node that has
/// only just registered.
fn system_field(info: Option<&NodeSystemInfo>, pick: fn(&NodeSystemInfo) -> &String) -> String {
    info.map(pick)
        .filter(|value| !value.is_empty())
        .map_or_else(|| UNKNOWN.to_owned(), Clone::clone)
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

/// Whether a listing has live usage worth two columns.
///
/// A cluster with no metrics-server gains no empty columns — the far more
/// common case on EKS, where it is not installed by default — and the footnote
/// carries the news instead. One node the sampler has not reached yet is not
/// enough to drop the columns for everyone else, so this is `any` rather than
/// `all`.
///
/// Public because the command layer asks the same question for a second reason:
/// a table with no usage columns owes the reader a footnote saying why, and
/// which footnote that is depends on whether the read failed or merely came back
/// empty. Asking the rows rather than the request keeps the note and the columns
/// from disagreeing.
#[must_use]
pub fn shows_usage(rows: &[NodeRow]) -> bool {
    rows.iter()
        .any(|row| row.cpu_used.amount.is_some() || row.memory_used.amount.is_some())
}

/// The usage-freshness note for a renderer that keeps its own copy of the
/// rows rather than printing a footnote list — the dashboard's node pane.
///
/// `list` foots the CLI table with a wrapped version of this: `Unsampled`
/// gets [`usage_unsampled`], which names `CPU USE` and `MEM USE` because
/// those are the table's own headings. The pane's bars have no such headings,
/// so this uses [`metrics::unsampled`] bare rather than inventing a second
/// wrapping for columns that do not exist. `Unreadable` says nothing here —
/// the pane has no footnote list to add `usage_unavailable` to yet, and a
/// bar reading `-` already says the figure did not arrive.
///
/// The classification is [`metrics::Outcome::of`], asked of the rows rather
/// than of the read result, for the reason its own doc comment gives: a read
/// that answered for pods a selector kept out of the table must not be
/// called `Shown` here either. `usage` is `Ok` when the read succeeded,
/// whatever it found — only whether it happened is asked, not what failed.
#[must_use]
pub fn usage_note(
    rows: &[NodeRow],
    usage: &Result<(), String>,
    samples: &[Option<Sample>],
    now: Timestamp,
    label: &str,
) -> Option<String> {
    match metrics::Outcome::of(usage.as_ref().ok(), shows_usage(rows)) {
        metrics::Outcome::Shown => {
            metrics::freshness(samples.iter().flatten(), now).map(metrics::freshness_note)
        }
        metrics::Outcome::Unsampled => Some(metrics::unsampled(label)),
        metrics::Outcome::Unreadable => None,
    }
}

/// Whether the pane's own [`usage_note`] already accounts for the `Cpu`/
/// `Memory` orderings ranking nothing — the one fact
/// [`order::cause`](crate::k8s::order::Cause) needs that the pane does not
/// keep as its own field.
///
/// The CLI table has two footnotes for a missing `CPU USE`/`MEM USE` column —
/// one for a failed read, one for a read that answered with nothing — and
/// [`cause`] points the usage orderings at either. The pane has only the
/// second: `usage_note` says nothing when the read failed outright (see its
/// own doc comment), so a failed read stays honestly unexplained here too.
///
/// Recovered from what the pane already has rather than a field carrying the
/// raw `Result`, because [`usage_note`]'s own three-way match makes the
/// answer a fact about its *output*: [`shows_usage`] is false in both the
/// unsampled and the unreadable case (neither put a figure on any row), and
/// among those two `usage_note` is `Some` in exactly the unsampled one. A
/// listing where [`shows_usage`] is true never reaches this — a `Cpu`/
/// `Memory` ordering has already ranked something there, and [`order::cause`]
/// is only ever asked about an ordering that ranked nothing.
#[must_use]
pub fn usage_missing_explained(rows: &[NodeRow], usage_note: Option<&str>) -> bool {
    !shows_usage(rows) && usage_note.is_some()
}

/// One column of the node table.
///
/// A value rather than two parallel lists of headers and cells, for the reason
/// [`crate::k8s::pods::row`]'s twin gives: a header added under one condition
/// and its cell under a subtly different one shifts every figure to the right
/// of it under the wrong heading, and the table still renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Column<'a> {
    Name,
    Status,
    Version,
    Cpu,
    CpuRequested,
    CpuUsed,
    Memory,
    MemoryRequested,
    MemoryUsed,
    Pods,
    /// One extended resource, by its fully-qualified name.
    ///
    /// The only column whose identity is not known until the nodes arrive, so
    /// it borrows the name from the rows it was computed from rather than
    /// owning a copy per column.
    Device(&'a str),
    EphemeralStorage,
    /// One huge-page size, by its bare name (`hugepages-2Mi`).
    ///
    /// Named like [`Device`](Self::Device) for the same reason — the sizes a
    /// cluster reports are not known until the nodes arrive — but shaped like
    /// [`Memory`](Self::Memory) rather than like a device count: a huge-page
    /// pool is bytes reserved, not hardware offered.
    Hugepage(&'a str),
    Age,
    InternalIp,
    ExternalIp,
    OsImage,
    KernelVersion,
    ContainerRuntime,
}

impl Column<'_> {
    /// The heading. The wide ones are spelled as `kubectl get nodes -o wide`
    /// spells them, hyphens and all, so the two tables can be read together.
    ///
    /// A `String` rather than a `&'static str` because a device column is
    /// headed by a name the cluster invented; every other column pays one small
    /// allocation per table for it.
    fn header(self) -> String {
        match self {
            Self::Name => "NAME".to_owned(),
            Self::Status => "STATUS".to_owned(),
            Self::Version => "VERSION".to_owned(),
            Self::Cpu => "CPU".to_owned(),
            Self::CpuRequested => "CPU REQ".to_owned(),
            Self::CpuUsed => "CPU USE".to_owned(),
            Self::Memory => "MEMORY".to_owned(),
            Self::MemoryRequested => "MEM REQ".to_owned(),
            Self::MemoryUsed => "MEM USE".to_owned(),
            Self::Pods => "PODS".to_owned(),
            Self::Device(name) | Self::Hugepage(name) => resource::heading(name),
            Self::EphemeralStorage => resource::heading("ephemeral-storage"),
            Self::Age => "AGE".to_owned(),
            Self::InternalIp => "INTERNAL-IP".to_owned(),
            Self::ExternalIp => "EXTERNAL-IP".to_owned(),
            Self::OsImage => "OS-IMAGE".to_owned(),
            Self::KernelVersion => "KERNEL-VERSION".to_owned(),
            Self::ContainerRuntime => "CONTAINER-RUNTIME".to_owned(),
        }
    }

    /// This column's cell for one row: the text, and how alarming it is.
    fn cell(self, row: &NodeRow) -> format::Cell {
        match self.severity(row) {
            Some(severity) => format::Cell::graded(self.text(row), severity),
            None => format::Cell::plain(self.text(row)),
        }
    }

    /// This column's text for one row.
    fn text(self, row: &NodeRow) -> String {
        match self {
            Self::Name => row.name.clone(),
            Self::Status => row.status.clone(),
            Self::Version => row.version.clone(),
            Self::Cpu => row.cpu.cell(quantity::cpu),
            Self::CpuRequested => row.cpu_requested.cell(quantity::cpu),
            Self::CpuUsed => row.cpu_used.cell(quantity::cpu),
            Self::Memory => row.memory.cell(quantity::memory),
            Self::MemoryRequested => row.memory_requested.cell(quantity::memory),
            Self::MemoryUsed => row.memory_used.cell(quantity::memory),
            // The limit is spelled out rather than left to the percentage,
            // because it varies by instance type and by CNI configuration:
            // `21%` means something quite different on a node that takes 17
            // pods and one that takes 234, and nobody knows which this is.
            Self::Pods => row.pods.pair(quantity::count),
            // A node that does not report this device at all reads `-`. It is
            // not a node with none free; it is a node with no such hardware,
            // and the two want different reactions from whoever is looking for
            // somewhere to put a GPU pod.
            Self::Device(name) => row
                .devices
                .get(name)
                .map_or_else(|| UNKNOWN.to_owned(), |device| device.cell()),
            Self::EphemeralStorage => row.ephemeral_storage.cell(quantity::memory),
            // A pool this row does not report at all — the union across the
            // listing found it on a different node — reads `-`, the same
            // answer a node without a device gets; found but empty is `0/…`.
            Self::Hugepage(name) => row.hugepages.get(name).map_or_else(
                || UNKNOWN.to_owned(),
                |capacity| capacity.cell(quantity::memory),
            ),
            Self::Age => row.age.clone(),
            Self::InternalIp => row.internal_ip.clone(),
            Self::ExternalIp => row.external_ip.clone(),
            Self::OsImage => row.os_image.clone(),
            Self::KernelVersion => row.kernel_version.clone(),
            Self::ContainerRuntime => row.container_runtime.clone(),
        }
    }

    /// How alarming this column's value is on this row, or `None` for a column
    /// that carries no judgement.
    ///
    /// The `None` columns are not an oversight, they are most of the table. A
    /// node's name, its kubelet version, its age, its addresses, and the AMI it
    /// booted are facts with nothing to be worried about; so is `CPU`, which
    /// says how big the machine is and not how much of it is spoken for. Every
    /// column that *is* graded here is a share of something — booked, burnt, or
    /// counted — plus `STATUS`, which is the row's own health.
    ///
    /// Nothing new is decided here. `STATUS` takes the severity `NodeRow`
    /// already carries so the CLI table and the dashboard cannot disagree about
    /// what a cordoned node is, and every share takes
    /// [`Share::severity`], which is
    /// [`Severity::from_utilisation`]'s single set of thresholds. This function
    /// only says *which cells* have a reading to colour.
    ///
    /// [`Severity::from_utilisation`]: crate::theme::Severity::from_utilisation
    fn severity(self, row: &NodeRow) -> Option<Severity> {
        match self {
            Self::Status => Some(row.severity),
            Self::CpuRequested => Some(row.cpu_requested.severity()),
            Self::CpuUsed => Some(row.cpu_used.severity()),
            Self::MemoryRequested => Some(row.memory_requested.severity()),
            Self::MemoryUsed => Some(row.memory_used.severity()),
            Self::Pods => Some(row.pods.severity()),
            // A node with none of this hardware reads `-` and grades
            // `Unknown`, which is the same answer the `-` in a request column
            // gets: the cell is an absence, and a muted one says so.
            Self::Device(name) => Some(
                row.devices
                    .get(name)
                    .map_or(Severity::Unknown, |device| device.share().severity()),
            ),
            Self::Name
            | Self::Version
            | Self::Cpu
            | Self::Memory
            | Self::EphemeralStorage
            | Self::Hugepage(_)
            | Self::Age
            | Self::InternalIp
            | Self::ExternalIp
            | Self::OsImage
            | Self::KernelVersion
            | Self::ContainerRuntime => None,
        }
    }
}

/// Every extended resource some node in this listing reports, in name order.
///
/// A union across the rows rather than the first node's list, and `any` rather
/// than `all` for the same reason the usage columns are: a cluster with one GPU
/// node group and three CPU ones must still get the column, and the nodes
/// without the hardware read `-`. Name order so two runs of one command cannot
/// put the columns in a different order.
fn device_names(rows: &[NodeRow]) -> BTreeSet<&str> {
    rows.iter()
        .flat_map(|row| row.devices.keys().map(String::as_str))
        .collect()
}

/// Whether any row has a figure for ephemeral storage worth a column.
///
/// The same `any`-not-`all` rule as [`shows_usage`]: a node still registering,
/// mid-listing, must not cost everyone else the column.
fn shows_ephemeral_storage(rows: &[NodeRow]) -> bool {
    rows.iter().any(|row| {
        row.ephemeral_storage.allocatable.is_some() || row.ephemeral_storage.capacity.is_some()
    })
}

/// Every huge-page size worth a column: one some row in this listing has
/// actually reserved.
///
/// Not the union of every size any row *reports* — [`hugepages`] puts an entry
/// on almost every node, at `0`, because the kernel supports the size whether
/// or not anyone asked for a pool of it. A column of zeroes headed
/// `HUGEPAGES-2MI` on every listing would be exactly the noise
/// [`resource::is_extended`] excludes `hugepages-*` from the device treatment
/// to avoid; this is the same condition applied one level up, at the column
/// rather than the resource name.
fn hugepage_names(rows: &[NodeRow]) -> BTreeSet<&str> {
    rows.iter()
        .flat_map(|row| row.hugepages.iter())
        .filter(|&(_, &capacity)| is_nonzero(capacity))
        .map(|(name, _)| name.as_str())
        .collect()
}

/// Which columns this listing gets, in order.
///
/// A pure function over the three things that decide it — whether any row has
/// live usage, which extended resources the listing reports, and the [`Width`]
/// — so the layout is settled by a test rather than by reading a table in a
/// terminal. The conditions differ in the same way the pod table's do: the
/// usage and device columns appear unasked for and so are dropped when a
/// cluster has nothing to put in them, while `--wide` columns were asked for
/// and appear whatever is in them.
///
/// [`Width::Narrow`] then drops columns from that starting set until the row
/// fits its target — see [`DROP_ORDER`]. `Wide` is not narrowed: the user
/// typed `--wide` for the extra columns, not for a table that stays away from
/// them.
///
/// [`Width`]: format::Width
/// [`Width::Narrow`]: format::Width::Narrow
pub(crate) fn columns(rows: &[NodeRow], width: format::Width) -> Vec<Column<'_>> {
    // Each REQ and USE column sits beside the capacity it is a share of, so the
    // comparison a person actually wants — booked against burnt — is a glance
    // rather than a scan across the row. AGE stays last of the default columns
    // because that is where every `kubectl get` puts it.
    let usage = shows_usage(rows);
    let mut columns = vec![
        Column::Name,
        Column::Status,
        Column::Version,
        Column::Cpu,
        Column::CpuRequested,
    ];
    if usage {
        columns.push(Column::CpuUsed);
    }
    columns.extend([Column::Memory, Column::MemoryRequested]);
    if usage {
        columns.push(Column::MemoryUsed);
    }
    // PODS after the two resources it is measured beside and before the
    // devices, because it is the third answer to the same question they answer
    // — what will still fit here — and the one that is true of every node
    // rather than only of the ones somebody put hardware in.
    columns.push(Column::Pods);
    // Ephemeral storage, devices, and huge pages sit after the resources every
    // node is measured against and before AGE, so the block of "what this
    // machine can give out" stays together and AGE stays last of the default
    // columns, where every `kubectl get` puts it. Ephemeral storage goes
    // first of the three: it is a capacity pair like CPU and MEMORY, and the
    // other two are conditional, rarer facts about the same machine.
    if shows_ephemeral_storage(rows) {
        columns.push(Column::EphemeralStorage);
    }
    columns.extend(device_names(rows).into_iter().map(Column::Device));
    columns.extend(hugepage_names(rows).into_iter().map(Column::Hugepage));
    columns.push(Column::Age);
    match width {
        format::Width::Default => columns,
        // `kubectl get nodes -o wide`'s own tail, in its order. It goes after
        // AGE rather than after VERSION, where `kubectl` puts it, so that the
        // default table is the same table with the tail cut off — a user
        // comparing a wide listing against a plain one should not have to
        // re-find the columns.
        format::Width::Wide => {
            columns.extend([
                Column::InternalIp,
                Column::ExternalIp,
                Column::OsImage,
                Column::KernelVersion,
                Column::ContainerRuntime,
            ]);
            columns
        }
        format::Width::Narrow(target) => narrow_to_fit(&columns, rows, target),
    }
}

/// The order columns get dropped in when [`Width::Narrow`] cannot fit them all.
///
/// A list of predicates rather than a single ranking, because some columns want
/// to leave together: `CPU REQ` and `MEM REQ` are placed to be read as one
/// answer to "what is booked on this machine", and dropping one but not the
/// other makes the remaining half read as noise. Partner columns — `CPU REQ`
/// and `CPU USE` alongside `CPU` — also drop before their base, so a request
/// or usage figure is never left in the table without the capacity it is a
/// share of.
///
/// The steps:
///
/// 1. `EPHEMERAL-STORAGE` and every `HUGEPAGES-*` column, together — the
///    newest and least asked-for facts on the row, and neither was ever
///    visible before tonight. On the overwhelming majority of listings, which
///    have no huge pages reserved, this step drops nothing and the next runs
///    in the same pass.
/// 2. `VERSION` — the same string on every node in a node group, easy to get
///    from `kubectl` on the day it matters.
/// 3. `AGE` — a standard column but rarely the one people came for.
/// 4. `PODS` — the first of the three booked figures to go, and deliberately
///    ahead of the other two. It is a third answer to "what will still fit
///    here", and the least often the binding one: a node runs out of CPU or
///    memory long before it runs out of pod slots, unless the CNI's address
///    budget is what is short. It also arrived last, and a column added later
///    should not be the thing that evicts `CPU REQ` and `MEM REQ` from every
///    80-column listing that has been keeping them.
/// 5. `CPU REQ` and `MEM REQ` — dropping the pair leaves capacity and usage
///    side-by-side, which is the "how busy is this machine" question.
/// 6. `CPU USE` and `MEM USE` — the pair the tool exists for, so late.
/// 7. `CPU` and `MEMORY` — the machine's own specs, dropped before the device
///    columns rather than after them: a device column only exists because
///    somebody installed the plugin that surfaces it, and a GPU cluster
///    surviving a narrow terminal with `GPU` intact and `CPU` gone is the
///    right trade — every listing has `CPU`, and only the interesting one has
///    the card.
/// 8. Every device column, together — a GPU cluster wants them all or none,
///    and the alphabet is a bad rule for "which card is important". On a
///    cluster with no devices this step is a no-op and the next runs in the
///    same pass.
/// 9. `STATUS` — the last thing to go before `NAME` is alone.
///
/// `NAME` never drops. A row we cannot fit at all is still a row with a name;
/// the terminal wraps it, and dropping the name would leave a row that no
/// listing can be about.
///
/// [`Width::Narrow`]: format::Width::Narrow
const DROP_ORDER: &[fn(&Column<'_>) -> bool] = &[
    |c| matches!(c, Column::EphemeralStorage | Column::Hugepage(_)),
    |c| matches!(c, Column::Version),
    |c| matches!(c, Column::Age),
    |c| matches!(c, Column::Pods),
    |c| matches!(c, Column::CpuRequested | Column::MemoryRequested),
    |c| matches!(c, Column::CpuUsed | Column::MemoryUsed),
    |c| matches!(c, Column::Cpu | Column::Memory),
    |c| matches!(c, Column::Device(_)),
    |c| matches!(c, Column::Status),
];

/// Drop columns from `columns` in [`DROP_ORDER`] until the row fits `target`.
///
/// Stops as soon as the row fits: on a wide-enough terminal a `Narrow(target)`
/// returns exactly what `Default` does, byte for byte. When even one column
/// cannot fit — a `--width 1`, or a very narrow terminal on a cluster with
/// long node names — the last step leaves only `NAME` and the row prints
/// wider than the target, which is the terminal's problem to wrap rather than
/// ours to solve by dropping the name too.
fn narrow_to_fit<'a>(columns: &[Column<'a>], rows: &[NodeRow], target: u16) -> Vec<Column<'a>> {
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
/// Asks [`format::column_widths`] the question, over the same headers and
/// cells [`render`] is about to hand [`format::table`]. The arithmetic itself
/// — each column as wide as its widest cell, two spaces between — belongs to
/// the renderer: a copy of it here would be free to drift, and a drop rule
/// measuring rows the renderer disagreed with would stop at a width nothing
/// prints at.
fn widths(columns: &[Column<'_>], rows: &[NodeRow]) -> Vec<usize> {
    let headers: Vec<String> = columns.iter().map(|column| column.header()).collect();
    let headers: Vec<&str> = headers.iter().map(String::as_str).collect();
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

/// Render the `eks nodes` table.
///
/// `cluster` is the human label used in the empty-list message, so a user who
/// typed the wrong `--context` finds out from the answer.
///
/// `width` is `--wide`. It changes only which columns are printed, never what
/// was fetched: everything the extra columns show came back with the nodes.
///
/// `notes` are appended under the table — see [`requests_unavailable`] and
/// [`usage_unavailable`]. They are dropped when there are no nodes, where a
/// footnote about missing columns would only be noise on top of a bigger
/// problem.
///
/// `palette` decides whether the graded cells — `STATUS`, and every share —
/// are written in ink. It changes no column, no width, and no footnote: a
/// [`Palette::Plain`] listing is this table exactly as it was before colour
/// existed, and a coloured one has its columns in the same places.
///
/// [`Palette::Plain`]: crate::theme::Palette::Plain
#[must_use]
pub fn render(
    rows: &[NodeRow],
    cluster: &str,
    notes: &[String],
    width: format::Width,
    palette: Palette,
) -> String {
    if rows.is_empty() {
        return format!(
            "{cluster} reports no nodes.\n\
             If you expected some, check that its node groups are scaled above zero."
        );
    }

    let columns = columns(rows, width);
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

/// The footnote shown when the pods could not be listed, so the empty request
/// columns are explained rather than mistaken for an idle cluster.
///
/// `explanation` is `k8s::explain`'s sentence about the underlying failure —
/// usually that the user's role covers nodes but not pods cluster-wide.
///
/// The columns it names come from the rows rather than from a constant, because
/// a device column has a booked figure in it too and it goes the same way.
///
/// It says "the booked half" of `PODS` and the device columns because the rest
/// of those cells survives: the pod limit and the device count came back with
/// the nodes, and only the numerator is missing, so they read `-/58` rather
/// than emptying. Saying they were empty would be visibly untrue on screen.
#[must_use]
pub fn requests_unavailable(rows: &[NodeRow], explanation: &str) -> String {
    // PODS first and always: it is on every listing, where a device column is
    // on the ones somebody put hardware in.
    let counted: Vec<String> = std::iter::once("PODS".to_owned())
        .chain(device_names(rows).into_iter().map(resource::heading))
        .collect();

    let columns = match format::list(&counted, "and") {
        Some(counted) => format!("CPU REQ, MEM REQ, and the booked half of {counted} are empty"),
        // Unreachable while PODS is unconditional, and written as the honest
        // fallback anyway rather than as an `expect` that would take a
        // terminal down over a footnote.
        None => "CPU REQ and MEM REQ are empty".to_owned(),
    };

    format!("{columns} because the pods could not be listed.\n{explanation}")
}

/// The footnote for nodes holding back devices they report.
///
/// A device column shows what the scheduler will hand out, which is the number
/// that decides whether the next pod fits. When a node's capacity is larger
/// than that, the difference is hardware the kubelet has and will not schedule
/// onto — usually a device its plugin marked unhealthy — and nothing in the
/// table shows it. That absence is precisely the case this whole column was
/// added for: a GPU pod sitting `Pending` on a cluster whose GPU node looks,
/// from the table, half empty.
///
/// `None` when every node offers everything it has, which is the ordinary case
/// and earns no line.
#[must_use]
pub fn devices_withheld(rows: &[NodeRow]) -> Option<String> {
    let mut withheld: Vec<(&str, &str, Quantity, Quantity)> = rows
        .iter()
        .flat_map(|row| {
            row.devices.iter().filter_map(move |(name, device)| {
                let (offered, held) = device.withheld()?;
                Some((row.name.as_str(), name.as_str(), offered, held))
            })
        })
        .collect();

    // Widest gap first, then by node and resource name, so the node this names
    // is the one worth looking at — and is the same one on every run, whatever
    // order `--sort` put the table in.
    withheld.sort_by_key(|&(node, name, offered, held)| {
        (
            Reverse(held.thousandths() - offered.thousandths()),
            node,
            name,
        )
    });
    let &(node, name, offered, held) = withheld.first()?;

    let (offered, held) = (quantity::count(offered), quantity::count(held));

    // Counted by node rather than by device, because the thing to go and look
    // at is a machine: one node with two sick device plugins is one visit.
    let nodes: BTreeSet<&str> = withheld.iter().map(|&(node, ..)| node).collect();
    // Two sentences rather than one clause slotted into either, because a
    // subordinate clause that has to read as both a whole sentence and a noun
    // phrase ends up doing neither well.
    let summary = if nodes.len() == 1 {
        format!("{node} offers {offered} of the {held} {name} it reports.")
    } else {
        format!(
            "{} nodes offer fewer devices than they report, including {node}, \
             which offers {offered} of the {held} {name} it has.",
            nodes.len()
        )
    };

    Some(format!(
        "{summary}\n\
         A device a node has but will not offer is usually one its plugin marked unhealthy; \
         check the device-plugin pods there, because a pod asking for the missing one will \
         stay Pending."
    ))
}

/// The footnote shown when there is no live usage to put in a column.
///
/// Unlike the request columns, the usage ones are simply absent in this case
/// rather than empty, so the note has to say what is missing — otherwise a
/// perfectly ordinary table silently answers a question the user thought they
/// had asked.
///
/// `explanation` is `k8s::metrics::explain`'s sentence, which for the usual
/// cause says what to install.
#[must_use]
pub fn usage_unavailable(explanation: &str) -> String {
    format!(
        "CPU USE and MEM USE are not shown because live usage could not be read.\n{explanation}"
    )
}

/// The footnote shown when the metrics read succeeded and had nothing in it.
///
/// The columns vanish exactly as they do when the read fails, and until now
/// nothing was printed about it — because the footnote above is written on the
/// error path and there was no error. A table that quietly drops two columns
/// after a successful request is the worst of the three cases: the user cannot
/// tell it apart from a cluster with no metrics-server, and the advice for the
/// two is different.
///
/// `explanation` is [`crate::k8s::metrics::unsampled`]'s sentence, which says
/// metrics-server is installed and what waiting on it looks like.
#[must_use]
pub fn usage_unsampled(explanation: &str) -> String {
    format!(
        "CPU USE and MEM USE are not shown because nothing here has been sampled yet.\n{explanation}"
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use crate::format::Width;

    use std::collections::BTreeMap;

    use k8s_openapi::api::core::v1::{
        NodeAddress, NodeCondition, NodeSpec, NodeStatus, NodeSystemInfo,
    };
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity as ApiQuantity;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
    use k8s_openapi::jiff::SignedDuration;

    use super::*;

    /// Most tests care about a node's own fields, not its pods; this is the
    /// "we listed the pods and found none" case, which is a real zero in both
    /// halves — no pods, and nothing booked.
    fn idle() -> Placed {
        Placed::default()
    }

    /// A node with pods on it, between them booking `cpu` and `memory`.
    ///
    /// The count is fixed at a plausible number rather than derived from the
    /// figures, because these tests are about the figures; the ones about the
    /// count say so by calling [`running`].
    fn booked(cpu: &str, memory: &str) -> Placed {
        running(12, cpu, memory)
    }

    fn running(pods: u32, cpu: &str, memory: &str) -> Placed {
        Placed {
            pods,
            requests: Requests {
                cpu: Quantity::parse(cpu).unwrap_or_default(),
                memory: Quantity::parse(memory).unwrap_or_default(),
                ..Requests::default()
            },
        }
    }

    fn used(cpu: &str, memory: &str) -> Usage {
        Usage {
            cpu: Quantity::parse(cpu).ok(),
            memory: Quantity::parse(memory).ok(),
        }
    }

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
                // What an m5.xlarge actually reports — `pods` included, because
                // every node has a limit and the table now shows it.
                capacity: Some(quantities(&[
                    ("cpu", "4"),
                    ("memory", "16374624Ki"),
                    ("pods", "58"),
                ])),
                allocatable: Some(quantities(&[
                    ("cpu", "3920m"),
                    ("memory", "15525152Ki"),
                    ("pods", "58"),
                ])),
                node_info: Some(NodeSystemInfo {
                    kubelet_version: "v1.33.1-eks-1a2b3c4".to_owned(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        }
    }

    /// A second node, differing from [`healthy_node`] in every field an
    /// ordering here could rank by: status and creation time. The
    /// "unranked note" tests below pair the two so their advice proves
    /// something stronger than "this ordering ranked a row" — that it would
    /// actually put two rows in a different arrangement, which is what
    /// [`crate::k8s::nodes::distinguishes`] asks and a single-row listing
    /// can never answer yes to.
    fn contrasting_node() -> Node {
        let mut other = healthy_node();
        other.metadata.name = Some("ip-10-0-2-7.ec2.internal".to_owned());
        other.metadata.creation_timestamp = Some(ago(2));
        if let Some(status) = other.status.as_mut() {
            status.conditions = Some(vec![condition("Ready", "False")]);
        }
        other
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
        let row = NodeRow::from_node(&healthy_node(), Some(&idle()), None, now());

        assert_eq!(row.name, "ip-10-0-1-9.ec2.internal");
        assert_eq!(row.status, "Ready");
        assert_eq!(row.severity, Severity::Ok);
        assert_eq!(row.version, "v1.33.1-eks-1a2b3c4");
        assert_eq!(row.age, "2d2h");
    }

    #[test]
    fn capacity_and_allocatable_are_read_as_separate_numbers() {
        let row = NodeRow::from_node(&healthy_node(), Some(&idle()), None, now());

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
        let row = NodeRow::from_node(&healthy_node(), Some(&idle()), None, now());
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

        let row = NodeRow::from_node(&node, Some(&idle()), None, now());
        assert_eq!(row.cpu.cell(quantity::cpu), "-");
        assert_eq!(row.memory.cell(quantity::memory), "-");
    }

    #[test]
    fn a_node_reporting_only_one_half_shows_the_half_it_has() {
        let mut node = healthy_node();
        if let Some(status) = node.status.as_mut() {
            status.allocatable = None;
        }

        let row = NodeRow::from_node(&node, Some(&idle()), None, now());
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

        let row = NodeRow::from_node(&node, Some(&idle()), None, now());
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
            let row = NodeRow::from_node(&node, Some(&idle()), None, now());

            assert_eq!(row.status, "NotReady", "Ready={status}");
            assert_eq!(row.severity, Severity::Critical, "Ready={status}");
        }
    }

    #[test]
    fn a_node_with_no_ready_condition_is_unknown_rather_than_broken() {
        let node = with_status(&healthy_node(), vec![condition("MemoryPressure", "False")]);
        let row = NodeRow::from_node(&node, Some(&idle()), None, now());

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

        let row = NodeRow::from_node(&node, Some(&idle()), None, now());
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

        let row = NodeRow::from_node(&node, Some(&idle()), None, now());
        assert_eq!(row.status, "NotReady,SchedulingDisabled");
        assert_eq!(row.severity, Severity::Critical);
    }

    #[test]
    fn a_node_with_nothing_filled_in_still_produces_a_row() {
        // Everything under `status` is optional in the API, and a node caught
        // mid-registration really can arrive like this.
        let row = NodeRow::from_node(&Node::default(), Some(&idle()), None, now());

        assert_eq!(row.name, "-");
        assert_eq!(row.status, "Unknown");
        assert_eq!(row.version, "-");
        assert_eq!(row.age, "-");
        assert_eq!(row.severity, Severity::Unknown);
        assert_eq!(row.cpu, Capacity::default());
        assert_eq!(row.memory, Capacity::default());
        // Pods were listed and this node has none, but with no allocatable to
        // divide by there is no percentage to show.
        assert_eq!(row.cpu_requested.cell(quantity::cpu), "0");
        assert_eq!(row.memory_requested.cell(quantity::memory), "0");
        assert_eq!(row.cpu_requested.severity(), Severity::Unknown);
    }

    #[test]
    fn requests_are_shown_against_allocatable_as_a_percentage() {
        let row = NodeRow::from_node(
            &healthy_node(),
            Some(&booked("1960m", "7762576Ki")),
            None,
            now(),
        );

        // Half of a 3920m allocatable, and half of a 15525152Ki one.
        assert_eq!(row.cpu_requested.cell(quantity::cpu), "1960m (50%)");
        assert_eq!(row.memory_requested.cell(quantity::memory), "7.4Gi (50%)");
        assert_eq!(row.cpu_requested.ratio(), Some(0.5));
    }

    #[test]
    fn a_node_running_no_pods_reads_as_zero_rather_than_as_an_error() {
        // The common case for a freshly scaled node group, and the one place a
        // missing value would be easiest to mistake for a bug.
        let row = NodeRow::from_node(&healthy_node(), Some(&idle()), None, now());

        assert_eq!(row.cpu_requested.cell(quantity::cpu), "0 (0%)");
        assert_eq!(row.memory_requested.cell(quantity::memory), "0 (0%)");
        assert_eq!(row.cpu_requested.severity(), Severity::Ok);
    }

    #[test]
    fn a_failed_pod_listing_reads_differently_from_an_empty_node() {
        // `None` means "we could not find out", which must not look like zero.
        let row = NodeRow::from_node(&healthy_node(), None, None, now());

        assert_eq!(row.cpu_requested.cell(quantity::cpu), "-");
        assert_eq!(row.memory_requested.cell(quantity::memory), "-");
        assert_eq!(row.cpu_requested.ratio(), None);
        assert_eq!(row.cpu_requested.severity(), Severity::Unknown);
    }

    // --- An explicit denominator ---------------------------------------------

    #[test]
    fn ratio_of_reads_the_denominator_it_is_given_rather_than_allocatable() {
        let share = Share {
            amount: Some(Quantity::parse("5500m").unwrap()),
            allocatable: Some(Quantity::parse("6").unwrap()),
        };

        // `ratio()` still answers "will another pod fit" against
        // allocatable...
        assert_eq!(share.ratio(), Some(5500.0 / 6000.0));
        // ...and `ratio_of` answers whatever question the caller asked, here
        // the node's raw 8-core capacity — the "is this machine busy"
        // reading a dashboard bar wants.
        let capacity = Some(Quantity::parse("8").unwrap());
        assert_eq!(share.ratio_of(capacity), Some(5500.0 / 8000.0));
    }

    #[test]
    fn ratio_of_is_unknown_with_no_amount_or_no_denominator() {
        let unmeasured = Share {
            amount: None,
            allocatable: Some(Quantity::parse("6").unwrap()),
        };
        assert_eq!(
            unmeasured.ratio_of(Some(Quantity::parse("8").unwrap())),
            None
        );

        let measured = Share {
            amount: Some(Quantity::parse("1").unwrap()),
            allocatable: None,
        };
        assert_eq!(measured.ratio_of(None), None);
    }

    #[test]
    fn severity_of_can_disagree_with_severity_over_the_same_measurement() {
        // The same 5.5-core reading is critical against a 6-core allocatable
        // and merely ok against an 8-core capacity — the whole reason a
        // dashboard bar and a `CPU USE` cell are allowed to read differently
        // for one node.
        let share = Share {
            amount: Some(Quantity::parse("5500m").unwrap()),
            allocatable: Some(Quantity::parse("6").unwrap()),
        };

        assert_eq!(share.severity(), Severity::Critical);
        assert_eq!(
            share.severity_of(Some(Quantity::parse("8").unwrap())),
            Severity::Ok
        );
    }

    // --- The pod count ------------------------------------------------------

    #[test]
    fn the_pod_count_is_shown_against_the_limit_the_node_will_accept() {
        // The third reason a pod will not schedule, and the one no amount of
        // spare CPU fixes.
        let row = NodeRow::from_node(&healthy_node(), Some(&running(12, "0", "0")), None, now());

        assert_eq!(row.pods.pair(quantity::count), "12/58 (21%)");
        assert_eq!(row.pods.ratio(), Some(12.0 / 58.0));
    }

    #[test]
    fn the_pod_limit_is_the_allocatable_one_the_scheduler_enforces() {
        // A node's `capacity.pods` and its `allocatable.pods` can differ, and
        // only the second one is a promise: it is what the kubelet's
        // `--max-pods` and the VPC CNI's address budget both land in, and what
        // the scheduler actually counts against. Dividing by capacity would
        // flatter every node whose CNI is the binding constraint, which on EKS
        // is most of them.
        let mut node = healthy_node();
        if let Some(status) = node.status.as_mut() {
            status.capacity = Some(quantities(&[("cpu", "4"), ("pods", "110")]));
            status.allocatable = Some(quantities(&[("cpu", "3920m"), ("pods", "58")]));
        }

        let row = NodeRow::from_node(&node, Some(&running(29, "0", "0")), None, now());

        assert_eq!(row.pods.pair(quantity::count), "29/58 (50%)");
    }

    #[test]
    fn a_node_at_its_pod_limit_is_as_alarming_as_one_out_of_cpu() {
        // The same `theme` thresholds every other share on the row goes
        // through: a node that cannot take another pod is full, whatever its
        // cores are doing.
        let row = NodeRow::from_node(&healthy_node(), Some(&running(58, "0", "0")), None, now());

        assert_eq!(row.pods.pair(quantity::count), "58/58 (100%)");
        assert_eq!(row.pods.severity(), Severity::Critical);
        // And the cores really are idle, which is the whole point of the
        // column: nothing else on this row says the node is full.
        assert_eq!(row.cpu_requested.severity(), Severity::Ok);
    }

    #[test]
    fn a_node_running_nothing_has_a_pod_count_of_zero_rather_than_an_unknown() {
        let row = NodeRow::from_node(&healthy_node(), Some(&idle()), None, now());

        assert_eq!(row.pods.pair(quantity::count), "0/58 (0%)");
        assert_eq!(row.pods.severity(), Severity::Ok);
    }

    #[test]
    fn a_failed_pod_listing_leaves_the_pod_count_without_a_numerator() {
        // The limit came back with the node and is still good; only the count
        // is unknown. `-/58` says more than an empty cell would, and much less
        // than `0/58` would falsely claim.
        let row = NodeRow::from_node(&healthy_node(), None, None, now());

        assert_eq!(row.pods.pair(quantity::count), "-/58");
        assert_eq!(row.pods.severity(), Severity::Unknown);
    }

    #[test]
    fn a_node_that_reports_no_pod_limit_still_shows_how_many_it_has() {
        // A node mid-registration has no allocatable map at all. The count is
        // a fact we worked out ourselves and is not the thing that went
        // missing, so it stands alone rather than vanishing with the limit.
        let mut node = healthy_node();
        if let Some(status) = node.status.as_mut() {
            status.allocatable = Some(quantities(&[("cpu", "3920m")]));
        }

        let row = NodeRow::from_node(&node, Some(&running(4, "0", "0")), None, now());

        assert_eq!(row.pods.pair(quantity::count), "4");
        assert_eq!(row.pods.ratio(), None);
    }

    #[test]
    fn a_node_over_its_pod_limit_says_so_rather_than_capping() {
        // Static pods and pods placed before `--max-pods` was lowered both do
        // this, and it is exactly the moment to be told.
        let row = NodeRow::from_node(&healthy_node(), Some(&running(60, "0", "0")), None, now());

        assert_eq!(row.pods.pair(quantity::count), "60/58 (103%)");
        assert_eq!(row.pods.severity(), Severity::Critical);
    }

    #[test]
    fn a_node_that_will_accept_no_pods_at_all_shows_no_percentage() {
        // A limit of zero has no percentage to be a share of, and `12/0` is a
        // better answer than a division by nothing. The same rule `Share`
        // already follows for a node reporting zero allocatable CPU.
        let mut node = healthy_node();
        if let Some(status) = node.status.as_mut() {
            status.allocatable = Some(quantities(&[("cpu", "3920m"), ("pods", "0")]));
        }

        let row = NodeRow::from_node(&node, Some(&running(2, "0", "0")), None, now());

        assert_eq!(row.pods.pair(quantity::count), "2/0");
        assert_eq!(row.pods.severity(), Severity::Unknown);
    }

    #[test]
    fn the_pod_column_sits_after_the_memory_it_is_the_third_answer_to() {
        let rows = [NodeRow::from_node(
            &healthy_node(),
            Some(&booked("1500m", "6Gi")),
            None,
            now(),
        )];

        let headings = headings(&rows, Width::Default);
        let at = |name: &str| headings.iter().position(|heading| heading == name);
        assert!(at("PODS") > at("MEM REQ"), "{headings:?}");
        assert!(at("PODS") < at("AGE"), "{headings:?}");
    }

    #[test]
    fn requests_without_a_reported_allocatable_still_show_the_absolute_figure() {
        // A node caught mid-registration knows nothing about its own capacity,
        // but the pods already assigned to it are still worth showing.
        let mut node = healthy_node();
        if let Some(status) = node.status.as_mut() {
            status.allocatable = None;
        }

        let row = NodeRow::from_node(&node, Some(&booked("500m", "1Gi")), None, now());
        assert_eq!(row.cpu_requested.cell(quantity::cpu), "500m");
        assert_eq!(row.memory_requested.cell(quantity::memory), "1Gi");
        assert_eq!(row.cpu_requested.severity(), Severity::Unknown);
    }

    #[test]
    fn percentages_use_the_shared_severity_thresholds() {
        // 3920m allocatable: 2940m is exactly 75%, 3528m exactly 90%.
        let cases = [
            ("100m", Severity::Ok),
            ("2939m", Severity::Ok),
            ("2940m", Severity::Warn),
            ("3527m", Severity::Warn),
            ("3528m", Severity::Critical),
        ];

        for (requested, expected) in cases {
            let row =
                NodeRow::from_node(&healthy_node(), Some(&booked(requested, "0")), None, now());
            assert_eq!(row.cpu_requested.severity(), expected, "{requested}");
        }
    }

    #[test]
    fn a_node_booked_past_its_allocatable_says_so_rather_than_capping() {
        // Pods placed before the kubelet revised its reservation really can add
        // up to more than allocatable, and that is the moment to say so.
        let row = NodeRow::from_node(&healthy_node(), Some(&booked("4312m", "0")), None, now());

        assert_eq!(row.cpu_requested.cell(quantity::cpu), "4312m (110%)");
        assert_eq!(row.cpu_requested.severity(), Severity::Critical);
    }

    #[test]
    fn rendering_lays_the_rows_out_in_columns() {
        let mut second = healthy_node();
        second.metadata.name = Some("ip-10-0-11-200.ec2.internal".to_owned());
        second.metadata.creation_timestamp = Some(ago(1));
        let second = with_status(&second, vec![condition("Ready", "False")]);

        let rows = [
            NodeRow::from_node(&healthy_node(), Some(&booked("1500m", "6Gi")), None, now()),
            // The second node is idle, which must read as a real zero.
            NodeRow::from_node(&second, Some(&idle()), None, now()),
        ];

        assert_eq!(
            render(
                &rows,
                "prod (us-east-1)",
                &[],
                Width::Default,
                Palette::Plain
            ),
            "NAME                         STATUS    VERSION              CPU      CPU REQ      MEMORY         MEM REQ    PODS         AGE\n\
             ip-10-0-1-9.ec2.internal     Ready     v1.33.1-eks-1a2b3c4  3920m/4  1500m (38%)  14.8Gi/15.6Gi  6Gi (41%)  12/58 (21%)  2d2h\n\
             ip-10-0-11-200.ec2.internal  NotReady  v1.33.1-eks-1a2b3c4  3920m/4  0 (0%)       14.8Gi/15.6Gi  0 (0%)     0/58 (0%)    60m"
        );
    }

    #[test]
    fn live_usage_is_shown_against_allocatable_as_a_percentage() {
        let row = NodeRow::from_node(
            &healthy_node(),
            Some(&booked("1960m", "7762576Ki")),
            Some(used("392m", "1552515Ki")),
            now(),
        );

        // A tenth of the same 3920m allocatable the requests are measured
        // against — the point of the pair of columns is that they divide by the
        // same number.
        assert_eq!(row.cpu_used.cell(quantity::cpu), "392m (10%)");
        assert_eq!(row.memory_used.cell(quantity::memory), "1.5Gi (10%)");
        assert_eq!(row.cpu_used.severity(), Severity::Ok);
    }

    #[test]
    fn a_node_running_hot_is_coloured_by_the_same_thresholds_as_its_requests() {
        // 3528m is exactly 90% of a 3920m allocatable.
        let row = NodeRow::from_node(
            &healthy_node(),
            Some(&booked("100m", "0")),
            Some(used("3528m", "0")),
            now(),
        );

        assert_eq!(row.cpu_used.severity(), Severity::Critical);
        // Booked low and burning hot is exactly the case the two columns exist
        // to tell apart, so they must not agree here.
        assert_eq!(row.cpu_requested.severity(), Severity::Ok);
    }

    #[test]
    fn a_cluster_with_no_metrics_server_gains_no_empty_columns() {
        let rows = [NodeRow::from_node(
            &healthy_node(),
            Some(&booked("1500m", "6Gi")),
            None,
            now(),
        )];

        assert!(!shows_usage(&rows));
        let table = render(
            &rows,
            "prod (us-east-1)",
            &[],
            Width::Default,
            Palette::Plain,
        );
        assert!(!table.contains("CPU USE"), "{table}");
        assert!(!table.contains("MEM USE"), "{table}");
    }

    #[test]
    fn one_unsampled_node_does_not_drop_the_columns_for_the_rest() {
        // metrics-server reports nothing for a node until it has scraped it,
        // which is a normal state for the first minute of a node's life.
        let mut second = healthy_node();
        second.metadata.name = Some("ip-10-0-11-200.ec2.internal".to_owned());

        let rows = [
            NodeRow::from_node(
                &healthy_node(),
                Some(&idle()),
                Some(used("392m", "1552515Ki")),
                now(),
            ),
            NodeRow::from_node(&second, Some(&idle()), None, now()),
        ];

        assert!(shows_usage(&rows));
        assert_eq!(rows[1].cpu_used.cell(quantity::cpu), "-");
        // The zero requests still read as a real zero beside the unknown usage.
        assert_eq!(rows[1].cpu_requested.cell(quantity::cpu), "0 (0%)");
    }

    // --- `usage_note`, the dashboard pane's reading of the same three cases -

    fn sampled(cpu: &str, memory: &str, seconds_ago: i64) -> Sample {
        Sample {
            usage: used(cpu, memory),
            taken_at: Some(now() - SignedDuration::from_secs(seconds_ago)),
            window: Some(SignedDuration::from_secs(20)),
        }
    }

    #[test]
    fn usage_note_dates_the_listing_when_the_columns_reached_the_rows() {
        let rows = [NodeRow::from_node(
            &healthy_node(),
            Some(&idle()),
            Some(used("392m", "1552515Ki")),
            now(),
        )];
        let samples = [Some(sampled("392m", "1552515Ki", 8))];

        let note = usage_note(&rows, &Ok(()), &samples, now(), "prod (us-east-1)");

        assert_eq!(
            note.as_deref(),
            Some("Usage is up to 8s old, averaged over 20s.")
        );
    }

    #[test]
    fn usage_note_for_an_unsampled_listing_is_the_bare_metrics_wording() {
        // The pane has no `CPU USE`/`MEM USE` headings to name, unlike the CLI
        // table's `usage_unsampled`, so this must not wrap the sentence in
        // language about columns the pane does not have.
        let rows = [NodeRow::from_node(
            &healthy_node(),
            Some(&idle()),
            None,
            now(),
        )];

        let note = usage_note(&rows, &Ok(()), &[None], now(), "prod (us-east-1)");

        assert_eq!(
            note.as_deref(),
            Some(metrics::unsampled("prod (us-east-1)").as_str())
        );
        assert!(!note.unwrap().contains("CPU USE"));
    }

    #[test]
    fn usage_note_is_silent_when_the_metrics_read_failed() {
        // Out of scope for this task: the pane has no footnote list yet to add
        // `usage_unavailable`'s explanation to, and the bars already read `-`.
        let rows = [NodeRow::from_node(
            &healthy_node(),
            Some(&idle()),
            None,
            now(),
        )];

        let note = usage_note(
            &rows,
            &Err("no metrics.k8s.io API".to_owned()),
            &[None],
            now(),
            "prod (us-east-1)",
        );

        assert_eq!(note, None);
    }

    // --- `usage_missing_explained`, the pane's reading of `order::Cause` ---

    #[test]
    fn a_shown_usage_column_is_not_missing() {
        let rows = [NodeRow::from_node(
            &healthy_node(),
            Some(&idle()),
            Some(used("392m", "1552515Ki")),
            now(),
        )];
        let note = usage_note(
            &rows,
            &Ok(()),
            &[Some(sampled("392m", "1552515Ki", 8))],
            now(),
            "prod (us-east-1)",
        );

        assert!(!usage_missing_explained(&rows, note.as_deref()));
    }

    #[test]
    fn an_unsampled_usage_column_is_missing_and_the_note_explains_it() {
        let rows = [NodeRow::from_node(
            &healthy_node(),
            Some(&idle()),
            None,
            now(),
        )];
        let note = usage_note(&rows, &Ok(()), &[None], now(), "prod (us-east-1)");

        assert!(usage_missing_explained(&rows, note.as_deref()));
    }

    #[test]
    fn a_failed_usage_read_is_missing_but_the_pane_says_nothing_about_it() {
        // The case the doc comment calls out: the pane has no footnote for a
        // failed read, so this must stay honestly `Unexplained` rather than
        // claiming a note that was never printed.
        let rows = [NodeRow::from_node(
            &healthy_node(),
            Some(&idle()),
            None,
            now(),
        )];
        let note = usage_note(
            &rows,
            &Err("no metrics.k8s.io API".to_owned()),
            &[None],
            now(),
            "prod (us-east-1)",
        );

        assert_eq!(note, None);
        assert!(!usage_missing_explained(&rows, note.as_deref()));
    }

    #[test]
    fn usage_columns_sit_beside_the_capacity_they_are_a_share_of() {
        let rows = [NodeRow::from_node(
            &healthy_node(),
            Some(&booked("1500m", "6Gi")),
            Some(used("392m", "1552515Ki")),
            now(),
        )];

        assert_eq!(
            render(
                &rows,
                "prod (us-east-1)",
                &[],
                Width::Default,
                Palette::Plain
            ),
            "NAME                      STATUS  VERSION              CPU      CPU REQ      CPU USE     MEMORY         MEM REQ    MEM USE      PODS         AGE\n\
             ip-10-0-1-9.ec2.internal  Ready   v1.33.1-eks-1a2b3c4  3920m/4  1500m (38%)  392m (10%)  14.8Gi/15.6Gi  6Gi (41%)  1.5Gi (10%)  12/58 (21%)  2d2h"
        );
    }

    #[test]
    fn a_footnote_explains_the_missing_usage_columns() {
        let rows = [NodeRow::from_node(
            &healthy_node(),
            Some(&idle()),
            None,
            now(),
        )];
        let note = usage_unavailable("prod (us-east-1) has no metrics.k8s.io API.");

        let output = render(
            &rows,
            "prod (us-east-1)",
            &[note],
            Width::Default,
            Palette::Plain,
        );
        let (table, footnote) = output
            .split_once("\n\n")
            .expect("a blank line before the note");

        assert_eq!(
            table,
            render(
                &rows,
                "prod (us-east-1)",
                &[],
                Width::Default,
                Palette::Plain
            )
        );
        assert!(footnote.contains("CPU USE"), "{footnote}");
        assert!(footnote.contains("metrics.k8s.io"), "{footnote}");
    }

    #[test]
    fn a_footnote_tells_an_unsampled_cluster_from_one_with_no_metrics_server() {
        // Same two missing columns, opposite advice: this cluster has
        // metrics-server, so telling the user to install it would send them off
        // to fix something that is not broken.
        let rows = [NodeRow::from_node(
            &healthy_node(),
            Some(&idle()),
            None,
            now(),
        )];
        let note = usage_unsampled(&crate::k8s::metrics::unsampled("prod (us-east-1)"));

        let output = render(
            &rows,
            "prod (us-east-1)",
            &[note],
            Width::Default,
            Palette::Plain,
        );
        let (table, footnote) = output
            .split_once("\n\n")
            .expect("a blank line before the note");

        assert_eq!(
            table,
            render(
                &rows,
                "prod (us-east-1)",
                &[],
                Width::Default,
                Palette::Plain
            )
        );
        assert!(footnote.contains("CPU USE"), "{footnote}");
        assert!(footnote.contains("MEM USE"), "{footnote}");
        assert!(footnote.contains("sampled"), "{footnote}");
        assert!(
            !footnote.contains("could not be read"),
            "the failed-read wording on a read that succeeded: {footnote}"
        );
    }

    #[test]
    fn a_sampled_listing_says_how_old_its_figures_are() {
        // The line that goes under every healthy table: a usage column with
        // nothing beside it cannot be told from an instantaneous reading, and
        // metrics-server going quiet never fails the request that asks it.
        let rows = [NodeRow::from_node(
            &healthy_node(),
            Some(&idle()),
            Some(used("392m", "1552515Ki")),
            now(),
        )];
        let sample = crate::k8s::metrics::Sample {
            usage: used("392m", "1552515Ki"),
            taken_at: Some(now() - SignedDuration::from_secs(12)),
            window: Some(SignedDuration::from_secs(20)),
        };
        let freshness = crate::k8s::metrics::freshness(&[sample], now())
            .expect("a stamped sample dates the listing");
        let note = crate::k8s::metrics::freshness_note(freshness);

        let output = render(
            &rows,
            "prod (us-east-1)",
            &[note],
            Width::Default,
            Palette::Plain,
        );

        assert!(
            output.ends_with("\n\nUsage is up to 12s old, averaged over 20s."),
            "{output}"
        );
    }

    #[test]
    fn two_footnotes_are_kept_apart_rather_than_run_together() {
        // A role that can read neither pods nor metrics gets both notes, and
        // they must not read as one paragraph.
        let rows = [NodeRow::from_node(&healthy_node(), None, None, now())];
        let notes = [
            requests_unavailable(&rows, "no pods for you"),
            usage_unavailable("no metrics for you"),
        ];

        let output = render(
            &rows,
            "prod (us-east-1)",
            &notes,
            Width::Default,
            Palette::Plain,
        );
        let paragraphs: Vec<&str> = output.split("\n\n").collect();

        assert_eq!(paragraphs.len(), 3, "{output}");
        assert!(paragraphs[1].contains("CPU REQ"), "{output}");
        assert!(paragraphs[2].contains("CPU USE"), "{output}");
    }

    #[test]
    fn a_footnote_explains_empty_request_columns() {
        let rows = [NodeRow::from_node(&healthy_node(), None, None, now())];
        let note = requests_unavailable(
            &rows,
            "prod (us-east-1) will not let you list this resource.",
        );

        let output = render(
            &rows,
            "prod (us-east-1)",
            &[note],
            Width::Default,
            Palette::Plain,
        );
        let (table, footnote) = output
            .split_once("\n\n")
            .expect("a blank line before the note");

        // The table is unchanged by the note; the note is what carries the news.
        assert_eq!(
            table,
            render(
                &rows,
                "prod (us-east-1)",
                &[],
                Width::Default,
                Palette::Plain
            )
        );
        assert!(footnote.contains("CPU REQ"), "{footnote}");
        assert!(footnote.contains("will not let you list"), "{footnote}");
    }

    #[test]
    fn the_sort_note_goes_under_the_table_with_the_footnotes() {
        // The renderer has no opinion about the note beyond where notes go;
        // this is the assertion that it lands under the table rather than
        // anywhere in it, and that the table above it is untouched.
        let rows = [NodeRow::from_node(&healthy_node(), None, None, now())];
        let notes = [
            requests_unavailable(&rows, "no pods for you"),
            crate::k8s::order::note(Order::Cpu, crate::k8s::order::Direction::Reversed)
                .expect("a reordered listing should say so"),
        ];

        let output = render(
            &rows,
            "prod (us-east-1)",
            &notes,
            Width::Default,
            Palette::Plain,
        );
        let paragraphs: Vec<&str> = output.split("\n\n").collect();

        assert_eq!(
            paragraphs[0],
            render(
                &rows,
                "prod (us-east-1)",
                &[],
                Width::Default,
                Palette::Plain
            )
        );
        assert_eq!(paragraphs[2], "Sorted by cpu, reversed.");
    }

    #[test]
    fn an_ordering_that_ranked_nothing_says_so_right_under_the_sort_note() {
        // `eks nodes --sort cpu` against a cluster with no metrics-server. The
        // usage footnote explains the missing columns; without the second note
        // nothing explains what became of the flag the user actually typed —
        // and the third line is the flag that would have worked on this table.
        // Two nodes, contrasting in status, requests, and age, so the advice
        // proves it named orderings that would have rearranged this listing
        // rather than merely ranked it.
        let rows = [
            NodeRow::from_node(&healthy_node(), Some(&idle()), None, now()),
            NodeRow::from_node(
                &contrasting_node(),
                Some(&booked("500m", "1Gi")),
                None,
                now(),
            ),
        ];
        let notes = sort_notes(&rows, Order::Cpu, no_usage());

        let output = render(
            &rows,
            "prod (us-east-1)",
            &notes,
            Width::Default,
            Palette::Plain,
        );
        let paragraphs: Vec<&str> = output.split("\n\n").collect();

        assert_eq!(paragraphs[1], "Sorted by cpu.");
        assert_eq!(
            paragraphs[2],
            "Nothing here has cpu to sort by, for the reason above.\n\
             Sort by status, cpu-requested, memory-requested, pods, or age instead."
        );
    }

    #[test]
    fn sorting_by_pods_after_a_failed_pod_listing_points_at_the_footnote() {
        // `eks nodes --sort pods` where the role grants nodes but not pods.
        // Every count is unknown, so the ordering ranked nothing — and the
        // request footnote above already says why, so the note points at it
        // rather than explaining the same failure twice. Two contrasting
        // nodes, so the suggested orderings are ones that would actually
        // reorder this listing rather than a single row with nowhere to go.
        let rows = [
            NodeRow::from_node(&healthy_node(), None, None, now()),
            NodeRow::from_node(&contrasting_node(), None, None, now()),
        ];
        let missing = Missing {
            requests: true,
            usage: true,
        };
        let notes = sort_notes(&rows, Order::Pods, missing);

        assert_eq!(
            notes,
            [
                "Sorted by pods.",
                "Nothing here has pods to sort by, for the reason above.\n\
                 Sort by status or age instead.",
            ]
        );
    }

    #[test]
    fn a_column_that_is_in_the_table_and_ranks_nothing_explains_itself() {
        // metrics-server answered and this node was sampled, so the `CPU USE`
        // column is right there — but the API server has not said what the node
        // can give out, so there is no share to rank by. Nothing above the table
        // is about that, so the note cannot lean on something above it.
        //
        // The listing where nothing at all was sampled no longer reaches this
        // branch: it now earns a footnote of its own, and `commands::nodes`
        // reports it as `Missing { usage: true }` so the note points upwards.
        let mut sampled_but_undescribed = healthy_node();
        if let Some(status) = sampled_but_undescribed.status.as_mut() {
            status.allocatable = None;
        }
        let mut other = contrasting_node();
        if let Some(status) = other.status.as_mut() {
            status.allocatable = None;
        }
        let rows = [
            NodeRow::from_node(
                &sampled_but_undescribed,
                Some(&idle()),
                Some(used("392m", "1552515Ki")),
                now(),
            ),
            NodeRow::from_node(&other, Some(&idle()), Some(used("100m", "256Mi")), now()),
        ];

        assert!(shows_usage(&rows), "the columns should be in the table");

        let notes = sort_notes(&rows, Order::Cpu, Missing::default());

        // The booked orderings rank by share too, so removing the denominator
        // takes them with it — which is exactly why the advice is computed from
        // the rows rather than listed out by hand. The second node contrasts in
        // status and age only, so those are the two orderings left standing.
        assert_eq!(
            notes[1],
            "Nothing here has cpu to sort by.\nSort by status or age instead."
        );
    }

    #[test]
    fn a_listing_nothing_sampled_points_at_the_footnote_that_now_explains_it() {
        // The other half of the pair above, and what changed: metrics-server
        // answered with nothing, the columns are gone, and there is now a
        // footnote saying so — so the note points at it rather than restating
        // the advice a paragraph later. Two contrasting nodes again, for the
        // same reason as the test above: the advice has to name orderings
        // that would rearrange this listing, not merely rank a lone row.
        let rows = [
            NodeRow::from_node(&healthy_node(), Some(&idle()), None, now()),
            NodeRow::from_node(
                &contrasting_node(),
                Some(&booked("500m", "1Gi")),
                None,
                now(),
            ),
        ];

        assert!(
            !shows_usage(&rows),
            "there is nothing to put in the columns"
        );

        let notes = sort_notes(&rows, Order::Cpu, no_usage());

        assert_eq!(
            notes[1],
            "Nothing here has cpu to sort by, for the reason above.\n\
             Sort by status, cpu-requested, memory-requested, pods, or age instead."
        );
    }

    #[test]
    fn an_ordering_no_footnote_could_explain_never_claims_one_did() {
        // Both fetches failed *and* the API server left out the creation
        // timestamps. The usage and request footnotes are above the table, but
        // neither of them is about `age`, so pointing at them would send the
        // user to read a paragraph that does not mention the column.
        // `Node::default()`: no creation timestamp, no allocatable, no status.
        // A second, equally bare node but with a real status condition, so
        // `status` has something to distinguish even though `age` never will.
        let reporting = Node {
            status: Some(NodeStatus {
                conditions: Some(vec![condition("Ready", "True")]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let rows = [
            NodeRow::from_node(&Node::default(), None, None, now()),
            NodeRow::from_node(&reporting, None, None, now()),
        ];
        let missing = Missing {
            requests: true,
            usage: true,
        };
        let notes = sort_notes(&rows, Order::Age, missing);

        assert_eq!(
            notes[1],
            "Nothing here has age to sort by.\nSort by status instead."
        );
    }

    #[test]
    fn a_listing_the_ordering_did_rank_gets_only_the_line_naming_it() {
        // One sampled node is enough: the busiest node the cluster knows about
        // is at the top, so there is nothing to apologise for.
        let rows = [NodeRow::from_node(
            &healthy_node(),
            Some(&idle()),
            Some(used("392m", "1552515Ki")),
            now(),
        )];

        assert_eq!(
            sort_notes(&rows, Order::Cpu, Missing::default()),
            ["Sorted by cpu."]
        );
    }

    #[test]
    fn the_default_listing_carries_neither_note() {
        // The guarantee that keeps `eks nodes` unchanged to the byte for
        // everyone who has never typed `--sort`.
        let rows = [NodeRow::from_node(
            &healthy_node(),
            Some(&idle()),
            None,
            now(),
        )];

        assert!(sort_notes(&rows, Order::default(), no_usage()).is_empty());
    }

    /// What the command reports when only the metrics read failed.
    fn no_usage() -> Missing {
        Missing {
            requests: false,
            usage: true,
        }
    }

    /// The pair of notes `commands::nodes` puts under the table, in its order
    /// and with the facts it hands them.
    fn sort_notes(rows: &[NodeRow], order: Order, missing: Missing) -> Vec<String> {
        let direction = crate::k8s::order::Direction::Natural;
        let mut notes = Vec::new();
        notes.extend(crate::k8s::order::note(order, direction));
        notes.extend(crate::k8s::order::unranked_note(
            order,
            crate::k8s::nodes::cause(order, missing),
            |candidate| crate::k8s::nodes::ranks_any(rows, candidate),
            |candidate| crate::k8s::nodes::distinguishes(rows, candidate),
        ));
        notes
    }

    #[test]
    fn a_cluster_with_no_nodes_says_nothing_about_the_order_it_would_have_been_in() {
        // `eks nodes --sort cpu` against a scaled-to-zero cluster: there is no
        // table for the note to be about, and the empty-cluster message is the
        // only thing worth reading.
        let note = crate::k8s::order::note(Order::Cpu, crate::k8s::order::Direction::Natural)
            .expect("a reordered listing should say so");
        let message = render(
            &[],
            "prod (us-east-1)",
            &[note],
            Width::Default,
            Palette::Plain,
        );

        assert!(!message.contains("Sorted by"), "{message}");
        assert!(message.contains("node groups"), "{message}");
    }

    #[test]
    fn a_cluster_with_no_nodes_skips_the_footnote() {
        // There is a bigger problem than a missing column to explain.
        let note = requests_unavailable(&[], "nope");
        let message = render(
            &[],
            "prod (us-east-1)",
            &[note],
            Width::Default,
            Palette::Plain,
        );

        assert!(!message.contains("CPU REQ"), "{message}");
        assert!(message.contains("node groups"), "{message}");
    }

    #[test]
    fn an_empty_cluster_explains_itself_instead_of_printing_a_bare_header() {
        let message = render(&[], "prod (us-east-1)", &[], Width::Default, Palette::Plain);

        assert!(message.contains("prod (us-east-1)"), "{message}");
        assert!(message.contains("node groups"), "{message}");
        assert!(!message.contains("NAME"), "{message}");
    }

    #[test]
    fn a_row_carries_the_instant_behind_its_age_as_well_as_the_text() {
        // `--sort age` ranks on this rather than on the rendered string: two
        // nodes can both read `3d` and be nearly a day apart.
        let joined = ago(50);
        let node = healthy_node();

        let row = NodeRow::from_node(&node, Some(&idle()), None, now());

        assert_eq!(row.created_at, Some(joined.0));
        assert_eq!(row.age, "2d2h");
    }

    #[test]
    fn a_node_the_api_server_gave_no_creation_time_has_no_instant_either() {
        let mut node = healthy_node();
        node.metadata.creation_timestamp = None;

        let row = NodeRow::from_node(&node, Some(&idle()), None, now());

        assert_eq!(row.created_at, None);
        assert_eq!(row.age, "-");
    }

    /// The `healthy_node` fixture with the fields only `--wide` shows filled
    /// in, as a real EKS node reports them.
    fn wide_node() -> Node {
        let mut node = healthy_node();
        if let Some(status) = node.status.as_mut() {
            status.addresses = Some(vec![
                address("InternalIP", "10.0.1.9"),
                address("Hostname", "ip-10-0-1-9.ec2.internal"),
            ]);
            status.node_info = Some(NodeSystemInfo {
                kubelet_version: "v1.33.1-eks-1a2b3c4".to_owned(),
                os_image: "Amazon Linux 2023.9.20260714".to_owned(),
                kernel_version: "6.1.148-172.265.amzn2023.x86_64".to_owned(),
                container_runtime_version: "containerd://1.7.28".to_owned(),
                ..Default::default()
            });
        }
        node
    }

    fn address(kind: &str, value: &str) -> NodeAddress {
        NodeAddress {
            type_: kind.to_owned(),
            address: value.to_owned(),
        }
    }

    #[test]
    fn the_default_node_table_holds_the_wide_columns_back() {
        let rows = [NodeRow::from_node(&wide_node(), Some(&idle()), None, now())];

        let table = render(
            &rows,
            "prod (us-east-1)",
            &[],
            Width::Default,
            Palette::Plain,
        );
        for held_back in [
            "INTERNAL-IP",
            "EXTERNAL-IP",
            "OS-IMAGE",
            "KERNEL-VERSION",
            "CONTAINER-RUNTIME",
        ] {
            assert!(!table.contains(held_back), "{held_back} in {table}");
        }
    }

    #[test]
    fn wide_adds_the_addresses_the_ami_the_kernel_and_the_runtime() {
        let rows = [NodeRow::from_node(&wide_node(), Some(&idle()), None, now())];

        assert_eq!(
            render(&rows, "prod (us-east-1)", &[], Width::Wide, Palette::Plain),
            "NAME                      STATUS  VERSION              CPU      CPU REQ  MEMORY         MEM REQ  PODS       AGE   INTERNAL-IP  EXTERNAL-IP  OS-IMAGE                      KERNEL-VERSION                   CONTAINER-RUNTIME\n\
             ip-10-0-1-9.ec2.internal  Ready   v1.33.1-eks-1a2b3c4  3920m/4  0 (0%)   14.8Gi/15.6Gi  0 (0%)   0/58 (0%)  2d2h  10.0.1.9     -            Amazon Linux 2023.9.20260714  6.1.148-172.265.amzn2023.x86_64  containerd://1.7.28"
        );
    }

    #[test]
    fn the_default_node_table_is_the_wide_one_with_its_tail_cut_off() {
        // Someone comparing a wide listing against a plain one should not have
        // to re-find the columns they were already reading, so the wide columns
        // go on the end rather than beside VERSION where `kubectl` puts them.
        let rows = [NodeRow::from_node(&wide_node(), Some(&idle()), None, now())];

        let narrow = columns(&rows, Width::Default);
        let wide = columns(&rows, Width::Wide);

        assert_eq!(wide[..narrow.len()], narrow[..]);
        assert_eq!(
            wide[narrow.len()..],
            [
                Column::InternalIp,
                Column::ExternalIp,
                Column::OsImage,
                Column::KernelVersion,
                Column::ContainerRuntime,
            ]
        );
    }

    #[test]
    fn the_wide_columns_still_follow_the_usage_ones_when_both_are_shown() {
        // The two conditions compose: `--wide` must not disturb where CPU USE
        // and MEM USE sit beside the capacities they are a share of.
        let rows = [NodeRow::from_node(
            &wide_node(),
            Some(&idle()),
            Some(used("392m", "1552515Ki")),
            now(),
        )];

        let headers: Vec<String> = columns(&rows, Width::Wide)
            .iter()
            .map(|column| column.header())
            .collect();

        assert_eq!(
            headers,
            [
                "NAME",
                "STATUS",
                "VERSION",
                "CPU",
                "CPU REQ",
                "CPU USE",
                "MEMORY",
                "MEM REQ",
                "MEM USE",
                "PODS",
                "AGE",
                "INTERNAL-IP",
                "EXTERNAL-IP",
                "OS-IMAGE",
                "KERNEL-VERSION",
                "CONTAINER-RUNTIME",
            ]
        );
    }

    #[test]
    fn a_public_node_shows_the_address_a_private_one_has_none_of() {
        let mut node = wide_node();
        if let Some(status) = node.status.as_mut() {
            status.addresses = Some(vec![
                address("InternalIP", "10.0.1.9"),
                address("ExternalIP", "54.72.9.14"),
            ]);
        }

        let row = NodeRow::from_node(&node, Some(&idle()), None, now());
        assert_eq!(row.internal_ip, "10.0.1.9");
        assert_eq!(row.external_ip, "54.72.9.14");
    }

    #[test]
    fn the_first_address_of_a_type_is_the_one_shown() {
        // A node on two subnets reports several, and the API server lists the
        // primary one first — as `kubectl` relies on too.
        let mut node = wide_node();
        if let Some(status) = node.status.as_mut() {
            status.addresses = Some(vec![
                address("InternalIP", "10.0.1.9"),
                address("InternalIP", "10.0.2.9"),
            ]);
        }

        assert_eq!(
            NodeRow::from_node(&node, Some(&idle()), None, now()).internal_ip,
            "10.0.1.9"
        );
    }

    #[test]
    fn a_node_that_has_reported_nothing_yet_shows_dashes_not_blanks() {
        // A node caught mid-registration has no addresses and no nodeInfo at
        // all, and the `nodeInfo` strings are not optional in the API — "not
        // reported" arrives as an empty string, which would render as a gap
        // that reads like a bug.
        let row = NodeRow::from_node(&Node::default(), Some(&idle()), None, now());

        assert_eq!(row.internal_ip, "-");
        assert_eq!(row.external_ip, "-");
        assert_eq!(row.os_image, "-");
        assert_eq!(row.kernel_version, "-");
        assert_eq!(row.container_runtime, "-");

        let table = render(&[row], "prod (us-east-1)", &[], Width::Wide, Palette::Plain);
        assert!(table.contains("CONTAINER-RUNTIME"), "{table}");
    }

    #[test]
    fn an_empty_wide_listing_still_says_where_the_nodes_went() {
        // `--wide` changes columns, and there are no columns here to change.
        assert_eq!(
            render(&[], "prod (us-east-1)", &[], Width::Wide, Palette::Plain),
            render(&[], "prod (us-east-1)", &[], Width::Default, Palette::Plain)
        );
    }

    #[test]
    fn a_wide_table_keeps_its_footnotes() {
        let rows = [NodeRow::from_node(&wide_node(), Some(&idle()), None, now())];
        let note = "Sorted by cpu.".to_owned();

        let output = render(
            &rows,
            "prod (us-east-1)",
            std::slice::from_ref(&note),
            Width::Wide,
            Palette::Plain,
        );

        assert!(output.ends_with(&format!("\n\n{note}")), "{output}");
    }

    // --- Extended resources -------------------------------------------------

    /// A GPU node group's node: four cards, and the `hugepages` entry every
    /// real node carries beside them.
    fn gpu_node() -> Node {
        let mut node = healthy_node();
        if let Some(status) = node.status.as_mut() {
            status.capacity = Some(quantities(&[
                ("cpu", "4"),
                ("memory", "16374624Ki"),
                ("pods", "58"),
                ("hugepages-2Mi", "0"),
                ("nvidia.com/gpu", "4"),
            ]));
            status.allocatable = Some(quantities(&[
                ("cpu", "3920m"),
                ("memory", "15525152Ki"),
                ("pods", "58"),
                ("hugepages-2Mi", "0"),
                ("nvidia.com/gpu", "4"),
            ]));
        }
        node
    }

    fn renamed(node: &Node, name: &str) -> Node {
        let mut node = node.clone();
        node.metadata.name = Some(name.to_owned());
        node
    }

    /// A node whose device plugin has marked some cards unhealthy: it reports
    /// `held` and will hand out only `offered`.
    fn withholding_node(name: &str, offered: &str, held: &str) -> Node {
        let mut node = renamed(&gpu_node(), name);
        if let Some(status) = node.status.as_mut() {
            status.capacity = Some(quantities(&[
                ("cpu", "4"),
                ("memory", "16374624Ki"),
                ("pods", "58"),
                ("nvidia.com/gpu", held),
            ]));
            status.allocatable = Some(quantities(&[
                ("cpu", "3920m"),
                ("memory", "15525152Ki"),
                ("pods", "58"),
                ("nvidia.com/gpu", offered),
            ]));
        }
        node
    }

    fn booked_device(resource: &str, count: &str) -> Placed {
        let base = booked("1", "1Gi");
        Placed {
            requests: Requests {
                extended: [(
                    resource.to_owned(),
                    Quantity::parse(count).unwrap_or_default(),
                )]
                .into_iter()
                .collect(),
                ..base.requests
            },
            ..base
        }
    }

    fn headings(rows: &[NodeRow], width: Width) -> Vec<String> {
        columns(rows, width)
            .iter()
            .map(|column| column.header())
            .collect()
    }

    fn device_cells(rows: &[NodeRow], resource: &str) -> Vec<String> {
        rows.iter()
            .map(|row| Column::Device(resource).text(row))
            .collect()
    }

    #[test]
    fn a_device_plugin_resource_earns_a_column_of_its_own() {
        let rows = [NodeRow::from_node(&gpu_node(), Some(&idle()), None, now())];

        assert_eq!(
            headings(&rows, Width::Default),
            [
                "NAME",
                "STATUS",
                "VERSION",
                "CPU",
                "CPU REQ",
                "MEMORY",
                "MEM REQ",
                "PODS",
                "NVIDIA.COM/GPU",
                "AGE",
            ]
        );
    }

    #[test]
    fn a_cluster_with_no_devices_gains_no_columns() {
        // The whole point of the condition: an EKS cluster of m5.xlarges must
        // print exactly the table it printed before.
        let rows = [NodeRow::from_node(
            &healthy_node(),
            Some(&idle()),
            None,
            now(),
        )];

        assert_eq!(
            headings(&rows, Width::Default),
            [
                "NAME", "STATUS", "VERSION", "CPU", "CPU REQ", "MEMORY", "MEM REQ", "PODS", "AGE"
            ]
        );
        assert!(rows[0].devices.is_empty());
    }

    #[test]
    fn the_resources_kubernetes_defines_itself_never_become_device_columns() {
        // `hugepages-2Mi` sits in the same capacity map and is not a device; a
        // column of zeroes headed `HUGEPAGES-2MI` on every node would be the
        // noise this condition exists to avoid.
        let rows = [NodeRow::from_node(&gpu_node(), Some(&idle()), None, now())];
        let table = render(
            &rows,
            "prod (us-east-1)",
            &[],
            Width::Default,
            Palette::Plain,
        );

        assert!(!table.contains("HUGEPAGES"), "{table}");
        assert_eq!(rows[0].devices.len(), 1, "{:?}", rows[0].devices);
    }

    #[test]
    fn a_device_cell_shows_what_is_booked_out_of_what_is_offered() {
        // The two numbers the question turns on: a pod asking for one more GPU
        // fits here, and would not if this read 4/4.
        let rows = [NodeRow::from_node(
            &gpu_node(),
            Some(&booked_device("nvidia.com/gpu", "2")),
            None,
            now(),
        )];

        assert_eq!(device_cells(&rows, "nvidia.com/gpu"), ["2/4 (50%)"]);
    }

    #[test]
    fn a_device_nobody_has_asked_for_reads_as_a_real_zero() {
        let rows = [NodeRow::from_node(&gpu_node(), Some(&idle()), None, now())];

        assert_eq!(device_cells(&rows, "nvidia.com/gpu"), ["0/4 (0%)"]);
    }

    #[test]
    fn a_column_appears_for_a_device_only_some_nodes_have() {
        // A cluster with one GPU node group and one without. The CPU node reads
        // `-` rather than `0/0`: it has no such hardware, which is a different
        // answer from having none of it free.
        let rows = [
            NodeRow::from_node(
                &renamed(&gpu_node(), "gpu-node"),
                Some(&idle()),
                None,
                now(),
            ),
            NodeRow::from_node(
                &renamed(&healthy_node(), "cpu-node"),
                Some(&idle()),
                None,
                now(),
            ),
        ];

        assert!(headings(&rows, Width::Default).contains(&"NVIDIA.COM/GPU".to_owned()));
        assert_eq!(device_cells(&rows, "nvidia.com/gpu"), ["0/4 (0%)", "-"]);
    }

    #[test]
    fn two_device_resources_get_a_column_each_in_name_order() {
        // A mixed-vendor cluster is exactly why the heading keeps the domain:
        // `GPU` over either column would be ambiguous over the other.
        let mut node = renamed(&gpu_node(), "mixed-node");
        if let Some(status) = node.status.as_mut() {
            status.capacity = Some(quantities(&[
                ("cpu", "4"),
                ("memory", "16374624Ki"),
                ("nvidia.com/gpu", "4"),
                ("amd.com/gpu", "2"),
            ]));
            status.allocatable = Some(quantities(&[
                ("cpu", "3920m"),
                ("memory", "15525152Ki"),
                ("nvidia.com/gpu", "4"),
                ("amd.com/gpu", "2"),
            ]));
        }
        let rows = [NodeRow::from_node(&node, Some(&idle()), None, now())];

        let headings = headings(&rows, Width::Default);
        let devices: Vec<&String> = headings
            .iter()
            .filter(|heading| heading.contains('/'))
            .collect();
        assert_eq!(devices, ["AMD.COM/GPU", "NVIDIA.COM/GPU"]);
    }

    #[test]
    fn the_device_columns_sit_after_the_usage_ones_and_before_age() {
        // The block of "what this machine can give out" stays together, and AGE
        // stays last of the default columns where every `kubectl get` puts it.
        let rows = [NodeRow::from_node(
            &gpu_node(),
            Some(&idle()),
            Some(used("392m", "1552515Ki")),
            now(),
        )];

        assert_eq!(
            headings(&rows, Width::Default),
            [
                "NAME",
                "STATUS",
                "VERSION",
                "CPU",
                "CPU REQ",
                "CPU USE",
                "MEMORY",
                "MEM REQ",
                "MEM USE",
                "PODS",
                "NVIDIA.COM/GPU",
                "AGE",
            ]
        );
    }

    #[test]
    fn the_wide_tail_still_follows_the_device_columns() {
        // Three conditions composing at once, which is the arrangement most
        // likely to put a figure under the wrong heading.
        let mut node = wide_node();
        if let Some(status) = node.status.as_mut() {
            status.capacity = Some(quantities(&[("cpu", "4"), ("nvidia.com/gpu", "4")]));
            status.allocatable = Some(quantities(&[("cpu", "3920m"), ("nvidia.com/gpu", "4")]));
        }
        let rows = [NodeRow::from_node(&node, Some(&idle()), None, now())];

        let narrow = columns(&rows, Width::Default);
        let wide = columns(&rows, Width::Wide);

        assert_eq!(wide[..narrow.len()], narrow[..]);
        assert!(narrow.contains(&Column::Device("nvidia.com/gpu")));
        assert_eq!(
            wide[narrow.len()..],
            [
                Column::InternalIp,
                Column::ExternalIp,
                Column::OsImage,
                Column::KernelVersion,
                Column::ContainerRuntime,
            ]
        );
    }

    #[test]
    fn a_gpu_table_lines_up_under_its_headings() {
        // The whole table, to the byte: a device column is the first one whose
        // heading is wider than its cells, and the alignment is what a reader
        // uses to tell which number belongs to which node.
        let rows = [NodeRow::from_node(
            &gpu_node(),
            Some(&booked_device("nvidia.com/gpu", "2")),
            None,
            now(),
        )];

        assert_eq!(
            render(
                &rows,
                "prod (us-east-1)",
                &[],
                Width::Default,
                Palette::Plain
            ),
            "NAME                      STATUS  VERSION              CPU      CPU REQ  MEMORY         MEM REQ   PODS         NVIDIA.COM/GPU  AGE\n\
             ip-10-0-1-9.ec2.internal  Ready   v1.33.1-eks-1a2b3c4  3920m/4  1 (26%)  14.8Gi/15.6Gi  1Gi (7%)  12/58 (21%)  2/4 (50%)       2d2h"
        );
    }

    #[test]
    fn a_failed_pod_listing_leaves_the_device_cell_without_a_numerator() {
        // The count came back with the nodes and is still good; only what is
        // booked of it is unknown, so the cell keeps the half it has.
        let rows = [NodeRow::from_node(&gpu_node(), None, None, now())];

        assert_eq!(device_cells(&rows, "nvidia.com/gpu"), ["-/4"]);
    }

    #[test]
    fn the_request_footnote_names_the_device_column_it_emptied() {
        let rows = [NodeRow::from_node(&gpu_node(), None, None, now())];

        let note = requests_unavailable(&rows, "prod (us-east-1) will not let you list pods.");

        assert!(
            note.starts_with(
                "CPU REQ, MEM REQ, and the booked half of PODS and NVIDIA.COM/GPU are empty \
                 because"
            ),
            "{note}"
        );
        assert!(note.contains("will not let you list pods"), "{note}");
    }

    #[test]
    fn the_request_footnote_is_unchanged_where_there_are_no_devices() {
        // The overwhelmingly common cluster must read exactly as it did.
        let rows = [NodeRow::from_node(&healthy_node(), None, None, now())];

        assert_eq!(
            requests_unavailable(&rows, "why"),
            "CPU REQ, MEM REQ, and the booked half of PODS are empty \
             because the pods could not be listed.\nwhy"
        );
    }

    #[test]
    fn a_node_offering_none_of_a_device_it_reports_shows_no_percentage() {
        // Every card unhealthy, or a plugin that has registered and found
        // nothing: `0/0` rather than a share of nothing.
        let rows = [NodeRow::from_node(
            &withholding_node("gpu-node", "0", "0"),
            Some(&idle()),
            None,
            now(),
        )];

        assert_eq!(device_cells(&rows, "nvidia.com/gpu"), ["0/0"]);
    }

    #[test]
    fn a_device_reported_only_as_capacity_stands_without_a_share() {
        // A node caught mid-registration, which has reported one map and not
        // the other. There is nothing to be a share of, so the figure is alone
        // rather than divided by a number nobody has.
        let mut node = renamed(&gpu_node(), "registering");
        if let Some(status) = node.status.as_mut() {
            status.allocatable = Some(quantities(&[("cpu", "3920m")]));
        }
        let rows = [NodeRow::from_node(
            &node,
            Some(&booked_device("nvidia.com/gpu", "1")),
            None,
            now(),
        )];

        assert!(headings(&rows, Width::Default).contains(&"NVIDIA.COM/GPU".to_owned()));
        assert_eq!(device_cells(&rows, "nvidia.com/gpu"), ["1"]);
    }

    #[test]
    fn a_node_holding_back_devices_it_has_earns_a_footnote() {
        // The gap is invisible in a cell that shows what can be handed out, and
        // it is the reason a GPU pod that should fit will not schedule.
        let rows = [NodeRow::from_node(
            &withholding_node("gpu-node", "3", "4"),
            Some(&idle()),
            None,
            now(),
        )];

        let note = devices_withheld(&rows).expect("a withheld device should be footnoted");

        assert!(
            note.starts_with("gpu-node offers 3 of the 4 nvidia.com/gpu it reports."),
            "{note}"
        );
        // Diagnosis is not enough: the note has to say where to look and what
        // happens if nobody does.
        assert!(note.contains("device-plugin"), "{note}");
        assert!(note.contains("Pending"), "{note}");
        // And the cell itself is unchanged — allocatable is still the number
        // that decides whether the next pod fits.
        assert_eq!(device_cells(&rows, "nvidia.com/gpu"), ["0/3 (0%)"]);
    }

    #[test]
    fn a_node_offering_everything_it_has_earns_no_footnote() {
        let rows = [NodeRow::from_node(&gpu_node(), Some(&idle()), None, now())];

        assert_eq!(devices_withheld(&rows), None);
        // And a listing with no nodes at all has nothing to say either.
        assert_eq!(devices_withheld(&[]), None);
    }

    #[test]
    fn the_withheld_footnote_names_the_worst_node_and_counts_the_others() {
        // Two sick nodes and a healthy one. The named node is the one worth
        // walking to, not the first one alphabetically, and the count is of
        // machines to visit rather than of devices lost.
        let rows = [
            NodeRow::from_node(
                &withholding_node("almost-fine", "3", "4"),
                Some(&idle()),
                None,
                now(),
            ),
            NodeRow::from_node(
                &withholding_node("badly-broken", "0", "8"),
                Some(&idle()),
                None,
                now(),
            ),
            NodeRow::from_node(&renamed(&gpu_node(), "healthy"), Some(&idle()), None, now()),
        ];

        let note = devices_withheld(&rows).expect("two withheld devices should be footnoted");

        assert!(
            note.starts_with(
                "2 nodes offer fewer devices than they report, including badly-broken, which \
                 offers 0 of the 8 nvidia.com/gpu it has."
            ),
            "{note}"
        );
    }

    #[test]
    fn a_devices_share_divides_by_what_the_node_will_hand_out() {
        // The same denominator the CPU and memory columns use, so a percentage
        // means one thing across a row.
        let rows = [NodeRow::from_node(
            &withholding_node("gpu-node", "4", "8"),
            Some(&booked_device("nvidia.com/gpu", "2")),
            None,
            now(),
        )];
        let device = rows[0].devices["nvidia.com/gpu"];

        assert_eq!(device.share().ratio(), Some(0.5));
        assert_eq!(
            device.withheld(),
            Some((
                Quantity::parse("4").unwrap_or_default(),
                Quantity::parse("8").unwrap_or_default()
            ))
        );
    }

    #[test]
    fn a_device_whose_quantity_will_not_parse_is_still_a_column() {
        // `Quantity::lookup` drops what it cannot read and logs it, so the
        // count is unknown while the resource is not. Dropping the column
        // instead would answer "does this node have GPUs?" with silence.
        let mut node = renamed(&gpu_node(), "odd-node");
        if let Some(status) = node.status.as_mut() {
            status.capacity = Some(quantities(&[("cpu", "4"), ("nvidia.com/gpu", "four")]));
            status.allocatable = Some(quantities(&[("cpu", "4"), ("nvidia.com/gpu", "four")]));
        }
        let rows = [NodeRow::from_node(&node, Some(&idle()), None, now())];

        assert!(headings(&rows, Width::Default).contains(&"NVIDIA.COM/GPU".to_owned()));
        assert_eq!(device_cells(&rows, "nvidia.com/gpu"), ["0"]);
        // Nothing to compare, so nothing is accused of holding cards back.
        assert_eq!(devices_withheld(&rows), None);
    }

    #[test]
    fn a_node_with_no_status_at_all_reports_no_devices() {
        let row = NodeRow::from_node(&Node::default(), Some(&idle()), None, now());

        assert!(row.devices.is_empty());
        assert_eq!(devices_withheld(std::slice::from_ref(&row)), None);
        assert_eq!(
            requests_unavailable(std::slice::from_ref(&row), "why"),
            "CPU REQ, MEM REQ, and the booked half of PODS are empty \
             because the pods could not be listed.\nwhy"
        );
    }

    // --- Ephemeral storage and huge pages ------------------------------------

    /// A node with ephemeral storage reported and one huge-page size actually
    /// reserved, beside the `hugepages-1Gi` entry every real node carries at
    /// zero regardless.
    fn storage_node() -> Node {
        let mut node = healthy_node();
        if let Some(status) = node.status.as_mut() {
            status.capacity = Some(quantities(&[
                ("cpu", "4"),
                ("memory", "16374624Ki"),
                ("pods", "58"),
                ("ephemeral-storage", "104857600Ki"),
                ("hugepages-1Gi", "0"),
                ("hugepages-2Mi", "20Mi"),
            ]));
            status.allocatable = Some(quantities(&[
                ("cpu", "3920m"),
                ("memory", "15525152Ki"),
                ("pods", "58"),
                ("ephemeral-storage", "94371840Ki"),
                ("hugepages-1Gi", "0"),
                ("hugepages-2Mi", "20Mi"),
            ]));
        }
        node
    }

    #[test]
    fn ephemeral_storage_earns_a_capacity_column_like_memorys() {
        let rows = [NodeRow::from_node(
            &storage_node(),
            Some(&idle()),
            None,
            now(),
        )];

        assert!(
            headings(&rows, Width::Default).contains(&"EPHEMERAL-STORAGE".to_owned()),
            "{:?}",
            headings(&rows, Width::Default)
        );
        // 94371840Ki is exactly 90Gi, 104857600Ki exactly 100Gi — allocatable
        // over capacity, the same order the CPU and MEMORY cells use.
        assert_eq!(
            Column::EphemeralStorage.text(&rows[0]),
            "90Gi/100Gi".to_owned()
        );
    }

    #[test]
    fn a_node_with_no_ephemeral_storage_gains_no_column() {
        // The overwhelmingly common cluster must read exactly as it did.
        let rows = [NodeRow::from_node(
            &healthy_node(),
            Some(&idle()),
            None,
            now(),
        )];

        assert!(
            !headings(&rows, Width::Default).contains(&"EPHEMERAL-STORAGE".to_owned()),
            "{:?}",
            headings(&rows, Width::Default)
        );
        assert_eq!(rows[0].ephemeral_storage, Capacity::default());
    }

    #[test]
    fn only_a_reserved_hugepage_pool_earns_a_column() {
        // `hugepages-1Gi` is on this node's own capacity map, at zero, exactly
        // like `hugepages-2Mi` was on `gpu_node` — and stays as invisible as
        // that one did. Only the size somebody actually reserved gets a
        // column.
        let rows = [NodeRow::from_node(
            &storage_node(),
            Some(&idle()),
            None,
            now(),
        )];
        let headings = headings(&rows, Width::Default);

        assert!(
            headings.contains(&"HUGEPAGES-2MI".to_owned()),
            "{headings:?}"
        );
        assert!(
            !headings.contains(&"HUGEPAGES-1GI".to_owned()),
            "{headings:?}"
        );
        assert_eq!(rows[0].hugepages.len(), 2, "{:?}", rows[0].hugepages);
    }

    #[test]
    fn a_hugepages_cell_reads_as_a_capacity_pair_not_a_device_count() {
        let rows = [NodeRow::from_node(
            &storage_node(),
            Some(&idle()),
            None,
            now(),
        )];

        // 20Mi reserved and fully handed out — the pool's own shape, not
        // `nvidia.com/gpu`'s `booked/offered` one.
        assert_eq!(
            Column::Hugepage("hugepages-2Mi").text(&rows[0]),
            "20Mi/20Mi"
        );
    }

    #[test]
    fn a_node_reporting_none_of_a_size_reads_a_dash_not_a_real_zero() {
        // Mirrors the device table's own rule: a size this node never listed
        // at all is different from a pool it listed and reserved nothing of.
        let rows = [
            NodeRow::from_node(
                &renamed(&storage_node(), "gpu-storage"),
                Some(&idle()),
                None,
                now(),
            ),
            NodeRow::from_node(
                &renamed(&healthy_node(), "plain-node"),
                Some(&idle()),
                None,
                now(),
            ),
        ];

        assert_eq!(Column::Hugepage("hugepages-2Mi").text(&rows[1]), "-");
        assert_eq!(Column::Hugepage("hugepages-2Mi").severity(&rows[1]), None);
    }

    #[test]
    fn ephemeral_storage_and_hugepages_sit_between_pods_and_age() {
        let rows = [NodeRow::from_node(
            &storage_node(),
            Some(&idle()),
            None,
            now(),
        )];

        assert_eq!(
            headings(&rows, Width::Default),
            [
                "NAME",
                "STATUS",
                "VERSION",
                "CPU",
                "CPU REQ",
                "MEMORY",
                "MEM REQ",
                "PODS",
                "EPHEMERAL-STORAGE",
                "HUGEPAGES-2MI",
                "AGE",
            ]
        );
    }

    #[test]
    fn a_narrow_terminal_drops_ephemeral_storage_and_hugepages_before_version() {
        // The newest and least essential facts on the row go first, ahead of
        // even VERSION — they were never visible before tonight, and a
        // reviewer used to the old table should see it unchanged until the
        // terminal is genuinely tight.
        let rows = [NodeRow::from_node(
            &storage_node(),
            Some(&idle()),
            None,
            now(),
        )];
        let wide_enough = headings(&rows, Width::Narrow(200));
        assert!(wide_enough.contains(&"EPHEMERAL-STORAGE".to_owned()));
        assert!(wide_enough.contains(&"HUGEPAGES-2MI".to_owned()));

        // Narrow enough to force exactly one drop step; VERSION is still here
        // and the two new columns are already gone.
        let target = u16::try_from(format::row_width(&widths(
            &columns(&rows, Width::Default),
            &rows,
        )))
        .unwrap_or(u16::MAX)
            - 1;
        let cols = headings_at(&rows, target);
        assert!(!cols.contains(&"EPHEMERAL-STORAGE".to_owned()), "{cols:?}");
        assert!(!cols.contains(&"HUGEPAGES-2MI".to_owned()), "{cols:?}");
        assert!(cols.contains(&"VERSION".to_owned()), "{cols:?}");
    }

    // --- Narrow mode --------------------------------------------------------

    /// One default-shape row: healthy, booked, no metrics-server. The width
    /// tests want a fixture whose row width is predictable rather than the
    /// widest cluster we can build, so a single node is enough.
    fn one_booked_row() -> Vec<NodeRow> {
        vec![NodeRow::from_node(
            &healthy_node(),
            Some(&booked("1500m", "6Gi")),
            None,
            now(),
        )]
    }

    fn headings_at(rows: &[NodeRow], target: u16) -> Vec<String> {
        columns(rows, Width::Narrow(target))
            .iter()
            .map(|column| column.header())
            .collect()
    }

    #[test]
    fn a_wide_enough_narrow_is_the_default_table_byte_for_byte() {
        // A resize that leaves the row still fitting must not shuffle it —
        // narrowing is subtraction, and subtracting nothing changes nothing.
        // 200 cols is roomier than any table this file produces.
        let rows = one_booked_row();
        assert_eq!(
            columns(&rows, Width::Narrow(200)),
            columns(&rows, Width::Default)
        );
        assert_eq!(
            render(
                &rows,
                "prod (us-east-1)",
                &[],
                Width::Narrow(200),
                Palette::Plain
            ),
            render(
                &rows,
                "prod (us-east-1)",
                &[],
                Width::Default,
                Palette::Plain
            ),
        );
    }

    #[test]
    fn a_narrow_terminal_drops_version_first() {
        // The default row is 120 chars for the fixture — `NAME(24) STATUS(6)
        // VERSION(19) CPU(7) CPU REQ(11) MEMORY(13) MEM REQ(9) PODS(11)
        // AGE(4)` with eight two-space separators — and dropping just
        // `VERSION` gets it to 99. `kubectl` prints VERSION too, so a person
        // on a narrow terminal asking for it has somewhere to go; it is the
        // right first thing to let go.
        assert_eq!(
            headings_at(&one_booked_row(), 100),
            [
                "NAME", "STATUS", "CPU", "CPU REQ", "MEMORY", "MEM REQ", "PODS", "AGE"
            ],
        );
    }

    #[test]
    fn the_pod_count_is_the_first_of_the_booked_figures_to_go() {
        // Three columns answer "will another pod fit here", and this is the
        // order they leave in. PODS goes first because it is the least often
        // the binding one — and because a column added later must not be what
        // evicts `CPU REQ` and `MEM REQ` from a listing that has been keeping
        // them. 90 is past `VERSION` and `AGE` and into `PODS`.
        let headings = headings_at(&one_booked_row(), 90);

        assert!(
            !headings.iter().any(|heading| heading == "PODS"),
            "{headings:?}"
        );
        assert_eq!(
            headings,
            ["NAME", "STATUS", "CPU", "CPU REQ", "MEMORY", "MEM REQ"]
        );
    }

    #[test]
    fn eighty_columns_still_keep_the_requests_beside_the_capacities() {
        // 80 cols is the width every laptop lid narrows to under a docked
        // browser: `VERSION` and `AGE` both leave, and the pair columns
        // survive the drop that matters — the request beside the capacity it
        // is a share of. AGE is the standard column but not the one a person
        // narrowing a listing came for.
        let rows = one_booked_row();
        assert_eq!(
            headings_at(&rows, 80),
            ["NAME", "STATUS", "CPU", "CPU REQ", "MEMORY", "MEM REQ"],
        );
        // And the columns it reported really do fit: the assertion is over the
        // rendered table rather than over the arithmetic that chose it, so a
        // drop rule measuring rows the renderer disagreed with would fail here.
        for line in render(
            &rows,
            "prod (us-east-1)",
            &[],
            Width::Narrow(80),
            Palette::Plain,
        )
        .lines()
        {
            assert!(line.chars().count() <= 80, "{line:?} is wider than 80");
        }
    }

    #[test]
    fn a_row_narrower_than_the_name_still_prints_the_name() {
        // At `--width 1` — the acceptance-test extreme — every column but NAME
        // is dropped, and NAME stays even though the row is still wider than
        // one character. The alternative — dropping NAME too — is a row a
        // person cannot read at all.
        assert_eq!(headings_at(&one_booked_row(), 1), ["NAME"]);
    }

    #[test]
    fn the_pair_columns_leave_together_rather_than_singly() {
        // `CPU REQ` next to `MEMORY` without `MEM REQ` reads as noise — the
        // eye pairs it with the wrong number — so the two go together, and
        // the same for the USE pair and for CPU + MEMORY themselves.
        let rows = one_booked_row();

        // 70 cols forces a drop past just VERSION; both REQ columns leave.
        let cols = headings_at(&rows, 70);
        assert!(!cols.contains(&"CPU REQ".to_owned()), "{cols:?}");
        assert!(!cols.contains(&"MEM REQ".to_owned()), "{cols:?}");

        // A width small enough to lose CPU also loses MEMORY, never one alone.
        let cols = headings_at(&rows, 30);
        assert!(!cols.contains(&"CPU".to_owned()), "{cols:?}");
        assert!(!cols.contains(&"MEMORY".to_owned()), "{cols:?}");
    }

    #[test]
    fn a_gpu_column_outlasts_the_cpu_column_on_a_narrow_terminal() {
        // The device columns are what a person on a GPU cluster came for, so
        // they drop after `CPU` and `MEMORY` do — the interesting column
        // stays as long as it can. On a general cluster this step is a
        // no-op, so the tests without devices see the CPU pair drop as
        // step 5 straight into STATUS being the last thing left.
        let rows = [NodeRow::from_node(
            &gpu_node(),
            Some(&booked_device("nvidia.com/gpu", "1")),
            None,
            now(),
        )];

        // At 60 cols VERSION, AGE, both REQ columns, and CPU + MEMORY have
        // all left, but `NVIDIA.COM/GPU` is still there beside NAME and
        // STATUS — the card count is what the reader typed `eks nodes` to
        // see, so it survives the drop that removes the resources every
        // cluster has anyway.
        let cols = headings_at(&rows, 60);
        assert!(cols.contains(&"NVIDIA.COM/GPU".to_owned()), "{cols:?}");
        assert!(!cols.contains(&"CPU".to_owned()), "{cols:?}");
        assert!(!cols.contains(&"MEMORY".to_owned()), "{cols:?}");
        assert!(cols.contains(&"NAME".to_owned()));
        assert!(cols.contains(&"STATUS".to_owned()));
    }

    #[test]
    fn a_device_column_outlasts_the_pod_count_too() {
        // The same argument the CPU column loses: `PODS` is on every listing
        // and the card count is only on the one somebody put hardware in. This
        // is the assertion the placement of `PODS` in `DROP_ORDER` turns on, so
        // it is worth making from the device end rather than only from the
        // request end.
        let rows = [NodeRow::from_node(
            &gpu_node(),
            Some(&booked_device("nvidia.com/gpu", "1")),
            None,
            now(),
        )];

        let cols = headings_at(&rows, 60);
        assert!(cols.contains(&"NVIDIA.COM/GPU".to_owned()), "{cols:?}");
        assert!(!cols.contains(&"PODS".to_owned()), "{cols:?}");
    }

    #[test]
    fn wide_beats_narrow_when_both_could_apply() {
        // `--wide` was explicit; the terminal is not. A row that widened when
        // asked to and then narrowed itself would be a flag that meant
        // nothing. The type gate is in `Width::for_terminal`, and this
        // asserts the listing agrees: `Width::Wide` is the wide set, not a
        // dropped one.
        let rows = one_booked_row();
        let wide = columns(&rows, Width::Wide);
        assert!(wide.iter().any(|c| matches!(c, Column::InternalIp)));
        assert!(wide.iter().any(|c| matches!(c, Column::KernelVersion)));
    }

    #[test]
    fn a_metrics_server_row_drops_the_use_pair_before_the_base_pair() {
        // The USE columns come off before CPU and MEMORY themselves, so a row
        // with metrics never lands in "CPU is gone but CPU USE is still here"
        // — the partner without its base would read as a percentage of
        // nothing.
        let rows = [NodeRow::from_node(
            &healthy_node(),
            Some(&booked("1500m", "6Gi")),
            Some(used("392m", "1552515Ki")),
            now(),
        )];

        // A width tight enough that CPU is gone: the USE columns are already
        // gone too. Loop rather than pick one width, because the fixture's
        // exact widths shift when the sample values do and the invariant is
        // the ordering, not a specific number.
        for target in [1_u16, 20, 40, 60] {
            let cols = headings_at(&rows, target);
            let has_cpu = cols.iter().any(|c| c == "CPU");
            let has_cpu_use = cols.iter().any(|c| c == "CPU USE");
            let has_mem = cols.iter().any(|c| c == "MEMORY");
            let has_mem_use = cols.iter().any(|c| c == "MEM USE");
            assert!(!has_cpu_use || has_cpu, "{target}: {cols:?}");
            assert!(!has_mem_use || has_mem, "{target}: {cols:?}");
        }
    }

    // ---- Severity colour ----------------------------------------------------

    /// A palette that paints, without asking a terminal anything.
    fn colour() -> Palette {
        Palette::choose(crate::theme::ColourChoice::Always, false, None, None)
    }

    #[test]
    fn a_status_cell_is_graded_by_the_row_the_dashboard_would_grade() {
        // Not a second reading of "is this node healthy": the CLI table takes
        // the severity `NodeRow` already carries, so the two surfaces cannot
        // come to disagree about a cordoned node.
        let cases = [
            (healthy_node(), Severity::Ok),
            (
                with_status(&healthy_node(), vec![condition("Ready", "False")]),
                Severity::Critical,
            ),
            (with_status(&healthy_node(), vec![]), Severity::Unknown),
        ];

        for (node, expected) in cases {
            let row = NodeRow::from_node(&node, Some(&idle()), None, now());
            assert_eq!(Column::Status.severity(&row), Some(expected), "{row:?}");
            assert_eq!(Column::Status.severity(&row), Some(row.severity));
        }
    }

    #[test]
    fn every_share_is_graded_and_nothing_else_is() {
        // The columns that carry a reading, and the ones that carry a fact.
        // `CPU` is the interesting `None`: it says how big the machine is, not
        // how much of it is spoken for, and there is nothing about `3920m/4` to
        // be worried about.
        let row = NodeRow::from_node(
            &gpu_node(),
            Some(&booked("3800m", "15Gi")),
            Some(used("400m", "2Gi")),
            now(),
        );

        for column in [
            Column::Status,
            Column::CpuRequested,
            Column::CpuUsed,
            Column::MemoryRequested,
            Column::MemoryUsed,
            Column::Pods,
            Column::Device("nvidia.com/gpu"),
        ] {
            assert!(
                column.severity(&row).is_some(),
                "{} should carry a reading",
                column.header()
            );
        }

        for column in [
            Column::Name,
            Column::Version,
            Column::Cpu,
            Column::Memory,
            Column::Age,
            Column::InternalIp,
            Column::ExternalIp,
            Column::OsImage,
            Column::KernelVersion,
            Column::ContainerRuntime,
        ] {
            assert_eq!(
                column.severity(&row),
                None,
                "{} is a fact, not a reading",
                column.header()
            );
        }
    }

    #[test]
    fn a_share_takes_the_thresholds_it_already_had() {
        // `Severity::from_utilisation`'s, through `Share::severity`. Nothing
        // about what counts as hot is decided by the column.
        let row = NodeRow::from_node(&healthy_node(), Some(&booked("3800m", "1Gi")), None, now());

        assert_eq!(
            Column::CpuRequested.severity(&row),
            Some(row.cpu_requested.severity())
        );
        assert_eq!(
            Column::CpuRequested.severity(&row),
            Some(Severity::Critical)
        );
        assert_eq!(Column::MemoryRequested.severity(&row), Some(Severity::Ok));
    }

    #[test]
    fn a_node_without_the_hardware_grades_its_dash_as_an_absence() {
        // `-` is "no such card", not "none free", and a muted cell says so
        // without joining the two nodes that do have one in red.
        let rows = [
            NodeRow::from_node(&gpu_node(), Some(&idle()), None, now()),
            NodeRow::from_node(&healthy_node(), Some(&idle()), None, now()),
        ];
        let gpu = Column::Device("nvidia.com/gpu");

        assert_eq!(gpu.text(&rows[1]), "-");
        assert_eq!(gpu.severity(&rows[1]), Some(Severity::Unknown));
        // The node that has them and has booked none is a real, calm zero.
        assert_eq!(gpu.severity(&rows[0]), Some(Severity::Ok));
    }

    #[test]
    fn a_failed_pod_listing_greys_the_columns_it_emptied() {
        // The footnote already says why they are empty; the ink says which
        // ones without the reader having to match a sentence to a heading.
        let row = NodeRow::from_node(&healthy_node(), None, None, now());

        assert_eq!(Column::CpuRequested.text(&row), "-");
        assert_eq!(Column::CpuRequested.severity(&row), Some(Severity::Unknown));
        assert_eq!(Column::Pods.severity(&row), Some(Severity::Unknown));
    }

    #[test]
    fn a_plain_table_is_unchanged_to_the_byte() {
        // The promise every listing here makes: `eks nodes | grep` gets what
        // it always got. Asserted against the ink stripped back off the
        // coloured one, so it covers the whole table rather than one column.
        let rows = [
            NodeRow::from_node(
                &with_status(&healthy_node(), vec![condition("Ready", "False")]),
                Some(&booked("3800m", "15Gi")),
                Some(used("3900m", "15Gi")),
                now(),
            ),
            NodeRow::from_node(
                &renamed(&healthy_node(), "ip-10-0-2-3.ec2.internal"),
                Some(&booked("100m", "1Gi")),
                Some(used("50m", "512Mi")),
                now(),
            ),
        ];

        let plain = render(
            &rows,
            "prod (us-east-1)",
            &[],
            Width::Default,
            Palette::Plain,
        );
        let inked = render(&rows, "prod (us-east-1)", &[], Width::Default, colour());

        assert_ne!(plain, inked);
        assert_eq!(plain, strip_ansi(&inked));
    }

    #[test]
    fn only_the_alarming_rows_carry_ink() {
        // The point of `Theme::severity_ink`: a healthy row is written in the
        // terminal's own colour, so the one broken node is the only thing on
        // screen with a colour on it.
        let healthy = [NodeRow::from_node(
            &healthy_node(),
            Some(&booked("100m", "1Gi")),
            Some(used("50m", "512Mi")),
            now(),
        )];

        let table = render(&healthy, "prod (us-east-1)", &[], Width::Default, colour());
        assert!(!table.contains('\u{1b}'), "{table:?}");
    }

    #[test]
    fn a_footnote_is_never_written_in_ink() {
        // Notes are prose under the table, not readings off it. A sentence in
        // red would be shouting a second time about something the table has
        // already coloured.
        let rows = [NodeRow::from_node(
            &with_status(&healthy_node(), vec![condition("Ready", "False")]),
            Some(&idle()),
            None,
            now(),
        )];
        let note = "Live usage is not shown.".to_owned();

        let table = render(
            &rows,
            "prod (us-east-1)",
            std::slice::from_ref(&note),
            Width::Default,
            colour(),
        );

        let (_, footnotes) = table.split_at(table.find(&note).unwrap_or(0));
        assert!(!footnotes.contains('\u{1b}'), "{footnotes:?}");
    }

    #[test]
    fn narrowing_drops_the_same_columns_whether_or_not_there_is_ink() {
        // The width arithmetic measures text, so the drop rule cannot be
        // fooled into taking a column away because a cell was coloured. The
        // node here is hot enough that every share is painted.
        let rows: Vec<NodeRow> = ["ip-10-0-1-9", "ip-10-0-11-200", "ip-10-0-2-77"]
            .into_iter()
            .map(|name| {
                NodeRow::from_node(
                    &renamed(&gpu_node(), name),
                    Some(&booked("3800m", "15Gi")),
                    Some(used("3900m", "15Gi")),
                    now(),
                )
            })
            .collect();

        for target in [1, 60, 80, 100, 200] {
            let width = Width::Narrow(target);
            assert_eq!(
                render(&rows, "prod (us-east-1)", &[], width, Palette::Plain),
                strip_ansi(&render(&rows, "prod (us-east-1)", &[], width, colour())),
                "at {target} columns"
            );
        }
    }

    #[test]
    fn an_empty_listing_says_the_same_thing_in_either_palette() {
        // There is no table to colour, and the message under it is advice.
        assert_eq!(
            render(&[], "prod (us-east-1)", &[], Width::Default, colour()),
            render(&[], "prod (us-east-1)", &[], Width::Default, Palette::Plain)
        );
    }

    /// Every escape sequence removed from a rendered table.
    ///
    /// A copy of the pod table's, deliberately: a shared helper would be a
    /// third opinion about what this crate emits, and the point of these tests
    /// is that the renderer and the reader agree.
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
