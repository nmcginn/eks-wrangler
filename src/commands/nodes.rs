//! `eks nodes` — the nodes of one cluster, as a table.

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use k8s_openapi::jiff::Timestamp;

use crate::cluster::ClusterView;
use crate::commands::contexts;
use crate::k8s::{self, nodes as k8s_nodes};
use crate::kubeconfig::KubeConfig;

/// Fetch and render the node table for the selected cluster.
///
/// `selector` is whatever the user passed to `--context`: a full context name,
/// or the short cluster name `eks contexts` shows. `None` means the cluster
/// their kubeconfig already points at.
pub async fn list(
    config: &KubeConfig,
    paths: &[PathBuf],
    selector: Option<&str>,
) -> Result<String> {
    let target = target_cluster(config, selector)?;
    let label = target.label();

    let client = k8s::connect(paths, &target).await?;

    let nodes = k8s_nodes::fetch(client).await.map_err(|error| {
        // The raw error is worth having when debugging, but it is not what the
        // user needs to read; `-vv` brings it back.
        tracing::debug!(%error, "listing nodes failed");
        anyhow!(k8s::explain(&error, &label))
    })?;

    // One instant for every row, so a slow listing cannot show two nodes
    // created together with different ages.
    let now = Timestamp::now();
    let mut rows: Vec<k8s_nodes::NodeRow> = nodes
        .iter()
        .map(|node| k8s_nodes::NodeRow::from_node(node, now))
        .collect();
    // The API server happens to return nodes in name order today; sorting makes
    // that a promise rather than an accident.
    rows.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(k8s_nodes::render(&rows, &label))
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
