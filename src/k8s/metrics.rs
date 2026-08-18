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
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::{ClusterResourceScope, NamespaceResourceScope};
use kube::api::{Api, ListParams};
use kube::{Client, Resource};
use serde::Deserialize;

use crate::k8s::client;
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
pub fn by_pod(metrics: &[PodMetrics]) -> BTreeMap<PodKey, Usage> {
    metrics
        .iter()
        .filter_map(|sample| {
            let namespace = sample
                .metadata
                .namespace
                .as_deref()
                .filter(|ns| !ns.is_empty())?;
            let name = sample.metadata.name.as_deref().filter(|n| !n.is_empty())?;
            Some(((namespace.to_owned(), name.to_owned()), pod_usage(sample)))
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
    fn node_usage(&self) -> impl Future<Output = Result<Vec<NodeMetrics>, kube::Error>> + Send;

    /// Usage for the pods in `scope`, narrowed by the label half of
    /// `selectors` — see [`pod_params`] for why only that half.
    fn pod_usage(
        &self,
        scope: &Scope,
        selectors: &Selectors,
    ) -> impl Future<Output = Result<Vec<PodMetrics>, kube::Error>> + Send;
}

/// The real thing: `metrics.k8s.io` on a live cluster.
///
/// The only implementation that touches the network.
impl Source for Client {
    fn node_usage(&self) -> impl Future<Output = Result<Vec<NodeMetrics>, kube::Error>> + Send {
        let api: Api<NodeMetrics> = Api::all(self.clone());
        async move { Ok(api.list(&ListParams::default()).await?.items) }
    }

    fn pod_usage(
        &self,
        scope: &Scope,
        selectors: &Selectors,
    ) -> impl Future<Output = Result<Vec<PodMetrics>, kube::Error>> + Send {
        let api: Api<PodMetrics> = match scope {
            Scope::Namespace(name) => Api::namespaced(self.clone(), name),
            Scope::All => Api::all(self.clone()),
        };
        let params = pod_params(selectors);
        async move { Ok(api.list(&params).await?.items) }
    }
}

/// Fetch node usage and index it by node name.
///
/// Generic over [`Source`] so the command layer's happy path and its
/// degraded one are both reachable from a test without a cluster.
pub async fn usage_by_node<S: Source>(source: &S) -> Result<BTreeMap<String, Usage>, kube::Error> {
    Ok(by_node(&source.node_usage().await?))
}

/// Fetch pod usage for `scope` and index it by namespace and name.
///
/// Generic over [`Source`] for the same reason as [`usage_by_node`]: the paths
/// worth testing are the ones a healthy cluster will not produce.
pub async fn usage_by_pod<S: Source>(
    source: &S,
    scope: &Scope,
    selectors: &Selectors,
) -> Result<BTreeMap<PodKey, Usage>, kube::Error> {
    Ok(by_pod(&source.pod_usage(scope, selectors).await?))
}

/// Index a usage listing by node name.
///
/// A sample with no name in it is dropped: there is no row it could belong to,
/// and guessing would be worse than the `-` the caller already renders for a
/// node the sampler has not reached.
#[must_use]
pub fn by_node(metrics: &[NodeMetrics]) -> BTreeMap<String, Usage> {
    metrics
        .iter()
        .filter_map(|sample| {
            let name = sample.metadata.name.as_deref().filter(|n| !n.is_empty())?;
            Some((name.to_owned(), Usage::read(&sample.usage)))
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
pub fn explain(error: &kube::Error, cluster: &str) -> String {
    match error {
        // The aggregation layer answers for a group nobody registered with a
        // 404 — sometimes as a `Status`, sometimes as a bare `404 page not
        // found` that `kube` reconstructs into one. Either way this is the
        // fresh-EKS-cluster case, and it is not an error the user made.
        kube::Error::Api(status) if status.code == 404 => format!(
            "{cluster} has no metrics.k8s.io API, so metrics-server does not appear to be installed.\n\
             Install it to see live usage: https://github.com/kubernetes-sigs/metrics-server"
        ),
        // Registered but not serving: metrics-server refuses to answer until it
        // has scraped every node once, which takes a minute or so after it
        // starts and forever if it cannot reach the kubelets.
        kube::Error::Api(status) if status.code == 503 => format!(
            "metrics-server is registered on {cluster} but is not answering yet.\n\
             It stays unavailable until it has scraped every node once — give it a minute, \
             then check its pod in kube-system if it does not settle."
        ),
        other => client::explain(other, cluster),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use kube::core::Status;

    use super::*;

    fn api_error(code: u16, message: &str) -> kube::Error {
        kube::Error::Api(Status::failure(message, "Failure").with_code(code).boxed())
    }

    fn sample(name: &str, cpu: &str, memory: &str) -> NodeMetrics {
        NodeMetrics {
            metadata: ObjectMeta {
                name: Some(name.to_owned()),
                ..Default::default()
            },
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
            containers,
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
        fn node_usage(&self) -> impl Future<Output = Result<Vec<NodeMetrics>, kube::Error>> + Send {
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
        ) -> impl Future<Output = Result<Vec<PodMetrics>, kube::Error>> + Send {
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
        assert_eq!(index["node-a"].cpu, Some(Quantity::parse("412m").unwrap()));
        assert_eq!(
            index["node-b"].memory,
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

        let usage = by_node(&[broken])["node-a"];
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
        let index = usage_by_node(&source).await.unwrap();

        assert_eq!(index["node-a"].cpu, Some(Quantity::parse("412m").unwrap()));
    }

    #[tokio::test]
    async fn a_source_that_fails_hands_the_error_back_for_explaining() {
        let source = Fake::nodes(Err(404));
        let error = usage_by_node(&source)
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
            index[&("kube-system".to_owned(), "coredns".to_owned())].cpu,
            Some(Quantity::parse("5m").unwrap())
        );
        assert_eq!(
            index[&("payments".to_owned(), "coredns".to_owned())].cpu,
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

        let index = usage_by_pod(&source, &Scope::All, &Selectors::default())
            .await
            .unwrap();

        assert_eq!(
            index[&("payments".to_owned(), "api-7c9f".to_owned())].cpu,
            Some(Quantity::parse("250m").unwrap())
        );
    }

    #[tokio::test]
    async fn a_pod_listing_with_no_metrics_server_hands_back_the_advice_to_install_one() {
        let source = Fake::pods(Err(404));
        let scope = Scope::Namespace("payments".to_owned());

        let error = usage_by_pod(&source, &scope, &Selectors::default())
            .await
            .expect_err("a 404 is not a usage listing");

        assert!(explain(&error, "prod (us-east-1)").contains("metrics-server"));
    }
}
