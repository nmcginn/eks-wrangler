//! Live usage from `metrics.k8s.io`, when there is any.
//!
//! Requests say what a pod *asked* for; usage says what it is actually doing.
//! The two diverge constantly — a node booked to 95% and idling at 8% is
//! over-provisioned, and a node booked to 30% and pegged at 99% is about to
//! throttle — and neither number answers the other's question. This module
//! supplies the second one.
//!
//! Two things shape everything here.
//!
//! The first is that `metrics.k8s.io` is not part of Kubernetes. It is an
//! aggregated API served by metrics-server, an optional add-on that EKS does
//! not install for you. So the absent case is not an edge case, it is the
//! default on a fresh cluster, and it must cost the user two columns and a
//! footnote rather than their node listing. [`explain`] is where a `404` from
//! the aggregation layer becomes a sentence saying what to install.
//!
//! The second is that the metrics types are not in `k8s-openapi`, which only
//! generates the core API. [`NodeMetrics`] is therefore hand-written: a serde
//! struct plus a [`kube::Resource`] impl naming the group, version, and plural
//! that put `/apis/metrics.k8s.io/v1beta1/nodes` on the wire.
//!
//! The pod half of the API is the same idea in a different shape.
//! [`NodeMetrics`] carries one `usage` map because a node is one machine;
//! [`PodMetrics`] carries a *list* of containers, because a pod's usage is
//! whatever its containers are doing added together. It is also namespaced,
//! which makes the listing follow `--namespace`/`--all-namespaces` rather than
//! being cluster-wide like the node one. [`pod_usage`] is the summing, and it
//! is a pure function precisely because the awkward cases live there.
//!
//! Fetching sits behind [`Source`] so the interesting paths — no
//! metrics-server, a node the sampler has not reached yet, a usage figure that
//! will not parse — are fixtures rather than a cluster somebody has to break.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::future::Future;

use k8s_openapi::apimachinery::pkg::api::resource::Quantity as ApiQuantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};
use k8s_openapi::jiff::{SignedDuration, Timestamp};
use k8s_openapi::{ClusterResourceScope, NamespaceResourceScope};
use kube::api::{Api, ListParams};
use kube::{Client, Resource};
use serde::Deserialize;

use crate::format;
use crate::k8s::client;
use crate::k8s::page;
use crate::k8s::pods::{Scope, Selectors};
use crate::k8s::quantity::Quantity;

/// One node's sampled usage, as `metrics.k8s.io/v1beta1` reports it.
///
/// Only the fields we read are modelled; serde ignores the rest, so a newer
/// metrics-server adding a field cannot break the listing.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeMetrics {
    #[serde(default)]
    pub metadata: ObjectMeta,
    /// When the reading was taken — the *end* of the window below, so a fresh
    /// sample is a few seconds old rather than zero seconds old.
    #[serde(default)]
    pub timestamp: Option<Time>,
    /// How long the reading was averaged over, as Go's `time.Duration` prints
    /// it: `20.04s`, `1m0s`. Kept as text here and parsed when the sample is
    /// indexed, so a spelling we do not understand costs the freshness note
    /// rather than the whole sample.
    #[serde(default)]
    pub window: Option<String>,
    /// `cpu` and `memory`, in the usual resource-quantity grammar.
    #[serde(default)]
    pub usage: BTreeMap<String, ApiQuantity>,
}

// Hand-written because `k8s-openapi` only generates the core API, and
// `metrics.k8s.io` is an aggregated one. The default `url_path` turns these
// four strings into `/apis/metrics.k8s.io/v1beta1/nodes`, which is the whole
// point of the impl.
impl Resource for NodeMetrics {
    type DynamicType = ();
    type Scope = ClusterResourceScope;

    fn kind((): &()) -> Cow<'_, str> {
        "NodeMetrics".into()
    }

    fn group((): &()) -> Cow<'_, str> {
        "metrics.k8s.io".into()
    }

    fn version((): &()) -> Cow<'_, str> {
        "v1beta1".into()
    }

    fn plural((): &()) -> Cow<'_, str> {
        "nodes".into()
    }

    fn meta(&self) -> &ObjectMeta {
        &self.metadata
    }

    fn meta_mut(&mut self) -> &mut ObjectMeta {
        &mut self.metadata
    }
}

/// What one node is actually using, right now.
///
/// Each half is `None` when the sampler did not report it, or reported
/// something that will not parse. That is deliberately not folded to zero the
/// way a missing *request* is: a container with no request really has asked for
/// nothing, whereas a node with no usage reading is a node we have not heard
/// from, and rendering that as `0%` would invent an idle machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub cpu: Option<Quantity>,
    pub memory: Option<Quantity>,
}

impl Usage {
    /// Read `cpu` and `memory` out of a `usage` map.
    #[must_use]
    pub fn read(usage: &BTreeMap<String, ApiQuantity>) -> Self {
        Self {
            cpu: Quantity::lookup(Some(usage), "cpu"),
            memory: Quantity::lookup(Some(usage), "memory"),
        }
    }
}

/// One reading, with the two facts that say how much to trust it.
///
/// A usage figure on its own cannot be told apart from an instantaneous one,
/// and it cannot be told apart from a stale one either — which matters because
/// metrics-server going quiet does not fail the request that asks it for a
/// sample. The same table keeps rendering, with figures that are minutes old
/// and look exactly like fresh ones. [`taken_at`](Self::taken_at) and
/// [`window`](Self::window) are what [`freshness`] dates a listing from.
///
/// Both are optional because both are the sampler's word rather than ours. A
/// reading whose timestamp is missing or unreadable is still a reading, so it
/// keeps its figures and loses only its place in the note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sample {
    /// What the node or pod was using.
    pub usage: Usage,
    /// When the reading was taken.
    pub taken_at: Option<Timestamp>,
    /// How long it was averaged over.
    pub window: Option<SignedDuration>,
}

impl Sample {
    /// Build a sample from one metrics item's usage and its two stamps.
    fn new(usage: Usage, timestamp: Option<&Time>, window: Option<&str>) -> Self {
        Self {
            usage,
            taken_at: timestamp.map(|at| at.0),
            window: window.and_then(parse_duration),
        }
    }

    /// Whether this one reading, on its own, is old enough to call stale.
    ///
    /// [`freshness_note`] asks this of the whole listing's *oldest* sample, which
    /// is a guarantee about every row and therefore only as good as the worst
    /// one — a single node whose kubelet stopped reporting drags the note down
    /// without saying which row is the reason. This asks
    /// [`Freshness::is_stale`] the same question of one sample, so a row can
    /// say the same thing about itself.
    #[must_use]
    pub fn is_stale(&self, now: Timestamp) -> bool {
        freshness(std::iter::once(self), now).is_some_and(Freshness::is_stale)
    }
}

/// Go's duration units, in nanoseconds, longest spelling first.
///
/// The order is the whole correctness of [`parse_duration`]: read `ms` as `m`
/// and a 500-millisecond window becomes eight hours, which would call every
/// listing fresh forever.
const UNITS: [(&str, i64); 8] = [
    ("ns", 1),
    ("us", 1_000),
    // Go emits the micro sign; some encoders emit the Greek letter instead, and
    // the two are different characters that look identical.
    ("\u{b5}s", 1_000),
    ("\u{3bc}s", 1_000),
    ("ms", 1_000_000),
    ("s", 1_000_000_000),
    ("m", 60_000_000_000),
    ("h", 3_600_000_000_000),
];

