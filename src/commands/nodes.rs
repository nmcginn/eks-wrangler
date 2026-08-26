//! `eks nodes` — the nodes of one cluster, as a table.

use std::path::PathBuf;
use std::sync::mpsc;

use anyhow::{Result, anyhow};
use k8s_openapi::jiff::Timestamp;

use crate::cluster::ClusterView;
use crate::commands::{self, contexts};
use crate::format::Width;
use crate::k8s::metrics::{self as k8s_metrics};
use crate::k8s::order::Direction;
use crate::k8s::page;
use crate::k8s::{self, nodes as k8s_nodes, pods as k8s_pods};
use crate::kubeconfig::KubeConfig;
use crate::theme::Palette;

/// What the user asked `eks nodes` for, as it came off the command line.
///
/// A struct rather than five more parameters, for the reason `eks pods`'s
/// flags travel as one (decision 29): the flags describe a single request, and
/// a row of same-typed positional arguments is how an ordering quietly ends up
/// in the direction's slot. Every field is applied to the finished rows, so
/// none of them changes what is fetched — only how it is read back.
#[derive(Debug, Clone, Copy, Default)]
pub struct Request {
    /// `--sort`.
    pub order: k8s_nodes::Order,
    /// `--sort-reverse`, which flips `order` without moving the rows the
    /// ordering has nothing to rank — those stay in the tail.
    pub direction: Direction,
    /// `--wide`, or the width of the terminal this is being printed into.
    /// Every column it adds arrived with the nodes, so it costs no request.
    pub width: Width,
    /// Whether the graded cells are written in colour. Decided in `main`,
    /// where stdout is, so nothing below here asks what a terminal is.
    pub palette: Palette,
    /// `--timeout`, spent per step rather than per command: each of the three
    /// listings below is read in pages, and a cluster large enough to need
    /// several of them should not be cut off for being large. The step before
    /// all of them is the credential helper, which `k8s::connect` runs on a
    /// blocking task so that this can bound it.
    pub budget: page::Budget,
}

/// What one fetch found, before either renderer decides what to do with it.
///
/// `list` turns this into a table and its footnotes; the dashboard's node
/// pane (via [`spawn_gather`]) turns it into a loaded `App` state. Splitting
/// here — after the rows are built, before either renderer runs — is what
/// stops the CLI table and the pane from quietly answering the same question
/// two different ways.
struct Gathered {
    label: String,
    rows: Vec<k8s_nodes::NodeRow>,
    /// `Err` is already a sentence, via `k8s::explain`/`k8s_metrics::explain`.
    requests: Result<(), String>,
    usage: Result<(), String>,
    /// Kept for the freshness note, which `list` builds and the pane does not
    /// yet read.
    samples: Vec<Option<k8s_metrics::Sample>>,
    now: Timestamp,
}

