//! `eks pods` — the pods of one namespace, or of every namespace, as a table.

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use k8s_openapi::jiff::Timestamp;

use crate::cluster::ClusterView;
use crate::commands::nodes::target_cluster;
use crate::k8s::metrics::{self as k8s_metrics};
use crate::k8s::pods::{Order, PodRow, Scope, Selectors};
use crate::k8s::{self, pods as k8s_pods, selector};
use crate::kubeconfig::KubeConfig;

/// What the user asked `eks pods` for, as it came off the command line.
///
/// A struct rather than five more parameters: the flags describe one request,
/// and a row of same-typed positional arguments is how a `--namespace` quietly
/// ends up in the `--field-selector` slot. Everything here is still raw text —
/// validating it is [`list`]'s first job, before it connects to anything.
#[derive(Debug, Clone, Copy, Default)]
pub struct Request<'a> {
    /// `--namespace`. Without one, the context's own namespace is used, which
    /// is what a bare `kubectl get pods` would do.
    pub namespace: Option<&'a str>,
    /// `--all-namespaces`, which contradicts `namespace` rather than
    /// overriding it.
    pub all_namespaces: bool,
    /// `-l`, unparsed.
    pub label_selector: Option<&'a str>,
    /// `--field-selector`, unparsed.
    pub field_selector: Option<&'a str>,
    /// `--sort`. Applied to the finished rows, so it changes nothing about what
    /// is fetched — only the order it is read in.
    pub order: Order,
}

/// Fetch and render the pod table for the selected cluster and scope.
///
/// `context` is whatever the user passed to `--context`, resolved exactly as
/// `eks nodes` resolves it; `request` is the rest of the flags.
pub async fn list(
    config: &KubeConfig,
    paths: &[PathBuf],
    context: Option<&str>,
    request: Request<'_>,
) -> Result<String> {
    let target = target_cluster(config, context)?;
    let label = target.label();
    // Resolved before connecting: a contradictory pair of flags, or a selector
    // the user mistyped, should be rejected instantly, not after a credential
    // helper has run and a request has gone out.
    let scope = scope_for(&target, request.namespace, request.all_namespaces)?;
    let selectors = selectors_for(request.label_selector, request.field_selector)?;

    let client = k8s::connect(paths, &target).await?;

    // Concurrently, not in sequence: the two requests are independent, and the
    // command should cost one round trip's worth of waiting rather than two.
    let (pods, usage) = tokio::join!(
        k8s_pods::fetch_scope(client.clone(), &scope, &selectors),
        k8s_metrics::usage_by_pod(&client, &scope, &selectors),
    );

    let pods = pods.map_err(|error| {
        // The raw error is worth having when debugging, but it is not what
        // the user needs to read; `-vv` brings it back.
        tracing::debug!(%error, "listing pods failed");
        let explanation = k8s::explain(&error, &label);
        anyhow!(match k8s::Failure::of(&error) {
            k8s::Failure::Forbidden => denied(&explanation, &scope),
            _ => explanation,
        })
    })?;

    // Only the pod listing is fatal. Missing usage costs the user two columns
    // and earns a footnote, because metrics-server is an add-on EKS does not
    // install for you and a partial answer beats no answer.
    let mut notes = Vec::new();
    let usage = match usage {
        Ok(usage) => Some(usage),
        Err(error) => {
            tracing::debug!(%error, "reading pod metrics failed");
            notes.push(k8s_pods::usage_unavailable(&k8s_metrics::explain(
                &error, &label,
            )));
            None
        }
    };

    // One instant for every row, so a slow listing cannot show two pods created
    // together with different ages.
    let now = Timestamp::now();
    let mut rows: Vec<PodRow> = pods
        .iter()
        .map(|pod| {
            // The join is by namespace and name, which is what makes the
            // usage follow the selectors: only pods the API server already
            // returned get a row, so only they can be given a figure.
            let used = usage.as_ref().and_then(|samples| {
                let namespace = pod.metadata.namespace.as_deref()?;
                let name = pod.metadata.name.as_deref()?;
                samples
                    .get(&(namespace.to_owned(), name.to_owned()))
                    .copied()
            });
            PodRow::from_pod(pod, used, now)
        })
        .collect();
    // Ordering lives in `k8s::pods::order` rather than here, so the default and
    // the one `--sort` asks for are decided in the same place and by the same
    // rules — and so both can be tested on rows alone.
    k8s_pods::sort(&mut rows, request.order);

    Ok(k8s_pods::render(&rows, &label, &scope, &selectors, &notes))
}

/// Validate the label and field selectors, before any network call.
///
/// Kept separate from the fetching, like [`scope_for`], so the "you mistyped
/// the selector" answers are settled without a cluster. A blank selector is
/// treated as absent — `-l ''` is not an error, it just filters nothing — so
/// the empty string never reaches the API server as an empty selector.
pub fn selectors_for(
    label_selector: Option<&str>,
    field_selector: Option<&str>,
) -> Result<Selectors> {
    Ok(Selectors {
        label: validate(label_selector, selector::label_selector)?,
        field: validate(field_selector, selector::field_selector)?,
    })
}

/// Run one selector through its parser, folding a blank result to `None`.
fn validate(
    input: Option<&str>,
    parse: impl Fn(&str) -> Result<String, selector::Error>,
) -> Result<Option<String>> {
    let Some(input) = input else {
        return Ok(None);
    };
    let canonical = parse(input).map_err(|error| anyhow!("{error}"))?;
    Ok((!canonical.is_empty()).then_some(canonical))
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

    #[test]
    fn no_selectors_leaves_both_filters_unset() {
        assert_eq!(selectors_for(None, None).unwrap(), Selectors::default());
    }

    #[test]
    fn valid_selectors_are_carried_through_in_canonical_form() {
        let selectors =
            selectors_for(Some("app == api"), Some(" status.phase != Running ")).unwrap();

        assert_eq!(selectors.label.as_deref(), Some("app=api"));
        assert_eq!(selectors.field.as_deref(), Some("status.phase!=Running"));
    }

    #[test]
    fn a_blank_selector_is_treated_as_no_filter_rather_than_an_error() {
        // `-l ''` should not become an empty selector on the wire, which some
        // servers read differently from an absent one.
        assert_eq!(
            selectors_for(Some("   "), None).unwrap(),
            Selectors::default()
        );
    }

    #[test]
    fn a_malformed_label_selector_is_rejected_before_anything_connects() {
        // `selectors_for` is the whole point: this failure has to happen here,
        // with no cluster in sight.
        let error = selectors_for(Some("app in"), None).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("\"app in\""), "{message}");
        assert!(message.contains("value list"), "{message}");
    }

    #[test]
    fn a_malformed_field_selector_is_rejected_too() {
        let error = selectors_for(None, Some("status.phase")).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("\"status.phase\""), "{message}");
        assert!(message.contains("operator"), "{message}");
    }
}