/// Parse a Go duration string, which is how a `metav1.Duration` reaches the wire.
///
/// metrics-server reports its averaging window as `20.04s`, `1m0s`, `500ms` —
/// `time.Duration`'s own formatting, a run of `<number><unit>` pairs rather than
/// anything ISO-8601 that `jiff` would take directly.
///
/// Anything that is not that grammar is `None` rather than a guess: the window
/// decides whether a listing is called stale, and a window read wrongly would
/// either accuse a healthy cluster or excuse a scraper that has stopped.
///
/// Integer arithmetic throughout, deliberately. `20.04s` is exact in nanoseconds
/// and is not exact in binary floating point, and this value is compared against
/// another duration rather than merely printed.
fn parse_duration(text: &str) -> Option<SignedDuration> {
    let (negative, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };

    // Go prints a zero duration as `0s`, and accepts a bare `0`. Take both.
    if body == "0" {
        return Some(SignedDuration::ZERO);
    }

    let mut nanos: i64 = 0;
    let mut rest = body;
    while !rest.is_empty() {
        // Digits and `.` are ASCII, so this byte index is always a char
        // boundary, including when the unit that follows is a micro sign.
        let end = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        let (number, tail) = rest.split_at(end);
        let (scale, remainder) = UNITS
            .iter()
            .find_map(|(unit, scale)| tail.strip_prefix(unit).map(|rest| (*scale, rest)))?;
        nanos = nanos.checked_add(scaled(number, scale)?)?;
        rest = remainder;
    }

    // An empty `body` never entered the loop, so it lands here as a zero it is
    // not; reject it alongside the arithmetic that overflowed.
    if body.is_empty() {
        return None;
    }

    let nanos = if negative {
        nanos.checked_neg()?
    } else {
        nanos
    };
    Some(SignedDuration::from_nanos(nanos))
}

/// One `<number><unit>` pair, in nanoseconds.
///
/// The fractional part is walked a digit at a time rather than parsed, so that
/// the multiplication is by a power of ten this function has already reduced —
/// no rounding, and a window written to more precision than a nanosecond is
/// truncated exactly as Go truncates it rather than overflowing.
fn scaled(number: &str, scale: i64) -> Option<i64> {
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty() && fraction.is_empty() {
        return None;
    }

    // Go reads `.5s`, where the whole part is absent rather than zero.
    let mut nanos = if whole.is_empty() {
        0
    } else {
        whole.parse::<i64>().ok()?.checked_mul(scale)?
    };

    let mut place = scale;
    for digit in fraction.chars() {
        place /= 10;
        if place == 0 {
            break;
        }
        nanos = nanos.checked_add(i64::from(digit.to_digit(10)?).checked_mul(place)?)?;
    }
    Some(nanos)
}

/// How old the usage figures in one listing are.
///
/// Computed from the samples that actually reached a row, so a listing narrowed
/// by a selector is dated by what it shows rather than by whatever the metrics
/// endpoint happened to return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Freshness {
    /// How long ago the *oldest* sample in the listing was taken, so that every
    /// figure in the table is at most this old. The oldest rather than the
    /// newest because the note is a guarantee about the whole table, and a
    /// guarantee is only as good as its worst row.
    pub age: SignedDuration,
    /// The longest window any of those samples was averaged over, when any of
    /// them said.
    pub window: Option<SignedDuration>,
}

impl Freshness {
    /// Whether these figures describe the past rather than the present.
    ///
    /// A couple of windows is the bar. metrics-server publishes a new reading
    /// about once per window, so a listing one window behind is ordinary and one
    /// two windows behind has missed a scrape — which is what a stopped scraper
    /// looks like from here, since the request that asks for the sample keeps
    /// succeeding.
    ///
    /// A listing whose window we could not read is never accused. Without one
    /// there is no scale to judge an age against, and "your figures are stale"
    /// is not a sentence to print on a guess.
    #[must_use]
    pub fn is_stale(self) -> bool {
        self.window
            .filter(|window| *window > SignedDuration::ZERO)
            .and_then(|window| window.checked_mul(2))
            .is_some_and(|couple| self.age > couple)
    }
}

/// Date a listing from the samples that reached it.
///
/// `None` when nothing was sampled, or when no sample carried a timestamp we
/// could read. A note that cannot say how old the figures are is worse than no
/// note at all: the reader would take the missing half for freshness.
///
/// Clock skew — a sample stamped in the future — reads as an age of zero rather
/// than as a negative one, which is what [`format::human_duration`] already does
/// with a node created in the future.
#[must_use]
pub fn freshness<'a>(
    samples: impl IntoIterator<Item = &'a Sample>,
    now: Timestamp,
) -> Option<Freshness> {
    let mut oldest: Option<Timestamp> = None;
    let mut window: Option<SignedDuration> = None;

    for sample in samples {
        if let Some(at) = sample.taken_at {
            oldest = Some(oldest.map_or(at, |current| current.min(at)));
        }
        // The longest, so a listing whose samples disagree is described by the
        // slowest of them — the one that decides how long "up to date" is.
        if let Some(seen) = sample.window {
            window = Some(window.map_or(seen, |current| current.max(seen)));
        }
    }

    Some(Freshness {
        age: now.duration_since(oldest?).max(SignedDuration::ZERO),
        window,
    })
}

/// The line under a table saying how old its usage figures are, and whether that
/// is a problem.
///
/// One wording for both listings, because it is a fact about the sample rather
/// than about the columns: `eks nodes` and `eks pods` read the same
/// metrics-server, and a person reading one after the other should not have to
/// work out whether two differently worded lines mean the same thing.
#[must_use]
pub fn freshness_note(freshness: Freshness) -> String {
    let age = format::human_duration(freshness.age);

    let Some(window) = freshness.window else {
        // Honest about the half we have. Naming no window also keeps the line
        // from implying a staleness judgement that `is_stale` refused to make.
        return format!("Usage is up to {age} old.");
    };
    let window = format::human_duration(window);

    if freshness.is_stale() {
        format!(
            "Usage is up to {age} old, averaged over {window} \u{2014} more than two sampling windows, so these figures are stale.\n\
             metrics-server can stop scraping without failing this request; check its pod in kube-system."
        )
    } else {
        format!("Usage is up to {age} old, averaged over {window}.")
    }
}

/// Mark one row's own usage cell as older than the rest of the table.
///
/// [`freshness_note`] dates the whole listing from its oldest sample, so a
/// table where one row's sample is stale and the rest are fresh reads as
/// uniformly current everywhere but that line. This is the per-row half:
/// [`Sample::is_stale`] on the one sample behind `cell`, not a second
/// staleness rule. A no-op when `stale` is `false`, so a listing where every
/// sample is current renders unchanged to the byte — and a caller that has
/// not sampled this row at all passes `false` for the same reason, rather
/// than a marker meaning two different things.
///
/// One wording for both listings, the same reason [`freshness_note`] is.
#[must_use]
pub fn mark_stale(cell: String, stale: bool) -> String {
    if stale {
        format!("{cell} (stale)")
    } else {
        cell
    }
}

/// What became of a listing's usage columns.
///
/// Three outcomes rather than two, which is the whole of this task: until now
/// the command asked only whether the metrics read had failed, and a read that
/// *succeeded* with nothing in it took the columns away just as thoroughly while
/// saying nothing at all. From the reader's chair that third case is
/// indistinguishable from a cluster with no metrics-server, and the advice for
/// the two is opposite — one says install it, the other says wait for it.
///
/// A value rather than a pair of `bool`s at the call site, for the reason
/// [`crate::k8s::order::Cause`] is one: the branch decides which sentence a user
/// reads, and it should be legible where it is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Figures reached the table. They want dating — see [`freshness_note`].
    Shown,
    /// The read answered, and nothing in this listing had been sampled. Nothing
    /// has been said about it yet, so this is the case that owes a footnote —
    /// see [`unsampled`].
    Unsampled,
    /// The read failed. Already footnoted where the error was caught, with the
    /// explanation only that branch has — see [`explain`].
    Unreadable,
}

