//! `eks pods` — the pods of one namespace, or of every namespace, as a table.

use std::path::PathBuf;
use std::sync::mpsc;

use anyhow::{Result, anyhow};
use futures_util::io::AsyncBufReadExt;
use futures_util::stream::StreamExt;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::jiff::Timestamp;
use kube::api::Api;

use crate::aws::LoginMode;
use crate::cluster::ClusterView;
use crate::commands::{self, StreamHandle, credentials, nodes::target_cluster};
use crate::format::Width;
use crate::k8s::metrics::{self as k8s_metrics};
use crate::k8s::order::Direction;
use crate::k8s::page;
use crate::k8s::pods::events::{self, EventRow};
use crate::k8s::pods::logs::{self, LogEvent};
use crate::k8s::pods::{ContainerRow, Order, PodRow, Scope, Selectors};
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
    /// `--login`, carried beside the budget for the reason `eks nodes` carries
    /// it there: both describe the step before the first request. Only this
    /// listing reads it — the dashboard's three pod fetches below run on
    /// background threads that must never stop to ask a question, so they
    /// connect through `k8s::connect` directly and the dashboard puts its own
    /// question before the terminal opens (`credentials::preflight`).
    pub login: LoginMode,
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

    let client = credentials::connect(paths, &target, request.budget, request.login).await?;

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
        |candidate| k8s_pods::distinguishes(&rows, candidate),
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
) -> mpsc::Receiver<Result<PodsFetch, commands::FetchError>> {
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
        .map_err(|error| commands::FetchError::of(&error))
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
        .map_err(|error| k8s::client::Error::explained(&error, &label))?;

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

/// What the pod-containers pane's background fetch delivers.
///
/// `ip`, `nominated_node`, and `readiness_gates` are the pod-level facts
/// `eks pods --wide` reserves a column for — see decision 72. A pane already
/// committed to one pod has room to say them outright, so they ride along
/// beside its containers rather than waiting on a wide mode this pane will
/// never grow.
///
/// `events`, `events_error`, and `events_empty_note` are a second, independent
/// fetch riding along in the same struct: the events listing can fail — most
/// commonly an RBAC role that grants `pods/get` but not `events/list` —
/// without the pod's own containers being any less real, the same
/// "independent fetches, partial degradation" rule `eks nodes` follows for
/// its own node/pod/metrics trio. `events_empty_note` is computed regardless
/// of whether the pane ends up needing it, so rendering never has to reach
/// for a clock or the pod's creation timestamp of its own — see
/// [`events::empty_note`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainersFetch {
    pub rows: Vec<ContainerRow>,
    pub ip: String,
    pub nominated_node: String,
    pub readiness_gates: Option<String>,
    pub events: Vec<EventRow>,
    pub events_error: Option<String>,
    pub events_empty_note: String,
}

/// Fetch one pod's containers, on a background thread.
///
/// Unlike [`spawn_gather_for_node`], this asks for a single named object
/// rather than a listing: the node-pods pane already has every field a
/// container row needs sitting in the `Pod`s it fetched, but nothing keeps
/// that `Pod` around once it has been reduced to a [`PodRow`] — a row that
/// many other pods on the node share the shape of has no room for one pod's
/// full container list. Asking again for the one pod the reader drilled into
/// is simpler than carrying every pod's raw containers through a pane that
/// almost never needs them.
#[must_use]
pub fn spawn_gather_containers(
    config: KubeConfig,
    paths: Vec<PathBuf>,
    cluster: Option<String>,
    namespace: String,
    pod: String,
    budget: page::Budget,
) -> mpsc::Receiver<Result<ContainersFetch, commands::FetchError>> {
    commands::spawn(async move {
        gather_containers(
            &config,
            &paths,
            cluster.as_deref(),
            &namespace,
            &pod,
            budget,
        )
        .await
        .map_err(|error| commands::FetchError::of(&error))
    })
}

/// The `spawn_gather_containers` future, kept separate for the same reason
/// [`gather_for_node`] is: its early returns use `?` against one `Result`.
async fn gather_containers(
    config: &KubeConfig,
    paths: &[PathBuf],
    cluster: Option<&str>,
    namespace: &str,
    pod: &str,
    budget: page::Budget,
) -> Result<ContainersFetch> {
    let target = target_cluster(config, cluster)?;
    let label = target.label();
    let client = k8s::connect(paths, &target, budget).await?;

    let api: Api<Pod> = Api::namespaced(client.clone(), namespace);
    // Concurrently, not in sequence: the pod and its events are independent
    // questions, and only one of the two — the pod itself — is fatal to the
    // pane. `budget.wrap` covers the single `get`; `events::fetch` covers its
    // own paging the same way every other listing in this tool does.
    let (fetched, events) = tokio::join!(
        budget.wrap(api.get(pod)),
        events::fetch(client, namespace, pod, budget),
    );
    let fetched = fetched.map_err(|error| k8s::client::Error::explained(&error, &label))?;

    let now = Timestamp::now();
    let created_at = fetched
        .metadata
        .creation_timestamp
        .as_ref()
        .map(|created| created.0);
    let (events, events_error) = match events {
        Ok(events) => (events::from_events(&events, now), None),
        Err(error) => {
            // Not fatal: the containers this pane exists to show are already
            // in hand, and a role that grants `pods/get` but not
            // `events/list` is an ordinary shape of RBAC to run into.
            tracing::debug!(%error, "listing pod events failed");
            (Vec::new(), Some(k8s::explain(&error, &label)))
        }
    };

    Ok(ContainersFetch {
        rows: ContainerRow::from_pod(&fetched),
        ip: k8s::pods::pod_ip(&fetched),
        nominated_node: k8s::pods::nominated_node(&fetched),
        readiness_gates: k8s::pods::readiness_gates(&fetched),
        events,
        events_error,
        events_empty_note: events::empty_note(created_at, now),
    })
}

