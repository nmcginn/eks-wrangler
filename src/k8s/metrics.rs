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
//! Fetching sits behind [`Source`] so the interesting paths — no
//! metrics-server, a node the sampler has not reached yet, a usage figure that
//! will not parse — are fixtures rather than a cluster somebody has to break.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::future::Future;

use k8s_openapi::ClusterResourceScope;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity as ApiQuantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, ListParams};
use kube::{Client, Resource};
use serde::Deserialize;

use crate::k8s::client;
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
}

/// The real thing: `metrics.k8s.io` on a live cluster.
///
/// The only implementation that touches the network.
impl Source for Client {
    fn node_usage(&self) -> impl Future<Output = Result<Vec<NodeMetrics>, kube::Error>> + Send {
        let api: Api<NodeMetrics> = Api::all(self.clone());
        async move { Ok(api.list(&ListParams::default()).await?.items) }
    }
}

/// Fetch node usage and index it by node name.
///
/// Generic over [`Source`] so the command layer's happy path and its
/// degraded one are both reachable from a test without a cluster.
pub async fn usage_by_node<S: Source>(source: &S) -> Result<BTreeMap<String, Usage>, kube::Error> {
    Ok(by_node(&source.node_usage().await?))
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

    /// A [`Source`] that answers from a fixture, so the absent-metrics-server
    /// path is a test rather than a cluster somebody has to uninstall from.
    struct Fake(Result<Vec<NodeMetrics>, u16>);

    impl Source for Fake {
        fn node_usage(&self) -> impl Future<Output = Result<Vec<NodeMetrics>, kube::Error>> + Send {
            let answer = match &self.0 {
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
        let source = Fake(Ok(vec![sample("node-a", "412m", "3925716Ki")]));
        let index = usage_by_node(&source).await.unwrap();

        assert_eq!(index["node-a"].cpu, Some(Quantity::parse("412m").unwrap()));
    }

    #[tokio::test]
    async fn a_source_that_fails_hands_the_error_back_for_explaining() {
        let source = Fake(Err(404));
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
}