impl Outcome {
    /// Read from the two things the command layer knows: whether the metrics
    /// request answered, and whether any figure from it reached the rendered
    /// rows.
    ///
    /// The second half is asked of the rows rather than of the reply, so the
    /// footnote and the columns cannot disagree: a reply full of samples for
    /// pods that a `--field-selector` kept out of the table is, to this listing,
    /// no samples at all.
    #[must_use]
    pub fn of<T>(read: Option<&T>, shown: bool) -> Self {
        match (read, shown) {
            // A read that never answered cannot have put a figure on screen, so
            // the second half has nothing to add.
            (None, _) => Self::Unreadable,
            (Some(_), true) => Self::Shown,
            (Some(_), false) => Self::Unsampled,
        }
    }

    /// Whether the table lost its usage columns, whichever way it lost them.
    ///
    /// Both losing outcomes now leave a footnote above the table, which is what
    /// `k8s::nodes::Missing` and `k8s::pods::Missing` are really asking about
    /// when they decide whether the "nothing ranked" note can point upwards
    /// instead of repeating the advice.
    #[must_use]
    pub fn is_missing(self) -> bool {
        !matches!(self, Self::Shown)
    }
}

/// The sentence for a metrics read that answered, and answered with nothing.
///
/// Every branch of [`explain`] is a failure; this is not one. The aggregation
/// layer is registered, the request succeeded, and the reply held no reading for
/// anything in this listing. On screen that is indistinguishable from a cluster
/// with no metrics-server — the usage columns are simply not there — so it earns
/// the same footnote, worded for a scraper that has not got here yet rather than
/// one that was never installed.
///
/// `cluster` is a human label such as `prod (us-east-1)`, not an ARN.
#[must_use]
pub fn unsampled(cluster: &str) -> String {
    format!(
        "metrics-server answered for {cluster}, so it is installed \u{2014} it has simply not got to anything in this listing.\n\
         A fresh install, or a node that has only just joined, takes a scrape interval or two to appear; if it stays empty, check the metrics-server pod in kube-system."
    )
}

/// One pod's sampled usage, as `metrics.k8s.io/v1beta1` reports it.
///
/// Unlike [`NodeMetrics`] this has no `usage` of its own: metrics-server
/// reports per *container*, and the pod's figure is their sum. See
/// [`pod_usage`] for what that sum has to be careful about.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PodMetrics {
    #[serde(default)]
    pub metadata: ObjectMeta,
    /// When the reading was taken. See [`NodeMetrics::timestamp`]; the pod half
    /// of the API stamps the pod, not each container, so one pod is one sample
    /// however many containers it has.
    #[serde(default)]
    pub timestamp: Option<Time>,
    /// How long it was averaged over. See [`NodeMetrics::window`].
    #[serde(default)]
    pub window: Option<String>,
    #[serde(default)]
    pub containers: Vec<ContainerMetrics>,
}

/// One container's slice of a [`PodMetrics`] sample.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerMetrics {
    #[serde(default)]
    pub name: String,
    /// `cpu` and `memory`, in the usual resource-quantity grammar.
    #[serde(default)]
    pub usage: BTreeMap<String, ApiQuantity>,
}

// Namespaced, unlike the node one: `/apis/metrics.k8s.io/v1beta1/pods` for
// every namespace, `/apis/metrics.k8s.io/v1beta1/namespaces/<ns>/pods` for one.
// `kube` picks between them from this `Scope` and the `Api` constructor used.
impl Resource for PodMetrics {
    type DynamicType = ();
    type Scope = NamespaceResourceScope;

    fn kind((): &()) -> Cow<'_, str> {
        "PodMetrics".into()
    }

    fn group((): &()) -> Cow<'_, str> {
        "metrics.k8s.io".into()
    }

    fn version((): &()) -> Cow<'_, str> {
        "v1beta1".into()
    }

    fn plural((): &()) -> Cow<'_, str> {
        "pods".into()
    }

    fn meta(&self) -> &ObjectMeta {
        &self.metadata
    }

    fn meta_mut(&mut self) -> &mut ObjectMeta {
        &mut self.metadata
    }
}

/// Which pod a sample belongs to: namespace, then name.
///
/// A bare name is not enough — `kube-system/coredns` and `payments/coredns` are
/// different pods, and `--all-namespaces` puts both in one table. Ordered
/// namespace-first so a `BTreeMap` keyed on it iterates the way the table is
/// sorted.
pub type PodKey = (String, String);

/// Sum a pod's per-container usage into one figure per resource.
///
/// Two rules, both of which exist so a number on screen is never quieter than
/// the truth:
///
/// - A pod with no containers in the sample is unknown, not zero. That is what
///   metrics-server sends for a pod it has registered but not yet scraped.
/// - If any one container is missing a resource, or reports something that will
///   not parse, the whole pod is unknown for that resource. A partial sum is
///   indistinguishable on screen from a complete one, and it would understate
///   exactly the pod somebody is investigating.
#[must_use]
pub fn pod_usage(sample: &PodMetrics) -> Usage {
    Usage {
        cpu: sum_containers(&sample.containers, "cpu"),
        memory: sum_containers(&sample.containers, "memory"),
    }
}

/// Add one resource up across a pod's containers, or give up entirely.
fn sum_containers(containers: &[ContainerMetrics], resource: &str) -> Option<Quantity> {
    if containers.is_empty() {
        return None;
    }

    // `try_fold` over `Option` is the "all or nothing" rule: the first
    // container that cannot be read stops the sum and the pod reads as unknown.
    containers.iter().try_fold(Quantity::default(), |total, c| {
        Quantity::lookup(Some(&c.usage), resource).map(|amount| total + amount)
    })
}

/// Index a pod usage listing by namespace and name.
///
/// A sample missing either half of its identity is dropped: there is no row it
/// could be joined onto, and putting it on the wrong one would be worse than
/// the `-` the renderer already has for a pod nothing was sampled for.
#[must_use]
pub fn by_pod(metrics: &[PodMetrics]) -> BTreeMap<PodKey, Sample> {
    metrics
        .iter()
        .filter_map(|item| {
            let namespace = item
                .metadata
                .namespace
                .as_deref()
                .filter(|ns| !ns.is_empty())?;
            let name = item.metadata.name.as_deref().filter(|n| !n.is_empty())?;
            let sample = Sample::new(
                pod_usage(item),
                item.timestamp.as_ref(),
                item.window.as_deref(),
            );
            Some(((namespace.to_owned(), name.to_owned()), sample))
        })
        .collect()
}

/// The list parameters a pod metrics listing is asked for.
///
/// The label selector is passed through, because the aggregation layer filters
/// on labels like any other API. The *field* selector deliberately is not:
/// metrics-server does not implement field filtering, and the fields people
/// select on — `status.phase`, `spec.nodeName` — are not on a `PodMetrics`
/// anyway. Sending one would be asking a server to filter on something it
/// cannot see. The listing is instead narrowed by the join: usage is only ever
/// shown against a pod row, and the rows have already been filtered by both
/// selectors server-side.
#[must_use]
pub fn pod_params(selectors: &Selectors) -> ListParams {
    let mut params = ListParams::default();
    if let Some(label) = &selectors.label {
        params = params.labels(label);
    }
    params
}

