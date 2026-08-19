//! `eks nodes` — the nodes of one cluster, as a table.

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use k8s_openapi::jiff::Timestamp;

use crate::cluster::ClusterView;
use crate::commands::contexts;
use crate::format::Width;
use crate::k8s::metrics::{self as k8s_metrics};
use crate::k8s::order::Direction;
use crate::k8s::{self, nodes as k8s_nodes, pods as k8s_pods};
use crate::kubeconfig::KubeConfig;

/// Fetch and render the node table for the selected cluster.
///
/// `selector` is whatever the user passed to `--context`: a full context name,
/// or the short cluster name `eks contexts` shows. `None` means the cluster
/// their kubeconfig already points at.
///
/// `order` and `direction` are `--sort` and `--sort-reverse`. They are applied
/// to the finished rows, so they change nothing about what is fetched — only
/// the order it is read in. `width` is `--wide`, and is the same again: every
/// column it adds arrived with the nodes, so it costs no extra request.
pub async fn list(
    config: &KubeConfig,
    paths: &[PathBuf],
    selector: Option<&str>,
    order: k8s_nodes::Order,
    direction: Direction,
    width: Width,
) -> Result<String> {
    let target = target_cluster(config, selector)?;
    let label = target.label();

    let client = k8s::connect(paths, &target).await?;

    // Concurrently, not in sequence: the three requests are independent, and the
    // command should cost one round trip's worth of waiting rather than three.
    let (nodes, pods, usage) = tokio::join!(
        k8s_nodes::fetch(client.clone()),
        k8s_pods::fetch(client.clone()),
        k8s_metrics::usage_by_node(&client),
    );

    let nodes = nodes.map_err(|error| {
        // The raw error is worth having when debugging, but it is not what the
        // user needs to read; `-vv` brings it back.
        tracing::debug!(%error, "listing nodes failed");
        anyhow!(k8s::explain(&error, &label))
    })?;

    // Only the node listing is fatal. The other two each cost the user some
    // columns and earn a footnote, because a partial answer beats no answer:
    // a read-only role that grants nodes but not pods across every namespace is
    // common, and metrics-server is an add-on EKS does not install for you.
    let mut footnotes = Vec::new();

    let requests = match pods {
        Ok(pods) => Some(k8s_pods::by_node(&pods)),
        Err(error) => {
            tracing::debug!(%error, "listing pods failed");
            footnotes.push(k8s_nodes::requests_unavailable(&k8s::explain(
                &error, &label,
            )));
            None
        }
    };

    let usage = match usage {
        Ok(usage) => Some(usage),
        Err(error) => {
            tracing::debug!(%error, "reading node metrics failed");
            footnotes.push(k8s_nodes::usage_unavailable(&k8s_metrics::explain(
                &error, &label,
            )));
            None
        }
    };

    // One instant for every row, so a slow listing cannot show two nodes
    // created together with different ages.
    let now = Timestamp::now();
    let mut rows: Vec<k8s_nodes::NodeRow> = nodes
        .iter()
        .map(|node| {
            // A node absent from the totals is running nothing, which is a real
            // zero. Only a failed pod listing leaves the figure unknown.
            let requested = requests.as_ref().map(|totals| {
                node.metadata
                    .name
                    .as_deref()
                    .and_then(|name| totals.get(name))
                    .copied()
                    .unwrap_or_default()
            });
            // Unlike the requests, an absent node here is *not* a zero: it is a
            // node metrics-server has not sampled yet, and drawing it as idle
            // would be an invention. `None` reads as `-`.
            let used = usage.as_ref().and_then(|samples| {
                node.metadata
                    .name
                    .as_deref()
                    .and_then(|name| samples.get(name))
                    .copied()
            });
            k8s_nodes::NodeRow::from_node(node, requested, used, now)
        })
        .collect();
    // Ordering lives in `k8s::nodes::order` rather than here, so the default and
    // the one `--sort` asks for are decided in the same place and by the same
    // rules — and so both can be tested on rows alone. The default is still by
    // name, which the API server happens to return today; sorting makes that a
    // promise rather than an accident.
    k8s_nodes::sort(&mut rows, order, direction);

    // Last of the footnotes, under whatever went wrong: a table nobody could
    // fill in is more urgent news than the order it came out in. The note is
    // silent unless `--sort` or `--sort-reverse` was given, so a plain
    // `eks nodes` prints exactly what it printed before.
    footnotes.extend(k8s::order::note(order, direction));
    // And immediately under it, the case where that line on its own misleads:
    // `--sort cpu` against a cluster with no metrics-server names an ordering
    // over a column this table does not have. Both halves the note cannot work
    // out for itself come from the listing: which orderings these rows can be
    // ranked by, and whether one of the footnotes above already accounts for
    // the column that came up empty — in which case the note points at it
    // rather than repeating the advice a paragraph later.
    let missing = k8s_nodes::Missing {
        requests: requests.is_none(),
        usage: usage.is_none(),
    };
    footnotes.extend(k8s::order::unranked_note(
        order,
        k8s_nodes::cause(order, missing),
        |candidate| k8s_nodes::ranks_any(&rows, candidate),
    ));

    Ok(k8s_nodes::render(&rows, &label, &footnotes, width))
}

