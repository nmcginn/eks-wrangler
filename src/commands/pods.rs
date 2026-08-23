//! `eks pods` — the pods of one namespace, or of every namespace, as a table.

use std::path::PathBuf;
use std::sync::mpsc;

use anyhow::{Result, anyhow};
use k8s_openapi::jiff::Timestamp;

use crate::cluster::ClusterView;
use crate::commands::{self, nodes::target_cluster};
use crate::format::Width;
use crate::k8s::metrics::{self as k8s_metrics};
use crate::k8s::order::Direction;
use crate::k8s::page;
use crate::k8s::pods::{Order, PodRow, Scope, Selectors};
use crate::k8s::{self, pods as k8s_pods, selector};
use crate::kubeconfig::KubeConfig;
use crate::theme::Palette;

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
    /// `--sort-reverse`, which flips `order` without changing which rows the
    /// ordering has nothing to rank — those stay in the tail.
    pub direction: Direction,
    /// `--wide`. Like `order`, applied to the finished rows: every column it
    /// adds arrived with the pods, so it costs no extra request.
    pub width: Width,
    /// Whether the graded cells are written in colour. Decided in `main`,
    /// where stdout is, so nothing below here asks what a terminal is.
    pub palette: Palette,
    /// `--timeout`, spent per step rather than per command — a namespace big
    /// enough to be read in several pages should not be cut off for its size.
    /// The first step is the credential helper, which `k8s::connect` runs on a
    /// blocking task so that this can bound it.
    pub budget: page::Budget,
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

    let client = k8s::connect(paths, &target, request.budget).await?;

    // Concurrently, not in sequence: the two requests are independent, and the
    // command should cost one round trip's worth of waiting rather than two.
    let (pods, usage) = tokio::join!(
        k8s_pods::fetch_scope(client.clone(), &scope, &selectors, request.budget),
        k8s_metrics::usage_by_pod(&client, &scope, &selectors, request.budget),
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
    // together with different ages — and so the freshness note below is measured
    // from the same instant the `AGE` column is.
    let now = Timestamp::now();

    // The join is done before the rows are built rather than inside them,
    // because the samples are wanted twice: once for the figures, and once to
    // date the table. It matters more here than on the node table, where the two
    // listings cover the same set: the metrics endpoint is asked for a whole
    // namespace, so a `--field-selector` can leave samples behind that no row
    // shows, and dating the table from those would put an age under a listing
    // they are not in.
    let samples: Vec<Option<k8s_metrics::Sample>> = pods
        .iter()
        .map(|pod| {
            // The join is by namespace and name, which is what makes the
            // usage follow the selectors: only pods the API server already
            // returned get a row, so only they can be given a figure.
            usage.as_ref().and_then(|samples| {
                let namespace = pod.metadata.namespace.as_deref()?;
                let name = pod.metadata.name.as_deref()?;
                samples
                    .get(&(namespace.to_owned(), name.to_owned()))
                    .copied()
            })
        })
        .collect();

    let mut rows: Vec<PodRow> = pods
        .iter()
        .zip(&samples)
        .map(|(pod, sample)| PodRow::from_pod(pod, sample.map(|s| s.usage), now))
        .collect();
    // Ordering lives in `k8s::pods::order` rather than here, so the default and
    // the one `--sort` asks for are decided in the same place and by the same
    // rules — and so both can be tested on rows alone.
    k8s_pods::sort(&mut rows, request.order, request.direction);

    // What became of the usage columns, decided and worded exactly as
    // `commands::nodes` decides and words it: the two tables read the same
    // metrics-server, and a person moving between them should not have to work
    // out whether two different sentences mean the same thing.
    let usage_columns = k8s_metrics::Outcome::of(usage.as_ref(), k8s_pods::shows_usage(&rows));
    match usage_columns {
        k8s_metrics::Outcome::Shown => notes.extend(
            k8s_metrics::freshness(samples.iter().flatten(), now).map(k8s_metrics::freshness_note),
        ),
        k8s_metrics::Outcome::Unsampled => {
            notes.push(k8s_pods::usage_unsampled(&k8s_metrics::unsampled(&label)));
        }
        // Already noted above, where the error was caught.
        k8s_metrics::Outcome::Unreadable => {}
    }

    // Last of the notes, under whatever went wrong, and worded and positioned
    // exactly as `eks nodes` does it — the two tables answer "which order is
    // this?" the same way because it is the same question. Silent unless
    // `--sort` or `--sort-reverse` was given.
    notes.extend(k8s::order::note(request.order, request.direction));
    // And under it, the case where that line on its own misleads: an ordering
    // that ranked no row at all — `--sort cpu` with no metrics-server, `--sort
    // restarts` where nothing has ever crashed — describes a listing the
    // alphabet arranged. Again worded once, in `k8s::order`, for both tables,
    // with the listing supplying the two things the wording turns on: what
    // these rows could be sorted by instead, and whether the note above already
    // explains the empty column.
    let missing = k8s_pods::Missing {
        // The columns being gone, rather than the read having failed: both
        // reasons for their absence now have a note above to point back at.
        usage: usage_columns.is_missing(),
    };
    notes.extend(k8s::order::unranked_note(
        request.order,
        k8s_pods::cause(request.order, missing),
        |candidate| k8s_pods::ranks_any(&rows, candidate),
    ));

    Ok(k8s_pods::render(
        &rows,
        &label,
        &scope,
        &selectors,
        &notes,
        request.width,
        request.palette,
    ))
}