/// Where usage figures come from.
///
/// A trait rather than a bare function because the answers worth testing are
/// the ones a cluster will not give on demand: metrics-server missing entirely,
/// a node absent from the sample, a reading that will not parse. A fake source
/// makes each of those a fixture.
///
/// The return type is spelled out rather than written as `async fn` so the
/// future is `Send`, which is what lets a caller put it in `tokio::join!`
/// alongside the node and pod listings.
pub trait Source {
    /// Usage for every node the sampler has heard from.
    fn node_usage(
        &self,
        budget: page::Budget,
    ) -> impl Future<Output = Result<Vec<NodeMetrics>, page::Error>> + Send;

    /// Usage for the pods in `scope`, narrowed by the label half of
    /// `selectors` — see [`pod_params`] for why only that half.
    fn pod_usage(
        &self,
        scope: &Scope,
        selectors: &Selectors,
        budget: page::Budget,
    ) -> impl Future<Output = Result<Vec<PodMetrics>, page::Error>> + Send;
}

/// The real thing: `metrics.k8s.io` on a live cluster.
///
/// The only implementation that touches the network, and it reads both
/// listings through [`page::collect`] like every other listing in the tool.
/// metrics-server itself does not chunk its answers — it serves what it has in
/// memory in one go — so `limit` is a request it declines rather than obeys,
/// and the loop finishes after one page. Going through the same path anyway is
/// what puts the *budget* on these requests: a metrics endpoint that has
/// stopped answering should cost the same wait as any other, not an unbounded
/// one, and the columns it feeds are the ones the tool can do without.
impl Source for Client {
    fn node_usage(
        &self,
        budget: page::Budget,
    ) -> impl Future<Output = Result<Vec<NodeMetrics>, page::Error>> + Send {
        let api: Api<NodeMetrics> = Api::all(self.clone());
        async move { page::collect(&api, &ListParams::default(), budget).await }
    }

    fn pod_usage(
        &self,
        scope: &Scope,
        selectors: &Selectors,
        budget: page::Budget,
    ) -> impl Future<Output = Result<Vec<PodMetrics>, page::Error>> + Send {
        let api: Api<PodMetrics> = match scope {
            Scope::Namespace(name) => Api::namespaced(self.clone(), name),
            Scope::All => Api::all(self.clone()),
        };
        let params = pod_params(selectors);
        async move { page::collect(&api, &params, budget).await }
    }
}

/// Fetch node usage and index it by node name.
///
/// Generic over [`Source`] so the command layer's happy path and its
/// degraded one are both reachable from a test without a cluster.
pub async fn usage_by_node<S: Source>(
    source: &S,
    budget: page::Budget,
) -> Result<BTreeMap<String, Sample>, page::Error> {
    Ok(by_node(&source.node_usage(budget).await?))
}

/// Fetch pod usage for `scope` and index it by namespace and name.
///
/// Generic over [`Source`] for the same reason as [`usage_by_node`]: the paths
/// worth testing are the ones a healthy cluster will not produce.
pub async fn usage_by_pod<S: Source>(
    source: &S,
    scope: &Scope,
    selectors: &Selectors,
    budget: page::Budget,
) -> Result<BTreeMap<PodKey, Sample>, page::Error> {
    Ok(by_pod(&source.pod_usage(scope, selectors, budget).await?))
}

/// Index a usage listing by node name.
///
/// A sample with no name in it is dropped: there is no row it could belong to,
/// and guessing would be worse than the `-` the caller already renders for a
/// node the sampler has not reached.
#[must_use]
pub fn by_node(metrics: &[NodeMetrics]) -> BTreeMap<String, Sample> {
    metrics
        .iter()
        .filter_map(|item| {
            let name = item.metadata.name.as_deref().filter(|n| !n.is_empty())?;
            let sample = Sample::new(
                Usage::read(&item.usage),
                item.timestamp.as_ref(),
                item.window.as_deref(),
            );
            Some((name.to_owned(), sample))
        })
        .collect()
}