/// Work out which cluster to talk to, before any network call happens.
///
/// Kept separate from the fetching so the "which cluster did you mean?"
/// answers — including the unhelpful ones — are testable without a cluster.
pub fn target_cluster(config: &KubeConfig, selector: Option<&str>) -> Result<ClusterView> {
    let views = contexts::views(config);

    let Some(name) = selector else {
        let current = config.current().ok_or_else(|| match &config.current_context {
            Some(name) => anyhow!(
                "current-context is set to {name:?}, but no such context exists in your kubeconfig.\n\
                 Run `eks contexts` to see what is available, then `eks use <name>`."
            ),
            None => {
                anyhow!("no current context is set; run `eks use <name>` or pass `--context <name>`")
            }
        })?;
        return Ok(ClusterView::from_context(&current));
    };

    contexts::resolve_selector(&views, name).cloned()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const CONFIG: &str = r"
apiVersion: v1
kind: Config
current-context: arn:aws:eks:us-east-1:111122223333:cluster/prod
clusters:
  - name: arn:aws:eks:us-east-1:111122223333:cluster/prod
    cluster:
      server: https://ABC.gr7.us-east-1.eks.amazonaws.com
  - name: arn:aws:eks:eu-west-1:111122223333:cluster/staging
    cluster:
      server: https://DEF.gr7.eu-west-1.eks.amazonaws.com
contexts:
  - name: arn:aws:eks:us-east-1:111122223333:cluster/prod
    context:
      cluster: arn:aws:eks:us-east-1:111122223333:cluster/prod
      user: prod
  - name: arn:aws:eks:eu-west-1:111122223333:cluster/staging
    context:
      cluster: arn:aws:eks:eu-west-1:111122223333:cluster/staging
      user: staging
";

    fn config(yaml: &str) -> KubeConfig {
        KubeConfig::parse(yaml).unwrap()
    }

    #[test]
    fn with_no_selector_the_current_context_is_used() {
        let target = target_cluster(&config(CONFIG), None).unwrap();

        assert_eq!(target.display_name, "prod");
        assert_eq!(target.label(), "prod (us-east-1)");
    }

    #[test]
    fn a_short_cluster_name_selects_a_context() {
        // Nobody should have to type an ARN to look at a cluster.
        let target = target_cluster(&config(CONFIG), Some("staging")).unwrap();

        assert_eq!(
            target.context_name,
            "arn:aws:eks:eu-west-1:111122223333:cluster/staging"
        );
    }

    #[test]
    fn a_selector_that_is_nearly_right_gets_a_suggestion() {
        let error = target_cluster(&config(CONFIG), Some("pro")).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("\"pro\""), "{message}");
        assert!(message.contains("Did you mean: prod"), "{message}");
    }

    #[test]
    fn a_selector_matching_nothing_points_at_the_context_list() {
        // No fuzzy matching yet, so a typo like this has nothing to suggest;
        // it must still say where to look rather than just failing.
        let error = target_cluster(&config(CONFIG), Some("prd")).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("prd"), "{message}");
        assert!(message.contains("eks contexts"), "{message}");
    }

    #[test]
    fn an_empty_kubeconfig_says_how_to_pick_a_cluster() {
        let error = target_cluster(&config(""), None).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("no current context"), "{message}");
        assert!(message.contains("eks use"), "{message}");
    }

    #[test]
    fn a_dangling_current_context_names_the_context_that_went_missing() {
        let error = target_cluster(&config("current-context: gone\n"), None).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("\"gone\""), "{message}");
        assert!(message.contains("eks contexts"), "{message}");
    }
}