/// The pod and container one log stream is for, and the cluster to reach it
/// through — bundled the way [`Request`] bundles `eks pods`'s flags, past
/// `clippy::too_many_arguments`' limit for the same reason: past seven, a
/// mistake that swaps two same-typed strings type-checks (decision 29).
struct LogTarget<'a> {
    cluster: Option<&'a str>,
    namespace: &'a str,
    pod: &'a str,
    container: &'a str,
    /// Whether to open the container's previous instance's log —
    /// `kubectl logs -p` — rather than the one currently running.
    previous: bool,
}

/// [`spawn_stream_logs`]'s counterpart to `LogTarget`, for the same
/// too-many-arguments reason: owning its strings, rather than borrowing them
/// the way `LogTarget` does, because it has to outlive the async block
/// `spawn_stream_logs` moves it into.
#[derive(Debug)]
pub struct LogRequest {
    pub namespace: String,
    pub pod: String,
    pub container: String,
    /// Whether to open the container's previous instance's log —
    /// `kubectl logs -p` — rather than the one currently running.
    pub previous: bool,
}

/// Stream one container's log on a background thread — its current instance,
/// live, or its previous one, per `target.previous`.
///
/// Unlike every other fetch in this module, a current-instance stream never
/// finishes on its own — `LogParams::follow` keeps the connection open for as
/// long as the container keeps printing, so the [`StreamHandle`] this returns
/// alongside the receiver is not bookkeeping, it is the only way the pane
/// ever stops reading. Dropping it — which the event loop does the moment the
/// dashboard's log pane backs out, or switches to the other instance — is
/// what ends the connection; see [`commands::spawn_stream`]'s doc comment for
/// how a signal reaches inside the read loop below. A previous-instance
/// stream has already stopped growing by the time it is opened (see
/// [`logs::params`]), so it ends on its own once read, the same as it would
/// if `stop` fired first.
#[must_use]
pub fn spawn_stream_logs(
    config: KubeConfig,
    paths: Vec<PathBuf>,
    cluster: Option<String>,
    request: LogRequest,
    budget: page::Budget,
) -> (mpsc::Receiver<LogEvent>, StreamHandle) {
    commands::spawn_stream(move |tx, stop| async move {
        let target = LogTarget {
            cluster: cluster.as_deref(),
            namespace: &request.namespace,
            pod: &request.pod,
            container: &request.container,
            previous: request.previous,
        };
        stream_logs(&config, &paths, target, budget, &tx, stop).await;
    })
}

/// The `spawn_stream_logs` task body, kept separate for the same reason
/// [`gather_containers`] is: every early return here is "stop streaming"
/// rather than "propagate a `?`", so it reads better as its own function than
/// folded into the closure above.
///
/// `budget` bounds connecting and opening the log — the same per-step
/// timeout every other fetch spends — but not the read loop that follows: a
/// `follow`ed log is supposed to sit open, and the only thing meant to end it
/// is `stop` firing.
async fn stream_logs(
    config: &KubeConfig,
    paths: &[PathBuf],
    target: LogTarget<'_>,
    budget: page::Budget,
    tx: &mpsc::Sender<LogEvent>,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) {
    let cluster = match target_cluster(config, target.cluster) {
        Ok(cluster) => cluster,
        Err(error) => {
            let _ = tx.send(LogEvent::Ended(Some(error.to_string())));
            return;
        }
    };
    let label = cluster.label();

    // `connect`'s own `Error` already carries a full, user-facing sentence in
    // every variant — including a cluster failure, which it has already run
    // through `explain` internally — so this is a message, not a second
    // classification of one.
    let client = match k8s::connect(paths, &cluster, budget).await {
        Ok(client) => client,
        Err(error) => {
            let _ = tx.send(LogEvent::Ended(Some(error.to_string())));
            return;
        }
    };

    let api: Api<Pod> = Api::namespaced(client, target.namespace);
    let lp = logs::params(target.container, target.previous);
    let stream = match budget.wrap(api.log_stream(target.pod, &lp)).await {
        Ok(stream) => stream,
        Err(error) => {
            let _ = tx.send(LogEvent::Ended(Some(k8s::explain(&error, &label))));
            return;
        }
    };

    let mut lines = stream.lines();
    loop {
        tokio::select! {
            // Dropping the `StreamHandle` fires this and ends the connection
            // — the `lines.next()` arm below is simply never polled again,
            // which drops the stream and, with it, the HTTP request.
            _ = &mut stop => return,
            next = lines.next() => match next {
                Some(Ok(line)) => {
                    // Nobody is listening any more — the pane moved on before
                    // its `StreamHandle` was dropped, which the event loop
                    // never actually does out of order, but a send that
                    // fails here has nowhere else to go either way.
                    if tx.send(LogEvent::Line(line)).is_err() {
                        return;
                    }
                }
                Some(Err(error)) => {
                    let _ = tx.send(LogEvent::Ended(Some(format!(
                        "the log stream for {} broke: {error}",
                        target.container
                    ))));
                    return;
                }
                None => {
                    let _ = tx.send(LogEvent::Ended(None));
                    return;
                }
            },
        }
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
