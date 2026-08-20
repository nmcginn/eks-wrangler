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

pub use order::{Missing, Order, cause, ranks_any, sort};

use crate::format;
use crate::k8s::metrics::Usage;
use crate::k8s::page;
use crate::k8s::pods::Requests;
use crate::k8s::quantity::{self, Quantity};
use crate::k8s::resource;
use crate::theme::Severity;

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
    /// `2/4 (50%)`, which is the pod table's usage cell rather than either of
    /// this table's two. Neither of those fits: `Capacity`'s pair would print
    /// allocatable over capacity, which for a device is the same number twice
    /// on every healthy node, and `Share`'s `2 (50%)` hides the total — and for
    /// a device the total is the fact people came for. "This node has eight
    /// A100s" is not something to work back out of a percentage.
    fn cell(self) -> String {
        let booked = self
            .booked
            .map_or_else(|| UNKNOWN.to_owned(), quantity::count);

        let Some(allocatable) = self.capacity.allocatable else {
            // Reported under `capacity` and not under `allocatable`, which
            // happens for a moment while a node registers. There is nothing for
            // the figure to be a share of, so it stands alone.
            return booked;
        };

        let total = quantity::count(allocatable);
        match self.share().ratio() {
            Some(ratio) => format!("{booked}/{total} ({})", format::percentage(ratio)),
            // No ratio means either no booked figure or a node offering none of
            // a device it reports; `-/4` and `0/0` are both better than a
            // percentage of nothing.
            None => format!("{booked}/{total}"),
        }
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
        self.amount?.ratio_of(self.allocatable?)
    }

    /// How alarming that fraction is, on the shared `theme` thresholds, so the
    /// CLI table and the dashboard cannot disagree about what counts as hot.
    #[must_use]
    pub fn severity(self) -> Severity {
        self.ratio()
            .map_or(Severity::Unknown, Severity::from_utilisation)
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
    /// `requested` is the total of the pods on this node — `Some(zero)` for a
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
        requested: Option<&Requests>,
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
        }
    }
}

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
    /// One extended resource, by its fully-qualified name.
    ///
    /// The only column whose identity is not known until the nodes arrive, so
    /// it borrows the name from the rows it was computed from rather than
    /// owning a copy per column.
    Device(&'a str),
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
            Self::Device(name) => resource::heading(name),
            Self::Age => "AGE".to_owned(),
            Self::InternalIp => "INTERNAL-IP".to_owned(),
            Self::ExternalIp => "EXTERNAL-IP".to_owned(),
            Self::OsImage => "OS-IMAGE".to_owned(),
            Self::KernelVersion => "KERNEL-VERSION".to_owned(),
            Self::ContainerRuntime => "CONTAINER-RUNTIME".to_owned(),
        }
    }

    /// This column's cell for one row.
    fn cell(self, row: &NodeRow) -> String {
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
            // A node that does not report this device at all reads `-`. It is
            // not a node with none free; it is a node with no such hardware,
            // and the two want different reactions from whoever is looking for
            // somewhere to put a GPU pod.
            Self::Device(name) => row
                .devices
                .get(name)
                .map_or_else(|| UNKNOWN.to_owned(), |device| device.cell()),
            Self::Age => row.age.clone(),
            Self::InternalIp => row.internal_ip.clone(),
            Self::ExternalIp => row.external_ip.clone(),
            Self::OsImage => row.os_image.clone(),
            Self::KernelVersion => row.kernel_version.clone(),
            Self::ContainerRuntime => row.container_runtime.clone(),
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

/// Which columns this listing gets, in order.
///
/// A pure function over the three things that decide it — whether any row has
/// live usage, which extended resources the listing reports, and `--wide` — so
/// the layout is settled by a test rather than by reading a table in a
/// terminal. The conditions differ in the same way the pod table's do: the
/// usage and device columns appear unasked for and so are dropped when a
/// cluster has nothing to put in them, while `--wide` columns were asked for
/// and appear whatever is in them.
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
    // Devices sit after the resources every node has and before AGE, so the
    // block of "what this machine can give out" stays together and AGE stays
    // last of the default columns, where every `kubectl get` puts it.
    columns.extend(device_names(rows).into_iter().map(Column::Device));
    columns.push(Column::Age);
    // `kubectl get nodes -o wide`'s own tail, in its order. It goes after AGE
    // rather than after VERSION, where `kubectl` puts it, so that the default
    // table is the same table with the tail cut off — a user comparing a wide
    // listing against a plain one should not have to re-find the columns.
    if width.is_wide() {
        columns.extend([
            Column::InternalIp,
            Column::ExternalIp,
            Column::OsImage,
            Column::KernelVersion,
            Column::ContainerRuntime,
        ]);
    }
    columns
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
#[must_use]
pub fn render(rows: &[NodeRow], cluster: &str, notes: &[String], width: format::Width) -> String {
    if rows.is_empty() {
        return format!(
            "{cluster} reports no nodes.\n\
             If you expected some, check that its node groups are scaled above zero."
        );
    }

    let columns = columns(rows, width);
    let headings: Vec<String> = columns.iter().map(|column| column.header()).collect();
    let headers: Vec<&str> = headings.iter().map(String::as_str).collect();
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|row| columns.iter().map(|column| column.cell(row)).collect())
        .collect();

    let table = format::table(&headers, &cells);

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
/// a device column has a booked figure in it too and it goes the same way. It
/// says "the booked half" of those because the rest of the cell survives: the
/// device count came back with the nodes, and only the numerator is missing.
#[must_use]
pub fn requests_unavailable(rows: &[NodeRow], explanation: &str) -> String {
    let devices: Vec<String> = device_names(rows)
        .into_iter()
        .map(resource::heading)
        .collect();

    let columns = match format::list(&devices, "and") {
        Some(devices) => format!("CPU REQ, MEM REQ, and the booked half of {devices} are empty"),
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
    /// "we listed the pods and found none" case, which is a real zero.
    fn idle() -> Requests {
        Requests::default()
    }

    fn booked(cpu: &str, memory: &str) -> Requests {
        Requests {
            cpu: Quantity::parse(cpu).unwrap_or_default(),
            memory: Quantity::parse(memory).unwrap_or_default(),
            ..Requests::default()
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
            render(&rows, "prod (us-east-1)", &[], Width::Default),
            "NAME                         STATUS    VERSION              CPU      CPU REQ      MEMORY         MEM REQ    AGE\n\
             ip-10-0-1-9.ec2.internal     Ready     v1.33.1-eks-1a2b3c4  3920m/4  1500m (38%)  14.8Gi/15.6Gi  6Gi (41%)  2d2h\n\
             ip-10-0-11-200.ec2.internal  NotReady  v1.33.1-eks-1a2b3c4  3920m/4  0 (0%)       14.8Gi/15.6Gi  0 (0%)     60m"
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
        let table = render(&rows, "prod (us-east-1)", &[], Width::Default);
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

    #[test]
    fn usage_columns_sit_beside_the_capacity_they_are_a_share_of() {
        let rows = [NodeRow::from_node(
            &healthy_node(),
            Some(&booked("1500m", "6Gi")),
            Some(used("392m", "1552515Ki")),
            now(),
        )];

        assert_eq!(
            render(&rows, "prod (us-east-1)", &[], Width::Default),
            "NAME                      STATUS  VERSION              CPU      CPU REQ      CPU USE     MEMORY         MEM REQ    MEM USE      AGE\n\
             ip-10-0-1-9.ec2.internal  Ready   v1.33.1-eks-1a2b3c4  3920m/4  1500m (38%)  392m (10%)  14.8Gi/15.6Gi  6Gi (41%)  1.5Gi (10%)  2d2h"
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

        let output = render(&rows, "prod (us-east-1)", &[note], Width::Default);
        let (table, footnote) = output
            .split_once("\n\n")
            .expect("a blank line before the note");

        assert_eq!(
            table,
            render(&rows, "prod (us-east-1)", &[], Width::Default)
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

        let output = render(&rows, "prod (us-east-1)", &[note], Width::Default);
        let (table, footnote) = output
            .split_once("\n\n")
            .expect("a blank line before the note");

        assert_eq!(
            table,
            render(&rows, "prod (us-east-1)", &[], Width::Default)
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

        let output = render(&rows, "prod (us-east-1)", &[note], Width::Default);

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

        let output = render(&rows, "prod (us-east-1)", &notes, Width::Default);
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

        let output = render(&rows, "prod (us-east-1)", &[note], Width::Default);
        let (table, footnote) = output
            .split_once("\n\n")
            .expect("a blank line before the note");

        // The table is unchanged by the note; the note is what carries the news.
        assert_eq!(
            table,
            render(&rows, "prod (us-east-1)", &[], Width::Default)
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

        let output = render(&rows, "prod (us-east-1)", &notes, Width::Default);
        let paragraphs: Vec<&str> = output.split("\n\n").collect();

        assert_eq!(
            paragraphs[0],
            render(&rows, "prod (us-east-1)", &[], Width::Default)
        );
        assert_eq!(paragraphs[2], "Sorted by cpu, reversed.");
    }

    #[test]
    fn an_ordering_that_ranked_nothing_says_so_right_under_the_sort_note() {
        // `eks nodes --sort cpu` against a cluster with no metrics-server. The
        // usage footnote explains the missing columns; without the second note
        // nothing explains what became of the flag the user actually typed —
        // and the third line is the flag that would have worked on this table.
        let rows = [NodeRow::from_node(
            &healthy_node(),
            Some(&idle()),
            None,
            now(),
        )];
        let notes = sort_notes(&rows, Order::Cpu, no_usage());

        let output = render(&rows, "prod (us-east-1)", &notes, Width::Default);
        let paragraphs: Vec<&str> = output.split("\n\n").collect();

        assert_eq!(paragraphs[1], "Sorted by cpu.");
        assert_eq!(
            paragraphs[2],
            "Nothing here has cpu to sort by, for the reason above.\n\
             Sort by status, cpu-requested, memory-requested, or age instead."
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
        let rows = [NodeRow::from_node(
            &sampled_but_undescribed,
            Some(&idle()),
            Some(used("392m", "1552515Ki")),
            now(),
        )];

        assert!(shows_usage(&rows), "the columns should be in the table");

        let notes = sort_notes(&rows, Order::Cpu, Missing::default());

        // The booked orderings rank by share too, so removing the denominator
        // takes them with it — which is exactly why the advice is computed from
        // the rows rather than listed out by hand.
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
        // the advice a paragraph later.
        let rows = [NodeRow::from_node(
            &healthy_node(),
            Some(&idle()),
            None,
            now(),
        )];

        assert!(
            !shows_usage(&rows),
            "there is nothing to put in the columns"
        );

        let notes = sort_notes(&rows, Order::Cpu, no_usage());

        assert_eq!(
            notes[1],
            "Nothing here has cpu to sort by, for the reason above.\n\
             Sort by status, cpu-requested, memory-requested, or age instead."
        );
    }

    #[test]
    fn an_ordering_no_footnote_could_explain_never_claims_one_did() {
        // Both fetches failed *and* the API server left out the creation
        // timestamps. The usage and request footnotes are above the table, but
        // neither of them is about `age`, so pointing at them would send the
        // user to read a paragraph that does not mention the column.
        // `Node::default()`: no creation timestamp, no allocatable, no status.
        let rows = [NodeRow::from_node(&Node::default(), None, None, now())];
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
        let message = render(&[], "prod (us-east-1)", &[note], Width::Default);

        assert!(!message.contains("Sorted by"), "{message}");
        assert!(message.contains("node groups"), "{message}");
    }

    #[test]
    fn a_cluster_with_no_nodes_skips_the_footnote() {
        // There is a bigger problem than a missing column to explain.
        let note = requests_unavailable(&[], "nope");
        let message = render(&[], "prod (us-east-1)", &[note], Width::Default);

        assert!(!message.contains("CPU REQ"), "{message}");
        assert!(message.contains("node groups"), "{message}");
    }

    #[test]
    fn an_empty_cluster_explains_itself_instead_of_printing_a_bare_header() {
        let message = render(&[], "prod (us-east-1)", &[], Width::Default);

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

        let table = render(&rows, "prod (us-east-1)", &[], Width::Default);
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
            render(&rows, "prod (us-east-1)", &[], Width::Wide),
            "NAME                      STATUS  VERSION              CPU      CPU REQ  MEMORY         MEM REQ  AGE   INTERNAL-IP  EXTERNAL-IP  OS-IMAGE                      KERNEL-VERSION                   CONTAINER-RUNTIME\n\
             ip-10-0-1-9.ec2.internal  Ready   v1.33.1-eks-1a2b3c4  3920m/4  0 (0%)   14.8Gi/15.6Gi  0 (0%)   2d2h  10.0.1.9     -            Amazon Linux 2023.9.20260714  6.1.148-172.265.amzn2023.x86_64  containerd://1.7.28"
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

        let table = render(&[row], "prod (us-east-1)", &[], Width::Wide);
        assert!(table.contains("CONTAINER-RUNTIME"), "{table}");
    }

    #[test]
    fn an_empty_wide_listing_still_says_where_the_nodes_went() {
        // `--wide` changes columns, and there are no columns here to change.
        assert_eq!(
            render(&[], "prod (us-east-1)", &[], Width::Wide),
            render(&[], "prod (us-east-1)", &[], Width::Default)
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
                ("hugepages-2Mi", "0"),
                ("nvidia.com/gpu", "4"),
            ]));
            status.allocatable = Some(quantities(&[
                ("cpu", "3920m"),
                ("memory", "15525152Ki"),
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
                ("nvidia.com/gpu", held),
            ]));
            status.allocatable = Some(quantities(&[
                ("cpu", "3920m"),
                ("memory", "15525152Ki"),
                ("nvidia.com/gpu", offered),
            ]));
        }
        node
    }

    fn booked_device(resource: &str, count: &str) -> Requests {
        Requests {
            extended: [(
                resource.to_owned(),
                Quantity::parse(count).unwrap_or_default(),
            )]
            .into_iter()
            .collect(),
            ..booked("1", "1Gi")
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
            .map(|row| Column::Device(resource).cell(row))
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
                "NAME", "STATUS", "VERSION", "CPU", "CPU REQ", "MEMORY", "MEM REQ", "AGE"
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
        let table = render(&rows, "prod (us-east-1)", &[], Width::Default);

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
            render(&rows, "prod (us-east-1)", &[], Width::Default),
            "NAME                      STATUS  VERSION              CPU      CPU REQ  MEMORY         MEM REQ   NVIDIA.COM/GPU  AGE\n\
             ip-10-0-1-9.ec2.internal  Ready   v1.33.1-eks-1a2b3c4  3920m/4  1 (26%)  14.8Gi/15.6Gi  1Gi (7%)  2/4 (50%)       2d2h"
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
                "CPU REQ, MEM REQ, and the booked half of NVIDIA.COM/GPU are empty because"
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
            "CPU REQ and MEM REQ are empty because the pods could not be listed.\nwhy"
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
            "CPU REQ and MEM REQ are empty because the pods could not be listed.\nwhy"
        );
    }
}
