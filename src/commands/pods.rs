//! `eks pods` — the pods of one namespace, or of every namespace, as a table.

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use k8s_openapi::jiff::Timestamp;

use crate::cluster::ClusterView;
use crate::commands::nodes::target_cluster;
use crate::k8s::pods::{PodRow, Scope};
use crate::k8s::{self, pods as k8s_pods};
use crate::kubeconfig::KubeConfig;

/// Fetch and render the pod table for the selected cluster and scope.
///
/// `selector` is whatever the user passed to `--context`, resolved exactly as
/// `eks nodes` resolves it. `namespace` is `--namespace` when they gave one;
/// without it the context's own namespace is used, which is what a bare
/// `kubectl get pods` would do.
pub async fn list(
    config: &KubeConfig,
    paths: &[PathBuf],
    selector: Option<&str>,
    namespace: Option<&str>,
    all_namespaces: bool,
) -> Result<String> {
    let target = target_cluster(config, selector)?;
    let label = target.label();
    // Resolved before connecting: a contradictory pair of flags should be
    // rejected instantly, not after a credential helper has run.
    let scope = scope_for(&target, namespace, all_namespaces)?;

    let client = k8s::connect(paths, &target).await?;
    let pods = k8s_pods::fetch_scope(client, &scope)
        .await
        .map_err(|error| {
            // The raw error is worth having when debugging, but it is not what
            // the user needs to read; `-vv` brings it back.
            tracing::debug!(%error, "listing pods failed");
            let explanation = k8s::explain(&error, &label);
            anyhow!(match k8s::Failure::of(&error) {
                k8s::Failure::Forbidden => denied(&explanation, &scope),
                _ => explanation,
            })
        })?;

    // One instant for every row, so a slow listing cannot show two pods created
    // together with different ages.
    let now = Timestamp::now();
    let mut rows: Vec<PodRow> = pods.iter().map(|pod| PodRow::from_pod(pod, now)).collect();
    // Namespace first, then name: the order the NAMESPACE column implies, and
    // the one `kubectl get pods -A` uses.
    rows.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));

    Ok(k8s_pods::render(&rows, &label, &scope))
}

/// What to say when the cluster refuses the listing.
///
/// A `403` on `--all-namespaces` is often not a missing permission so much as
/// the wrong question: read-only access is frequently bound to a single
/// namespace, and the cluster-wide list is the one call such a role cannot
/// serve. Saying so turns a dead end into the next command to type.
#[must_use]
pub fn denied(explanation: &str, scope: &Scope) -> String {
    match scope {
        Scope::All => format!(
            "{explanation}\n\
             If your access is scoped to one namespace, ask for that one: `eks pods -n <namespace>`."
        ),
        Scope::Namespace(_) => explanation.to_owned(),
    }
}

/// Which pods the user asked for.
///
/// Kept separate from the fetching so the answer — including the unhelpful
/// one — is settled without a cluster.
pub fn scope_for(
    target: &ClusterView,
    namespace: Option<&str>,
    all_namespaces: bool,
) -> Result<Scope> {
    match (namespace, all_namespaces) {
        // Quietly ignoring one of them would leave the user reading the wrong
        // list and believing it was the one they asked for.
        (Some(name), true) => Err(anyhow!(
            "`--namespace {name}` and `--all-namespaces` ask for different things.\n\
             Drop one: `--all-namespaces` for the whole cluster, `-n {name}` for just that namespace."
        )),
        (_, true) => Ok(Scope::All),
        (Some(name), false) => Ok(Scope::Namespace(name.to_owned())),
        // The namespace the context itself points at — `default` unless the
        // kubeconfig says otherwise — so `eks pods` and `kubectl get pods`
        // agree about what "here" means.
        (None, false) => Ok(Scope::Namespace(target.namespace.clone())),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const CONFIG: &str = r"
apiVersion: v1
kind: Config
current-context: prod
clusters:
  - name: prod
    cluster:
      server: https://ABC.gr7.us-east-1.eks.amazonaws.com
  - name: scoped
    cluster:
      server: https://DEF.gr7.eu-west-1.eks.amazonaws.com
contexts:
  - name: prod
    context:
      cluster: prod
      user: prod
  - name: scoped
    context:
      cluster: scoped
      user: scoped
      namespace: payments
";

    fn target(name: &str) -> ClusterView {
        let config = KubeConfig::parse(CONFIG).unwrap();
        target_cluster(&config, Some(name)).unwrap()
    }

    #[test]
    fn without_a_flag_the_contexts_own_namespace_is_used() {
        // Matching `kubectl get pods`, which reads the same field.
        assert_eq!(
            scope_for(&target("scoped"), None, false).unwrap(),
            Scope::Namespace("payments".to_owned())
        );
    }

    #[test]
    fn a_context_with_no_namespace_falls_back_to_default() {
        assert_eq!(
            scope_for(&target("prod"), None, false).unwrap(),
            Scope::Namespace("default".to_owned())
        );
    }

    #[test]
    fn an_explicit_namespace_overrides_the_contexts_own() {
        assert_eq!(
            scope_for(&target("scoped"), Some("storefront"), false).unwrap(),
            Scope::Namespace("storefront".to_owned())
        );
    }

    #[test]
    fn all_namespaces_ignores_the_contexts_namespace() {
        assert_eq!(
            scope_for(&target("scoped"), None, true).unwrap(),
            Scope::All
        );
    }

    #[test]
    fn a_refused_cluster_wide_listing_suggests_narrowing_to_a_namespace() {
        // The common EKS shape: a role bound to one namespace, and the only
        // call it cannot serve is the one the user just made.
        let refused = denied(
            "prod (us-east-1) will not let you list this resource.",
            &Scope::All,
        );

        assert!(refused.contains("will not let you list"), "{refused}");
        assert!(refused.contains("eks pods -n"), "{refused}");
    }

    #[test]
    fn a_refused_namespaced_listing_has_nothing_extra_to_suggest() {
        // Narrowing further is not an option, and inventing advice would only
        // send the user round in a circle.
        let explanation = "prod (us-east-1) will not let you list this resource.";
        let refused = denied(explanation, &Scope::Namespace("payments".to_owned()));

        assert_eq!(refused, explanation);
    }

    #[test]
    fn asking_for_one_namespace_and_all_of_them_is_an_error_with_a_way_out() {
        let error = scope_for(&target("prod"), Some("payments"), true).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("payments"), "{message}");
        assert!(message.contains("--all-namespaces"), "{message}");
        assert!(message.contains("Drop one"), "{message}");
    }
}