/// What the pod-drilldown pane's background fetch delivers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PodsFetch {
    pub rows: Vec<PodRow>,
    /// What to say instead of "this node has no pods" when an empty listing
    /// is a selector's doing rather than the node's — the pane's counterpart
    /// to [`k8s::pods::row::selector_note`](crate::k8s::pods::selector_note),
    /// which answers the same question for the CLI table. `None` when no
    /// selector is active, so the pane keeps its plainer wording.
    pub selector_note: Option<String>,
}

/// Fetch the pods placed on one node, on a background thread.
///
/// The dashboard's pod-browsing pane calls this once each time it is asked to
/// show a different node — unlike the node pane, it does not refresh itself
/// on an interval yet, which the dashboard follow-ups leave as its own task.
/// No usage figures are fetched either: a node's pods are already a full
/// round trip of their own, and wiring `metrics.k8s.io` into a third pane
/// reads better as a considered addition than as a rider on this one.
///
/// `selectors` is whatever the user typed with `-l`/`--field-selector`,
/// already validated by [`selectors_for`] — the same function and the same
/// rejection path `eks pods` uses, so a selector means one thing across the
/// tool. It is combined with the pane's own `spec.nodeName` filter rather
/// than replacing it.
#[must_use]
pub fn spawn_gather_for_node(
    config: KubeConfig,
    paths: Vec<PathBuf>,
    cluster: Option<String>,
    node: String,
    selectors: Selectors,
    budget: page::Budget,
) -> mpsc::Receiver<Result<PodsFetch, String>> {
    commands::spawn(async move {
        gather_for_node(
            &config,
            &paths,
            cluster.as_deref(),
            &node,
            &selectors,
            budget,
        )
        .await
        .map_err(|error| format!("{error:#}"))
    })
}

/// The `spawn_gather_for_node` future, kept separate so its early returns can
/// use `?` against one `Result` instead of matching by hand.
async fn gather_for_node(
    config: &KubeConfig,
    paths: &[PathBuf],
    cluster: Option<&str>,
    node: &str,
    selectors: &Selectors,
    budget: page::Budget,
) -> Result<PodsFetch> {
    let target = target_cluster(config, cluster)?;
    let label = target.label();
    let client = k8s::connect(paths, &target, budget).await?;

    let scoped = scoped_to_node(node, selectors);
    let pods = k8s_pods::fetch_scope(client, &Scope::All, &scoped, budget)
        .await
        .map_err(|error| anyhow!(k8s::explain(&error, &label)))?;

    let now = Timestamp::now();
    let rows = pods
        .iter()
        .map(|pod| PodRow::from_pod(pod, None, now))
        .collect();

    Ok(PodsFetch {
        rows,
        // From the user's own selectors, not `scoped` — the node filter is
        // implicit in "this is the node's pane", never something to explain
        // back to the user as a reason the list came back empty.
        selector_note: k8s_pods::selector_note(selectors),
    })
}

/// Combine the pane's own `spec.nodeName` scope with whatever selectors the
/// user typed, kept as a pure function so the combining rule — the node
/// filter and the user's are `AND`ed, never one replacing the other — is a
/// fixture rather than something only a live fetch exercises.
///
/// Every namespace: a node's pods are not scoped to one, and the pane answers
/// "what is running here", not "what is running in this namespace".
/// `spec.nodeName` is safe to interpolate unquoted — it comes from a
/// `NodeRow` the API server itself named, never from what a user typed, and a
/// Kubernetes node name cannot contain the characters the field-selector
/// grammar treats specially. A comma joins two field requirements the same
/// way it joins two label ones, so `--field-selector status.phase!=Running`
/// narrows this node's pods rather than being silently dropped by the pane's
/// own filter.
fn scoped_to_node(node: &str, selectors: &Selectors) -> Selectors {
    let mut field = format!("spec.nodeName={node}");
    if let Some(user_field) = &selectors.field {
        field.push(',');
        field.push_str(user_field);
    }
    Selectors {
        label: selectors.label.clone(),
        field: Some(field),
    }
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

    #[test]
    fn scoping_to_a_node_adds_nothing_else_when_no_selector_is_active() {
        let scoped = scoped_to_node("ip-10-0-1-9.ec2.internal", &Selectors::default());

        assert_eq!(scoped.label, None);
        assert_eq!(
            scoped.field.as_deref(),
            Some("spec.nodeName=ip-10-0-1-9.ec2.internal")
        );
    }

    #[test]
    fn scoping_to_a_node_carries_the_users_label_selector_through_unchanged() {
        let selectors = Selectors {
            label: Some("app=api".to_owned()),
            field: None,
        };

        let scoped = scoped_to_node("worker-1", &selectors);

        assert_eq!(scoped.label.as_deref(), Some("app=api"));
        assert_eq!(scoped.field.as_deref(), Some("spec.nodeName=worker-1"));
    }

    #[test]
    fn scoping_to_a_node_ands_the_users_field_selector_onto_the_node_filter() {
        // A comma joins two field requirements, so both must hold: this is
        // the node's pods *and* not-Running, not either on its own.
        let selectors = Selectors {
            label: None,
            field: Some("status.phase!=Running".to_owned()),
        };

        let scoped = scoped_to_node("worker-1", &selectors);

        assert_eq!(
            scoped.field.as_deref(),
            Some("spec.nodeName=worker-1,status.phase!=Running")
        );
    }
}