/// Resolve the cluster, fetch nodes/pods/metrics concurrently, and reduce
/// them to rows.
///
/// The one function [`list`] and [`spawn_gather`] both call, so the CLI table
/// and the dashboard pane cannot drift about what a node's row means.
async fn gather(
    config: &KubeConfig,
    paths: &[PathBuf],
    selector: Option<&str>,
    budget: page::Budget,
) -> Result<Gathered> {
    let target = target_cluster(config, selector)?;
    let label = target.label();

    let client = k8s::connect(paths, &target, budget).await?;

    // Concurrently, not in sequence: the three requests are independent, and the
    // command should cost one round trip's worth of waiting rather than three.
    let (nodes, pods, usage) = tokio::join!(
        k8s_nodes::fetch(client.clone(), budget),
        k8s_pods::fetch(client.clone(), budget),
        k8s_metrics::usage_by_node(&client, budget),
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

    // Both failures are held as explanations here and written under the table
    // once the rows exist, rather than footnoted on the spot. The request
    // footnote has to name the columns it emptied, and on a cluster with GPUs
    // that list is not known until the nodes have been read.
    let requests = match pods {
        Ok(pods) => Ok(k8s_pods::by_node(&pods)),
        Err(error) => {
            tracing::debug!(%error, "listing pods failed");
            Err(k8s::explain(&error, &label))
        }
    };

    let usage = match usage {
        Ok(usage) => Ok(usage),
        Err(error) => {
            tracing::debug!(%error, "reading node metrics failed");
            Err(k8s_metrics::explain(&error, &label))
        }
    };

    // One instant for every row, so a slow listing cannot show two nodes
    // created together with different ages — and so the freshness note below is
    // measured from the same instant the `AGE` column is.
    let now = Timestamp::now();

    // The join is done before the rows are built rather than inside them,
    // because the samples are wanted twice: once for the figures, and once to
    // date the table. Collecting them here means the note describes exactly the
    // samples that reached the listing, not every sample the endpoint returned.
    let samples: Vec<Option<k8s_metrics::Sample>> = nodes
        .iter()
        .map(|node| {
            // Unlike the requests below, an absent node here is *not* a zero: it
            // is a node metrics-server has not sampled yet, and drawing it as
            // idle would be an invention. `None` reads as `-`.
            usage.as_ref().ok().and_then(|samples| {
                node.metadata
                    .name
                    .as_deref()
                    .and_then(|name| samples.get(name))
                    .copied()
            })
        })
        .collect();

    // A node running nothing: no pods, and nothing booked. Named here rather
    // than built per row so the rows can borrow the totals instead of cloning a
    // map each.
    let nothing = k8s_pods::Placed::default();

    let rows: Vec<k8s_nodes::NodeRow> = nodes
        .iter()
        .zip(&samples)
        .map(|(node, sample)| {
            // A node absent from the totals is running nothing, which is a real
            // zero. Only a failed pod listing leaves the figures unknown.
            let placed = requests.as_ref().ok().map(|totals| {
                node.metadata
                    .name
                    .as_deref()
                    .and_then(|name| totals.get(name))
                    .unwrap_or(&nothing)
            });
            k8s_nodes::NodeRow::from_node(node, placed, sample.map(|s| s.usage), now)
        })
        .collect();

    Ok(Gathered {
        label,
        rows,
        // The map of totals has done its job once it is folded into the rows
        // above; only whether it failed, and why, is wanted from here on.
        requests: requests.map(|_| ()),
        usage: usage.map(|_| ()),
        samples,
        now,
    })
}

/// Fetch and render the node table for the selected cluster.
///
/// `selector` is whatever the user passed to `--context`: a full context name,
/// or the short cluster name `eks contexts` shows. `None` means the cluster
/// their kubeconfig already points at. `request` is the rest of the flags.
pub async fn list(
    config: &KubeConfig,
    paths: &[PathBuf],
    selector: Option<&str>,
    request: Request,
) -> Result<String> {
    let Request {
        order,
        direction,
        width,
        palette,
        budget,
    } = request;

    let Gathered {
        label,
        mut rows,
        requests,
        usage,
        samples,
        now,
    } = gather(config, paths, selector, budget).await?;

    // Ordering lives in `k8s::nodes::order` rather than here, so the default and
    // the one `--sort` asks for are decided in the same place and by the same
    // rules — and so both can be tested on rows alone. The default is still by
    // name, which the API server happens to return today; sorting makes that a
    // promise rather than an accident.
    k8s_nodes::sort(&mut rows, order, direction);

    let mut footnotes = Vec::new();

    // The two columns-are-missing footnotes, held back until now because the
    // first of them names the columns the failure emptied and a device column
    // is one of them. They stay in the order they always came in.
    if let Err(explanation) = &requests {
        footnotes.push(k8s_nodes::requests_unavailable(&rows, explanation));
    }
    if let Err(explanation) = &usage {
        footnotes.push(k8s_nodes::usage_unavailable(explanation));
    }

    // What became of the usage columns, asked of the rows rather than of the
    // request: a read that succeeded and returned nothing costs exactly the
    // columns a failed one does, and the two want opposite advice. Where the
    // columns did survive, they want a date instead — metrics-server going quiet
    // does not fail this request, so without one a stale figure and a fresh one
    // are the same table.
    let usage_columns =
        k8s_metrics::Outcome::of(usage.as_ref().ok(), k8s_nodes::shows_usage(&rows));
    match usage_columns {
        k8s_metrics::Outcome::Shown => footnotes.extend(
            k8s_metrics::freshness(samples.iter().flatten(), now).map(k8s_metrics::freshness_note),
        ),
        k8s_metrics::Outcome::Unsampled => {
            footnotes.push(k8s_nodes::usage_unsampled(&k8s_metrics::unsampled(&label)));
        }
        // Already footnoted above, where the error was caught and could still be
        // explained.
        k8s_metrics::Outcome::Unreadable => {}
    }

    // Under the notes about the columns that are missing, one about a column
    // that is there and is quietly smaller than the hardware behind it.
    footnotes.extend(k8s_nodes::devices_withheld(&rows));

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
        requests: requests.is_err(),
        // The columns being gone, rather than the read having failed: both
        // reasons for their absence now have a footnote above for the note to
        // point back at.
        usage: usage_columns.is_missing(),
    };
    footnotes.extend(k8s::order::unranked_note(
        order,
        k8s_nodes::cause(order, missing),
        |candidate| k8s_nodes::ranks_any(&rows, candidate),
        |candidate| k8s_nodes::distinguishes(&rows, candidate),
    ));

    Ok(k8s_nodes::render(&rows, &label, &footnotes, width, palette))
}

/// What the node pane's background fetch delivers.
///
/// Rows, and how stale the usage figures on them are — see
/// [`k8s_nodes::usage_note`]. A struct rather than a bare `Vec<NodeRow>`
/// because the pane wants both, and the note is not a fact any single row
/// carries: it is a property of the sample as a whole, the same reason the
/// CLI table foots it once rather than per row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodesFetch {
    pub rows: Vec<k8s_nodes::NodeRow>,
    pub usage_note: Option<String>,
}

/// Fetch this cluster's nodes on a background thread, delivering rows — or a
/// message explaining why there are none — over a channel.
///
/// The dashboard's counterpart to [`list`]: the same `gather`, reduced to a
/// [`NodesFetch`] rather than a rendered table, so the pane and the CLI table
/// cannot drift about what a node's row means. Parameters are owned, unlike
/// `gather`'s borrowed ones, because the future has to outlive this call —
/// [`commands::spawn`] moves it onto another thread.
#[must_use]
pub fn spawn_gather(
    config: KubeConfig,
    paths: Vec<PathBuf>,
    selector: Option<String>,
    budget: page::Budget,
) -> mpsc::Receiver<Result<NodesFetch, String>> {
    commands::spawn(async move {
        gather(&config, &paths, selector.as_deref(), budget)
            .await
            .map(|gathered| {
                let usage_note = k8s_nodes::usage_note(
                    &gathered.rows,
                    &gathered.usage,
                    &gathered.samples,
                    gathered.now,
                    &gathered.label,
                );
                NodesFetch {
                    rows: gathered.rows,
                    usage_note,
                }
            })
            .map_err(|error| format!("{error:#}"))
    })
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