/// Turn a failed metrics request into the sentence a user should read.
///
/// Separate from [`crate::k8s::explain`] because the two failures that dominate
/// here are ones a core-API caller never sees, and both have concrete advice
/// behind them. Everything else falls through to the shared explanation, which
/// already handles expired credentials, RBAC, and an unreachable API server.
///
/// `cluster` is a human label such as `prod (us-east-1)`, not an ARN.
#[must_use]
pub fn explain(error: &page::Error, cluster: &str) -> String {
    match error.status_code() {
        // The aggregation layer answers for a group nobody registered with a
        // 404 — sometimes as a `Status`, sometimes as a bare `404 page not
        // found` that `kube` reconstructs into one. Either way this is the
        // fresh-EKS-cluster case, and it is not an error the user made.
        Some(404) => format!(
            "{cluster} has no metrics.k8s.io API, so metrics-server does not appear to be installed.\n\
             Install it to see live usage: https://github.com/kubernetes-sigs/metrics-server"
        ),
        // Registered but not serving: metrics-server refuses to answer until it
        // has scraped every node once, which takes a minute or so after it
        // starts and forever if it cannot reach the kubelets.
        Some(503) => format!(
            "metrics-server is registered on {cluster} but is not answering yet.\n\
             It stays unavailable until it has scraped every node once — give it a minute, \
             then check its pod in kube-system if it does not settle."
        ),
        _ => client::explain(error, cluster),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use kube::core::Status;

    use super::*;

    fn api_error(code: u16, message: &str) -> page::Error {
        kube::Error::Api(Status::failure(message, "Failure").with_code(code).boxed()).into()
    }

    /// The instant every fixture here is dated against.
    fn now() -> Timestamp {
        "2026-08-17T12:00:00Z"
            .parse()
            .expect("a literal RFC 3339 timestamp")
    }

    fn seconds_ago(seconds: i64) -> Time {
        Time(now() - SignedDuration::from_secs(seconds))
    }

    /// Stamped as metrics-server stamps a reading it took a moment ago, since
    /// that is what almost every sample the tool sees looks like.
    fn sample(name: &str, cpu: &str, memory: &str) -> NodeMetrics {
        NodeMetrics {
            metadata: ObjectMeta {
                name: Some(name.to_owned()),
                ..Default::default()
            },
            timestamp: Some(seconds_ago(8)),
            window: Some("20.04s".to_owned()),
            usage: [("cpu", cpu), ("memory", memory)]
                .into_iter()
                .map(|(key, value)| (key.to_owned(), ApiQuantity(value.to_owned())))
                .collect(),
        }
    }

    fn pod_sample(namespace: &str, name: &str, containers: Vec<ContainerMetrics>) -> PodMetrics {
        PodMetrics {
            metadata: ObjectMeta {
                namespace: Some(namespace.to_owned()),
                name: Some(name.to_owned()),
                ..Default::default()
            },
            timestamp: Some(seconds_ago(8)),
            window: Some("20.04s".to_owned()),
            containers,
        }
    }

    /// A sample carrying nothing but its stamps, for dating a listing.
    fn stamped(taken_at: Option<Timestamp>, window: Option<&str>) -> Sample {
        Sample {
            usage: Usage::default(),
            taken_at,
            window: window.and_then(parse_duration),
        }
    }

    fn container(name: &str, cpu: &str, memory: &str) -> ContainerMetrics {
        ContainerMetrics {
            name: name.to_owned(),
            usage: [("cpu", cpu), ("memory", memory)]
                .into_iter()
                .map(|(key, value)| (key.to_owned(), ApiQuantity(value.to_owned())))
                .collect(),
        }
    }

    /// A [`Source`] that answers from a fixture, so the absent-metrics-server
    /// path is a test rather than a cluster somebody has to uninstall from.
    ///
    /// Both halves answer the same way, since every caller wants one of them.
    struct Fake {
        nodes: Result<Vec<NodeMetrics>, u16>,
        pods: Result<Vec<PodMetrics>, u16>,
    }

    impl Fake {
        fn nodes(answer: Result<Vec<NodeMetrics>, u16>) -> Self {
            Self {
                nodes: answer,
                pods: Ok(Vec::new()),
            }
        }

        fn pods(answer: Result<Vec<PodMetrics>, u16>) -> Self {
            Self {
                nodes: Ok(Vec::new()),
                pods: answer,
            }
        }
    }

    impl Source for Fake {
        fn node_usage(
            &self,
            _budget: page::Budget,
        ) -> impl Future<Output = Result<Vec<NodeMetrics>, page::Error>> + Send {
            let answer = match &self.nodes {
                Ok(samples) => Ok(samples.clone()),
                Err(code) => Err(api_error(*code, "no")),
            };
            async move { answer }
        }

        fn pod_usage(
            &self,
            _scope: &Scope,
            _selectors: &Selectors,
            _budget: page::Budget,
        ) -> impl Future<Output = Result<Vec<PodMetrics>, page::Error>> + Send {
            let answer = match &self.pods {
                Ok(samples) => Ok(samples.clone()),
                Err(code) => Err(api_error(*code, "no")),
            };
            async move { answer }
        }
    }

    #[test]
    fn the_metrics_endpoint_is_the_aggregated_one_not_a_core_api_path() {
        // The whole reason this type is hand-written: get the path wrong and
        // every cluster looks like it has no metrics-server.
        assert_eq!(
            NodeMetrics::url_path(&(), None),
            "/apis/metrics.k8s.io/v1beta1/nodes"
        );
        assert_eq!(NodeMetrics::api_version(&()), "metrics.k8s.io/v1beta1");
        assert_eq!(NodeMetrics::kind(&()), "NodeMetrics");
    }

    #[test]
    fn a_sample_deserialises_the_way_metrics_server_sends_it() {
        // Verbatim from a `kubectl get --raw /apis/metrics.k8s.io/v1beta1/nodes`
        // item, extra fields included, since those must be ignored rather than
        // rejected.
        let json = r#"{
            "metadata": {"name": "ip-10-0-1-9.ec2.internal", "creationTimestamp": "2026-08-18T09:00:00Z"},
            "timestamp": "2026-08-18T09:00:00Z",
            "window": "20.04s",
            "usage": {"cpu": "412m", "memory": "3925716Ki"}
        }"#;

        let parsed: NodeMetrics = serde_json::from_str(json).unwrap();
        let usage = Usage::read(&parsed.usage);

        assert_eq!(
            parsed.metadata.name.as_deref(),
            Some("ip-10-0-1-9.ec2.internal")
        );
        assert_eq!(usage.cpu, Some(Quantity::parse("412m").unwrap()));
        assert_eq!(usage.memory.map(Quantity::units), Some(4_019_933_184));
    }

    #[test]
    fn usage_is_indexed_by_node_name() {
        let index = by_node(&[
            sample("node-a", "412m", "3925716Ki"),
            sample("node-b", "1200m", "8Gi"),
        ]);

        assert_eq!(index.len(), 2);
        assert_eq!(
            index["node-a"].usage.cpu,
            Some(Quantity::parse("412m").unwrap())
        );
        assert_eq!(
            index["node-b"].usage.memory,
            Some(Quantity::parse("8Gi").unwrap())
        );
    }

    #[test]
    fn a_sample_with_no_name_is_dropped_rather_than_indexed_under_nothing() {
        let mut nameless = sample("node-a", "1", "1Gi");
        nameless.metadata.name = None;
        let mut blank = sample("node-b", "1", "1Gi");
        blank.metadata.name = Some(String::new());

        assert!(by_node(&[nameless, blank]).is_empty());
    }

    #[test]
    fn an_unreadable_usage_figure_is_unknown_rather_than_zero() {
        // Zero would draw an idle node. "We do not know" is the truth, and the
        // renderer has a placeholder for it.
        let mut broken = sample("node-a", "lots", "3925716Ki");
        broken.usage.remove("memory");

        let usage = by_node(&[broken])["node-a"].usage;
        assert_eq!(usage.cpu, None);
        assert_eq!(usage.memory, None);
    }

    #[test]
    fn an_empty_sample_list_is_an_empty_index_not_a_failure() {
        assert!(by_node(&[]).is_empty());
    }

    #[tokio::test]
    async fn a_source_that_answers_is_indexed_straight_through() {
        let source = Fake::nodes(Ok(vec![sample("node-a", "412m", "3925716Ki")]));
        let index = usage_by_node(&source, page::Budget::default())
            .await
            .unwrap();

        assert_eq!(
            index["node-a"].usage.cpu,
            Some(Quantity::parse("412m").unwrap())
        );
    }

    #[tokio::test]
    async fn a_source_that_fails_hands_the_error_back_for_explaining() {
        let source = Fake::nodes(Err(404));
        let error = usage_by_node(&source, page::Budget::default())
            .await
            .expect_err("a 404 is not a usage listing");

        assert!(explain(&error, "prod (us-east-1)").contains("metrics-server"));
    }

    #[test]
    fn a_missing_metrics_api_says_what_to_install_rather_than_reporting_a_404() {
        let message = explain(&api_error(404, "404 page not found"), "prod (us-east-1)");

        assert!(message.contains("prod (us-east-1)"), "{message}");
        assert!(message.contains("metrics-server"), "{message}");
        assert!(
            message.contains("github.com/kubernetes-sigs/metrics-server"),
            "{message}"
        );
        assert!(
            !message.contains("404"),
            "raw HTTP status leaked: {message}"
        );
    }

    #[test]
    fn a_metrics_server_that_is_not_ready_yet_is_told_apart_from_a_missing_one() {
        let message = explain(&api_error(503, "service unavailable"), "prod (us-east-1)");

        assert!(message.contains("not answering yet"), "{message}");
        assert!(!message.contains("Install it"), "{message}");
    }

    #[test]
    fn other_failures_keep_the_advice_the_rest_of_the_tool_gives() {
        // No reason to invent a second vocabulary for an expired SSO session
        // just because it happened on the metrics endpoint.
        let expired = api_error(401, "Unauthorized");
        assert_eq!(
            explain(&expired, "prod (us-east-1)"),
            client::explain(&expired, "prod (us-east-1)")
        );
        assert!(explain(&expired, "prod").contains("aws sso login"));

        let forbidden = api_error(403, "Forbidden");
        assert!(explain(&forbidden, "prod").contains("access entry"));
    }

    #[test]
    fn a_metrics_endpoint_that_never_answers_says_how_long_it_waited() {
        // A metrics-server that has stopped answering used to hang the command
        // for ever, since the usage read is joined with the listing that is not
        // optional. It now costs the budget, and the footnote it earns has to
        // say so rather than falling into the "not installed" advice — the
        // endpoint may be there and simply unreachable.
        let error = page::Error::TimedOut {
            limit: std::time::Duration::from_secs(30),
        };

        let message = explain(&error, "prod (us-east-1)");
        assert!(message.contains("within 30s"), "{message}");
        assert!(message.contains("--timeout 1m"), "{message}");
        assert!(!message.contains("Install it"), "{message}");
    }

    #[test]
    fn the_pod_metrics_endpoint_is_namespaced_under_the_aggregated_group() {
        // Get this wrong and every cluster looks like it has no pod metrics.
        assert_eq!(
            PodMetrics::url_path(&(), None),
            "/apis/metrics.k8s.io/v1beta1/pods"
        );
        assert_eq!(
            PodMetrics::url_path(&(), Some("payments")),
            "/apis/metrics.k8s.io/v1beta1/namespaces/payments/pods"
        );
        assert_eq!(PodMetrics::kind(&()), "PodMetrics");
    }

    #[test]
    fn a_pod_sample_deserialises_the_way_metrics_server_sends_it() {
        // Verbatim from a `kubectl get --raw
        // /apis/metrics.k8s.io/v1beta1/namespaces/payments/pods` item, extra
        // fields included, since those must be ignored rather than rejected.
        let json = r#"{
            "metadata": {"name": "api-7c9f", "namespace": "payments", "creationTimestamp": "2026-08-18T09:00:00Z"},
            "timestamp": "2026-08-18T09:00:00Z",
            "window": "20.04s",
            "containers": [
                {"name": "app", "usage": {"cpu": "250m", "memory": "512Mi"}},
                {"name": "proxy", "usage": {"cpu": "12m", "memory": "64Mi"}}
            ]
        }"#;

        let parsed: PodMetrics = serde_json::from_str(json).unwrap();
        let usage = pod_usage(&parsed);

        assert_eq!(parsed.metadata.namespace.as_deref(), Some("payments"));
        assert_eq!(usage.cpu, Some(Quantity::parse("262m").unwrap()));
        assert_eq!(usage.memory, Some(Quantity::parse("576Mi").unwrap()));
    }

    #[test]
    fn a_pods_usage_is_the_sum_of_its_containers() {
        // The whole difference from the node shape: one row, several samples.
        let sample = pod_sample(
            "payments",
            "api-7c9f",
            vec![
                container("app", "250m", "512Mi"),
                container("sidecar", "50m", "64Mi"),
                container("proxy", "12m", "8Mi"),
            ],
        );

        let usage = pod_usage(&sample);

        assert_eq!(usage.cpu, Some(Quantity::parse("312m").unwrap()));
        assert_eq!(usage.memory, Some(Quantity::parse("584Mi").unwrap()));
    }

    #[test]
    fn a_single_container_pod_reads_as_that_container() {
        let usage = pod_usage(&pod_sample(
            "payments",
            "api-7c9f",
            vec![container("app", "250m", "512Mi")],
        ));

        assert_eq!(usage.cpu, Some(Quantity::parse("250m").unwrap()));
        assert_eq!(usage.memory, Some(Quantity::parse("512Mi").unwrap()));
    }

    #[test]
    fn a_pod_with_no_containers_sampled_is_unknown_rather_than_idle() {
        // What metrics-server sends for a pod it knows about but has not
        // scraped yet. Summing nothing gives zero, and zero would draw a pod
        // doing nothing — which is exactly the wrong answer during an incident.
        let usage = pod_usage(&pod_sample("payments", "api-7c9f", Vec::new()));

        assert_eq!(usage, Usage::default());
        assert_eq!(usage.cpu, None);
        assert_eq!(usage.memory, None);
    }

    #[test]
    fn one_unreadable_container_makes_the_whole_pod_unknown_for_that_resource() {
        // A sum that silently drops a container understates the pod, and the
        // shortfall is invisible on screen. The other resource is unaffected.
        let mut broken = pod_sample(
            "payments",
            "api-7c9f",
            vec![
                container("app", "250m", "512Mi"),
                container("proxy", "12m", "64Mi"),
            ],
        );
        broken.containers[1].usage.remove("cpu");

        let usage = pod_usage(&broken);

        assert_eq!(usage.cpu, None);
        assert_eq!(usage.memory, Some(Quantity::parse("576Mi").unwrap()));

        // A figure that will not parse is the same case as an absent one.
        broken.containers[1]
            .usage
            .insert("cpu".to_owned(), ApiQuantity("lots".to_owned()));
        assert_eq!(pod_usage(&broken).cpu, None);
    }

    #[test]
    fn usage_is_indexed_by_namespace_and_name_not_name_alone() {
        // Two pods called `coredns` in different namespaces is the ordinary
        // case, not a corner one, and `-A` puts them in the same table.
        let index = by_pod(&[
            pod_sample("kube-system", "coredns", vec![container("c", "5m", "20Mi")]),
            pod_sample("payments", "coredns", vec![container("c", "9m", "30Mi")]),
        ]);

        assert_eq!(index.len(), 2);
        assert_eq!(
            index[&("kube-system".to_owned(), "coredns".to_owned())]
                .usage
                .cpu,
            Some(Quantity::parse("5m").unwrap())
        );
        assert_eq!(
            index[&("payments".to_owned(), "coredns".to_owned())]
                .usage
                .cpu,
            Some(Quantity::parse("9m").unwrap())
        );
    }

    #[test]
    fn a_pod_sample_missing_half_its_identity_is_dropped() {
        let mut nameless = pod_sample("payments", "api", vec![container("c", "1", "1Gi")]);
        nameless.metadata.name = None;
        let mut homeless = pod_sample("payments", "api", vec![container("c", "1", "1Gi")]);
        homeless.metadata.namespace = Some(String::new());

        assert!(by_pod(&[nameless, homeless]).is_empty());
    }

    #[test]
    fn an_empty_pod_sample_list_is_an_empty_index_not_a_failure() {
        assert!(by_pod(&[]).is_empty());
    }

    #[test]
    fn a_label_selector_is_sent_to_the_metrics_api_but_a_field_selector_is_not() {
        // metrics-server filters on labels like any other API server, and does
        // not implement field filtering at all — the fields people select on are
        // not even on a `PodMetrics`. The rows are already filtered by both, and
        // the join is what narrows the usage.
        let params = pod_params(&Selectors {
            label: Some("app=api".to_owned()),
            field: Some("status.phase!=Running".to_owned()),
        });

        assert_eq!(params.label_selector.as_deref(), Some("app=api"));
        assert_eq!(params.field_selector, None);
    }

    #[test]
    fn no_selectors_leaves_the_metrics_listing_unfiltered() {
        let params = pod_params(&Selectors::default());

        assert_eq!(params.label_selector, None);
        assert_eq!(params.field_selector, None);
    }

    #[tokio::test]
    async fn a_pod_source_that_answers_is_indexed_straight_through() {
        let source = Fake::pods(Ok(vec![pod_sample(
            "payments",
            "api-7c9f",
            vec![container("app", "250m", "512Mi")],
        )]));

        let index = usage_by_pod(
            &source,
            &Scope::All,
            &Selectors::default(),
            page::Budget::default(),
        )
        .await
        .unwrap();

        assert_eq!(
            index[&("payments".to_owned(), "api-7c9f".to_owned())]
                .usage
                .cpu,
            Some(Quantity::parse("250m").unwrap())
        );
    }

    #[tokio::test]
    async fn a_pod_listing_with_no_metrics_server_hands_back_the_advice_to_install_one() {
        let source = Fake::pods(Err(404));
        let scope = Scope::Namespace("payments".to_owned());

        let error = usage_by_pod(
            &source,
            &scope,
            &Selectors::default(),
            page::Budget::default(),
        )
        .await
        .expect_err("a 404 is not a usage listing");

        assert!(explain(&error, "prod (us-east-1)").contains("metrics-server"));
    }

    // --- The averaging window, off the wire -------------------------------

    #[test]
    fn the_window_metrics_server_actually_sends_is_read_to_the_nanosecond() {
        // `20.04s` is what a default metrics-server reports, and it is not
        // representable in binary floating point — which is why this is parsed
        // with integers rather than through an `f64`.
        assert_eq!(
            parse_duration("20.04s"),
            Some(SignedDuration::from_nanos(20_040_000_000))
        );
    }

    #[test]
    fn every_unit_go_prints_a_duration_in_is_understood() {
        // Both spellings of the micro sign: Go emits U+00B5, and encoders that
        // normalise emit the visually identical Greek U+03BC.
        let cases = [
            ("1ns", SignedDuration::from_nanos(1)),
            ("1us", SignedDuration::from_nanos(1_000)),
            ("1\u{b5}s", SignedDuration::from_nanos(1_000)),
            ("1\u{3bc}s", SignedDuration::from_nanos(1_000)),
            ("1ms", SignedDuration::from_nanos(1_000_000)),
            ("1s", SignedDuration::from_secs(1)),
            ("1m", SignedDuration::from_secs(60)),
            ("1h", SignedDuration::from_secs(3_600)),
        ];

        for (text, expected) in cases {
            assert_eq!(parse_duration(text), Some(expected), "{text}");
        }
    }

    #[test]
    fn a_unit_is_never_read_as_a_shorter_one_it_ends_with() {
        // The bug this ordering exists to prevent: `ms` read as `m` turns half a
        // second into half an hour, and every listing then looks fresh forever.
        assert_eq!(
            parse_duration("500ms"),
            Some(SignedDuration::from_millis(500))
        );
        assert_eq!(
            parse_duration("500ns"),
            Some(SignedDuration::from_nanos(500))
        );
    }

    #[test]
    fn a_compound_duration_adds_its_parts_up() {
        // How Go prints anything over a minute, which is what a metrics-server
        // configured with a slower resolution reports.
        assert_eq!(parse_duration("1m0s"), Some(SignedDuration::from_secs(60)));
        assert_eq!(parse_duration("1m30s"), Some(SignedDuration::from_secs(90)));
        assert_eq!(
            parse_duration("1h2m3s"),
            Some(SignedDuration::from_secs(3_723))
        );
        assert_eq!(
            parse_duration("2m0.5s"),
            Some(SignedDuration::from_millis(120_500))
        );
    }

    #[test]
    fn a_leading_dot_is_the_zero_go_lets_you_leave_out() {
        assert_eq!(
            parse_duration(".5s"),
            Some(SignedDuration::from_millis(500))
        );
    }

    #[test]
    fn a_zero_window_is_read_rather_than_rejected() {
        // Go writes zero as `0s` and accepts a bare `0`. Reading it is not the
        // same as trusting it — see the staleness test below, which refuses to
        // judge an age against a window of nothing.
        assert_eq!(parse_duration("0"), Some(SignedDuration::ZERO));
        assert_eq!(parse_duration("0s"), Some(SignedDuration::ZERO));
    }

    #[test]
    fn a_signed_duration_keeps_its_sign() {
        assert_eq!(parse_duration("-20s"), Some(-SignedDuration::from_secs(20)));
        assert_eq!(parse_duration("+20s"), Some(SignedDuration::from_secs(20)));
    }

    #[test]
    fn precision_finer_than_a_nanosecond_is_dropped_rather_than_refused() {
        // Go truncates here too. Refusing the whole window over a digit nothing
        // can represent would cost the note for no gain.
        assert_eq!(
            parse_duration("1.9999999999s"),
            Some(SignedDuration::from_nanos(1_999_999_999))
        );
    }

    #[test]
    fn a_window_that_is_not_a_duration_is_no_window_rather_than_a_guess() {
        // Every one of these would otherwise become a number, and a wrong window
        // either accuses a healthy cluster of staleness or excuses a scraper
        // that has stopped.
        for text in [
            "",       // absent
            "20",     // a bare number: Go requires the unit
            "s",      // a bare unit
            "twenty", // prose
            "1x",     // a unit we do not know
            "1.2.3s", // two decimal points
            "1s2",    // a trailing number with nothing to scale it
            "-",      // a sign and nothing else
            "PT20S",  // ISO 8601, which this is deliberately not
        ] {
            assert_eq!(parse_duration(text), None, "{text:?}");
        }
    }

    #[test]
    fn a_duration_too_large_to_count_in_nanoseconds_is_refused_not_wrapped() {
        // Overflow reads as "we could not tell", which costs the note. Wrapping
        // would read as a plausible small window and quietly change the verdict.
        assert_eq!(parse_duration("9223372036854775807h"), None);
        assert_eq!(parse_duration("9999999999999999999999s"), None);
    }

    #[test]
    fn a_sample_carries_the_stamps_it_arrived_with() {
        let index = by_node(&[sample("node-a", "412m", "3925716Ki")]);

        assert_eq!(
            index["node-a"].taken_at,
            Some(now() - SignedDuration::from_secs(8))
        );
        assert_eq!(
            index["node-a"].window,
            Some(SignedDuration::from_nanos(20_040_000_000))
        );
    }

    #[test]
    fn a_pod_sample_carries_the_stamps_it_arrived_with() {
        // The pod half stamps the pod, not each container, so a pod with three
        // containers is still one sample with one age.
        let index = by_pod(&[pod_sample(
            "payments",
            "api-7c9f",
            vec![container("app", "250m", "512Mi")],
        )]);

        let sample = index[&("payments".to_owned(), "api-7c9f".to_owned())];
        assert_eq!(sample.taken_at, Some(now() - SignedDuration::from_secs(8)));
        assert_eq!(
            sample.window,
            Some(SignedDuration::from_nanos(20_040_000_000))
        );
    }

    #[test]
    fn a_sample_with_unreadable_stamps_keeps_its_figures() {
        // The stamps are the sampler's word, and losing them costs the note
        // rather than the reading — a figure with no date is still a figure.
        let mut odd = sample("node-a", "412m", "3925716Ki");
        odd.timestamp = None;
        odd.window = Some("whenever".to_owned());

        let read = by_node(&[odd])["node-a"];

        assert_eq!(read.usage.cpu, Some(Quantity::parse("412m").unwrap()));
        assert_eq!(read.taken_at, None);
        assert_eq!(read.window, None);
    }

    // --- Dating a listing --------------------------------------------------

    #[test]
    fn a_listing_is_dated_by_its_oldest_sample() {
        // The note is a guarantee about the whole table, so it is only as good
        // as the worst row: "up to 5m old" must cover the 5m one.
        let samples = [
            stamped(Some(now() - SignedDuration::from_secs(10)), Some("20s")),
            stamped(Some(now() - SignedDuration::from_secs(300)), Some("20s")),
            stamped(Some(now() - SignedDuration::from_secs(45)), Some("20s")),
        ];

        let freshness = freshness(&samples, now()).expect("a stamped listing can be dated");

        assert_eq!(freshness.age, SignedDuration::from_secs(300));
    }

    #[test]
    fn the_window_reported_is_the_longest_any_sample_gave() {
        // Disagreeing windows mean two scrapers, or one being reconfigured. The
        // slower of them is what decides how long "up to date" lasts.
        let samples = [
            stamped(Some(now()), Some("20s")),
            stamped(Some(now()), Some("1m0s")),
        ];

        let freshness = freshness(&samples, now()).expect("a stamped listing can be dated");

        assert_eq!(freshness.window, Some(SignedDuration::from_secs(60)));
    }

    #[test]
    fn a_listing_nothing_stamped_cannot_be_dated_and_says_nothing() {
        // Half a note is worse than none: "averaged over 20s" with no age beside
        // it would read as a claim that the figures are current.
        let samples = [stamped(None, Some("20s")), stamped(None, None)];

        assert_eq!(freshness(&samples, now()), None);
    }

    #[test]
    fn an_empty_listing_has_no_freshness_to_report() {
        assert_eq!(freshness(&[], now()), None);
    }

    #[test]
    fn one_stamped_sample_dates_a_listing_the_rest_of_which_is_undated() {
        // `any` rather than `all`, matching the rule the usage columns
        // themselves follow: one unstamped row does not cost everyone the note.
        let samples = [
            stamped(None, None),
            stamped(Some(now() - SignedDuration::from_secs(30)), Some("20s")),
        ];

        let freshness = freshness(&samples, now()).expect("one stamp is enough");

        assert_eq!(freshness.age, SignedDuration::from_secs(30));
    }

    #[test]
    fn a_sample_stamped_in_the_future_reads_as_no_age_rather_than_a_negative_one() {
        // Clock skew between the API server and here. `human_duration` already
        // treats a node created in the future the same way.
        let samples = [stamped(
            Some(now() + SignedDuration::from_secs(30)),
            Some("20s"),
        )];

        let freshness = freshness(&samples, now()).expect("a stamped listing can be dated");

        assert_eq!(freshness.age, SignedDuration::ZERO);
        assert!(!freshness.is_stale());
    }

    // --- Staleness ---------------------------------------------------------

    #[test]
    fn a_listing_within_a_couple_of_windows_is_not_stale() {
        // One window of lag is what a working scraper looks like.
        for seconds in [0, 20, 39, 40] {
            let samples = [stamped(
                Some(now() - SignedDuration::from_secs(seconds)),
                Some("20s"),
            )];
            let freshness = freshness(&samples, now()).expect("a stamped listing can be dated");

            assert!(!freshness.is_stale(), "{seconds}s");
        }
    }

    #[test]
    fn a_listing_older_than_a_couple_of_windows_is_stale() {
        // Two windows behind means a scrape did not happen, which is what a
        // stopped metrics-server looks like from here: the request still
        // succeeds, and the figures stop moving.
        for seconds in [41, 300, 86_400] {
            let samples = [stamped(
                Some(now() - SignedDuration::from_secs(seconds)),
                Some("20s"),
            )];
            let freshness = freshness(&samples, now()).expect("a stamped listing can be dated");

            assert!(freshness.is_stale(), "{seconds}s");
        }
    }

    #[test]
    fn a_listing_with_no_window_is_never_accused_of_being_stale() {
        // Without a window there is no scale to judge an age against, and
        // "your figures are stale" is not a sentence to print on a guess.
        let samples = [stamped(
            Some(now() - SignedDuration::from_secs(86_400)),
            None,
        )];

        let freshness = freshness(&samples, now()).expect("a stamped listing can be dated");

        assert!(!freshness.is_stale());
    }

    #[test]
    fn a_window_of_zero_is_no_scale_to_judge_staleness_by_either() {
        // `0s` would make every listing older than an instant stale, including
        // the one taken half a second ago.
        let samples = [stamped(
            Some(now() - SignedDuration::from_secs(1)),
            Some("0s"),
        )];

        let freshness = freshness(&samples, now()).expect("a stamped listing can be dated");

        assert!(!freshness.is_stale());
    }

    // --- What the table says -----------------------------------------------

    #[test]
    fn the_freshness_note_gives_the_age_and_the_window_it_covers() {
        let samples = [stamped(
            Some(now() - SignedDuration::from_secs(14)),
            Some("20.04s"),
        )];
        let freshness = freshness(&samples, now()).expect("a stamped listing can be dated");

        assert_eq!(
            freshness_note(freshness),
            "Usage is up to 14s old, averaged over 20s."
        );
    }

    #[test]
    fn a_stale_listing_says_so_and_says_what_to_go_and_look_at() {
        // The diagnosis on its own would leave the reader with a number and no
        // next command; the second line is the point of the first.
        let samples = [stamped(
            Some(now() - SignedDuration::from_secs(370)),
            Some("20s"),
        )];
        let freshness = freshness(&samples, now()).expect("a stamped listing can be dated");

        let note = freshness_note(freshness);

        assert!(note.contains("6m10s"), "{note}");
        assert!(note.contains("stale"), "{note}");
        assert!(note.contains("kube-system"), "{note}");
    }

    #[test]
    fn a_listing_with_no_window_dates_itself_without_claiming_one() {
        let samples = [stamped(Some(now() - SignedDuration::from_secs(14)), None)];
        let freshness = freshness(&samples, now()).expect("a stamped listing can be dated");

        assert_eq!(freshness_note(freshness), "Usage is up to 14s old.");
    }

    #[test]
    fn a_fresh_listing_never_reads_as_a_warning() {
        // This line is under every table on a healthy cluster, so it has to be
        // a fact and not an alarm.
        let samples = [stamped(
            Some(now() - SignedDuration::from_secs(8)),
            Some("20s"),
        )];
        let freshness = freshness(&samples, now()).expect("a stamped listing can be dated");

        let note = freshness_note(freshness);

        assert!(!note.contains("stale"), "{note}");
        assert!(!note.contains("kube-system"), "{note}");
        assert_eq!(note.lines().count(), 1, "{note}");
    }

    #[test]
    fn an_unsampled_listing_says_metrics_server_is_there_rather_than_missing() {
        // The whole difference from the 404 footnote beside it: telling the user
        // to install something they already have sends them the wrong way.
        let message = unsampled("prod (us-east-1)");

        assert!(message.contains("prod (us-east-1)"), "{message}");
        assert!(message.contains("installed"), "{message}");
        assert!(message.contains("kube-system"), "{message}");
        assert!(
            !message.contains("github.com/kubernetes-sigs/metrics-server"),
            "advice to install what is already installed: {message}"
        );
    }

    // --- Which of the three cases a listing is in --------------------------

    #[test]
    fn usage_that_reached_the_rows_is_shown_and_wants_dating() {
        let read = by_node(&[sample("node-a", "412m", "3925716Ki")]);

        assert_eq!(Outcome::of(Some(&read), true), Outcome::Shown);
        assert!(!Outcome::of(Some(&read), true).is_missing());
    }

    #[test]
    fn a_read_that_answered_with_nothing_is_told_apart_from_one_that_failed() {
        // The case this task exists for. Both lose the columns; only one of them
        // means metrics-server is missing.
        let empty = by_node(&[]);

        assert_eq!(Outcome::of(Some(&empty), false), Outcome::Unsampled);
        assert_eq!(
            Outcome::of(None::<&BTreeMap<String, Sample>>, false),
            Outcome::Unreadable
        );
    }

    #[test]
    fn both_ways_of_losing_the_columns_count_as_missing() {
        // Which is what the "nothing ranked" note asks, since both now leave a
        // footnote above the table for it to point at.
        assert!(Outcome::Unsampled.is_missing());
        assert!(Outcome::Unreadable.is_missing());
    }

    #[test]
    fn samples_that_no_row_shows_are_not_samples_this_listing_has() {
        // `eks pods --field-selector spec.nodeName=...` narrows the rows but not
        // the metrics request, so the reply can be full of readings for pods the
        // table does not contain. Dating the table from those would put an age
        // under a listing they are not in.
        let read = by_pod(&[pod_sample(
            "payments",
            "elsewhere",
            vec![container("app", "250m", "512Mi")],
        )]);

        assert_eq!(Outcome::of(Some(&read), false), Outcome::Unsampled);
    }
}
