//! Listing, inspecting, and switching clusters.

use anyhow::{Context as _, Result, anyhow, bail};

use crate::cluster::ClusterView;
use crate::kubeconfig::{self, KubeConfig};

/// Build the display views for every context in the config.
#[must_use]
pub fn views(config: &KubeConfig) -> Vec<ClusterView> {
    config
        .resolved_contexts()
        .iter()
        .map(ClusterView::from_context)
        .collect()
}

/// Render `eks contexts` output.
///
/// `quiet` prints bare context names for piping into other tools; the default
/// is an aligned table with the active cluster marked.
#[must_use]
pub fn list(config: &KubeConfig, quiet: bool) -> String {
    let views = views(config);

    if quiet {
        return views
            .iter()
            .map(|v| v.context_name.as_str())
            .collect::<Vec<_>>()
            .join("\n");
    }

    render_table(&views)
}

/// Render `eks current`.
pub fn current(config: &KubeConfig) -> Result<String> {
    let current = config
        .current()
        .ok_or_else(|| match &config.current_context {
            Some(name) => anyhow!(
                "current-context is set to {name:?}, but no such context exists in your kubeconfig"
            ),
            None => anyhow!("no current context is set; run `eks use <name>` to pick one"),
        })?;

    let view = ClusterView::from_context(&current);
    Ok(format!(
        "{}\n  context   {}\n  namespace {}",
        view.label(),
        view.context_name,
        view.namespace
    ))
}

/// Switch the active cluster, writing `current-context` back to disk.
///
/// The selector may be either a full context name or the short cluster name
/// shown by `eks contexts`, so users never have to type an ARN.
pub fn switch(config: &KubeConfig, selector: &str) -> Result<String> {
    let views = views(config);
    let target = resolve_selector(&views, selector)?;

    let path = config
        .primary_source()
        .ok_or_else(|| anyhow!("kubeconfig was not loaded from a file, so it cannot be updated"))?;

    if config.current_context.as_deref() == Some(target.context_name.as_str()) {
        return Ok(format!("Already using {}", target.label()));
    }

    kubeconfig::set_current_context(path, &target.context_name)
        .with_context(|| format!("failed to update {}", path.display()))?;

    Ok(format!("Now using {}", target.label()))
}

/// Match a user-supplied selector against the available clusters.
///
/// Resolution order: exact context name, then exact short name. A short name
/// matching several clusters is an error rather than a guess.
pub fn resolve_selector<'a>(views: &'a [ClusterView], selector: &str) -> Result<&'a ClusterView> {
    if let Some(exact) = views.iter().find(|v| v.context_name == selector) {
        return Ok(exact);
    }

    let by_display: Vec<&ClusterView> = views
        .iter()
        .filter(|v| v.display_name == selector)
        .collect();

    match by_display.as_slice() {
        [single] => return Ok(single),
        [] => {}
        multiple => {
            let names = multiple
                .iter()
                .map(|v| format!("  {}", v.context_name))
                .collect::<Vec<_>>()
                .join("\n");
            bail!(
                "{selector:?} matches {} clusters:\n{names}\nUse the full context name.",
                multiple.len()
            );
        }
    }

    let needle = selector.to_lowercase();
    let suggestions: Vec<&str> = views
        .iter()
        .filter(|v| {
            v.display_name.to_lowercase().contains(&needle)
                || v.context_name.to_lowercase().contains(&needle)
        })
        .map(|v| v.display_name.as_str())
        .collect();

    if suggestions.is_empty() {
        bail!("no cluster named {selector:?}; run `eks contexts` to see what is available");
    }
    bail!(
        "no cluster named {selector:?}. Did you mean: {}?",
        suggestions.join(", ")
    );
}

/// Render the aligned cluster table.
fn render_table(views: &[ClusterView]) -> String {
    // The active-cluster marker is a fixed two-character gutter rather than a
    // padded column, so rows read as `* prod` instead of `*  prod`.
    const CURRENT: &str = "* ";
    const OTHER: &str = "  ";

    if views.is_empty() {
        return "No clusters found in your kubeconfig.\n\
                Run `aws eks update-kubeconfig --name <cluster>` to add one."
            .to_owned();
    }

    let headers = ["NAME", "REGION", "NAMESPACE"];
    let rows: Vec<[String; 3]> = views
        .iter()
        .map(|v| {
            [
                v.display_name.clone(),
                v.region.clone().unwrap_or_else(|| "-".to_owned()),
                v.namespace.clone(),
            ]
        })
        .collect();

    let widths: Vec<usize> = (0..headers.len())
        .map(|column| {
            rows.iter()
                .map(|row| display_width(&row[column]))
                .chain(std::iter::once(display_width(headers[column])))
                .max()
                .unwrap_or(0)
        })
        .collect();

    let mut out = String::new();
    push_row(&mut out, OTHER, &headers.map(str::to_owned), &widths);
    for (view, row) in views.iter().zip(&rows) {
        let gutter = if view.is_current { CURRENT } else { OTHER };
        push_row(&mut out, gutter, row, &widths);
    }
    out.trim_end().to_owned()
}

fn push_row(out: &mut String, gutter: &str, cells: &[String; 3], widths: &[usize]) {
    out.push_str(gutter);

    let last = cells.len() - 1;
    for (index, cell) in cells.iter().enumerate() {
        if index == last {
            // Never pad the final column; trailing whitespace shows up in diffs
            // and when users copy lines out of the terminal.
            out.push_str(cell);
        } else {
            let pad = widths[index].saturating_sub(display_width(cell));
            out.push_str(cell);
            out.extend(std::iter::repeat_n(' ', pad + 2));
        }
    }
    out.push('\n');
}

fn display_width(value: &str) -> usize {
    value.chars().count()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const TWO_CLUSTERS: &str = r"
current-context: arn:aws:eks:us-east-1:111122223333:cluster/prod
clusters:
  - name: arn:aws:eks:us-east-1:111122223333:cluster/prod
    cluster:
      server: https://AAAA.gr7.us-east-1.eks.amazonaws.com
contexts:
  - name: arn:aws:eks:us-east-1:111122223333:cluster/prod
    context:
      cluster: arn:aws:eks:us-east-1:111122223333:cluster/prod
      user: prod
  - name: staging
    context:
      cluster: arn:aws:eks:us-west-2:111122223333:cluster/staging
      user: staging
      namespace: payments
";

    fn config() -> KubeConfig {
        KubeConfig::parse(TWO_CLUSTERS).unwrap()
    }

    fn view(context_name: &str, display_name: &str) -> ClusterView {
        ClusterView {
            context_name: context_name.to_owned(),
            display_name: display_name.to_owned(),
            region: Some("us-east-1".to_owned()),
            account_id: None,
            namespace: "default".to_owned(),
            is_current: false,
        }
    }

    #[test]
    fn table_marks_the_current_cluster_and_hides_arns() {
        let output = list(&config(), false);

        assert!(output.contains("* prod"), "output was:\n{output}");
        assert!(output.contains("staging"));
        assert!(output.contains("us-west-2"));
        assert!(
            !output.contains("arn:aws:eks"),
            "the table should never show raw ARNs:\n{output}"
        );
    }

    #[test]
    fn table_columns_line_up() {
        let output = list(&config(), false);
        let starts: Vec<_> = output
            .lines()
            .map(|line| line.find("us-").unwrap_or_default())
            .filter(|offset| *offset > 0)
            .collect();

        assert!(
            starts.windows(2).all(|w| w[0] == w[1]),
            "ragged: {starts:?}"
        );
    }

    #[test]
    fn table_rows_have_no_trailing_whitespace() {
        for line in list(&config(), false).lines() {
            assert_eq!(line, line.trim_end(), "trailing whitespace in {line:?}");
        }
    }

    #[test]
    fn quiet_mode_prints_context_names_for_scripting() {
        let output = list(&config(), true);

        assert_eq!(
            output.lines().collect::<Vec<_>>(),
            vec!["arn:aws:eks:us-east-1:111122223333:cluster/prod", "staging"]
        );
    }

    #[test]
    fn empty_kubeconfig_explains_what_to_do_next() {
        let output = list(&KubeConfig::default(), false);
        assert!(output.contains("update-kubeconfig"), "{output}");
    }

    #[test]
    fn current_reports_the_active_cluster() {
        let output = current(&config()).unwrap();

        assert!(output.starts_with("prod (us-east-1)"), "{output}");
        assert!(output.contains("namespace default"));
    }

    #[test]
    fn current_explains_an_unset_context() {
        let err = current(&KubeConfig::default()).unwrap_err().to_string();
        assert!(err.contains("eks use"), "{err}");
    }

    #[test]
    fn selector_accepts_the_short_name_or_the_full_context() {
        let views = views(&config());

        assert_eq!(
            resolve_selector(&views, "prod").unwrap().display_name,
            "prod"
        );
        assert_eq!(
            resolve_selector(&views, "arn:aws:eks:us-east-1:111122223333:cluster/prod")
                .unwrap()
                .display_name,
            "prod"
        );
    }

    #[test]
    fn selector_prefers_an_exact_context_name_over_a_short_name() {
        // A context literally named "prod" that points elsewhere must win over
        // another cluster whose short name happens to be "prod".
        let views = vec![
            view("arn:aws:eks:us-east-1:1:cluster/prod", "prod"),
            view("prod", "prod"),
        ];

        assert_eq!(
            resolve_selector(&views, "prod").unwrap().context_name,
            "prod"
        );
    }

    #[test]
    fn ambiguous_short_names_are_an_error_not_a_guess() {
        let views = vec![
            view("arn:aws:eks:us-east-1:1:cluster/prod", "prod"),
            view("arn:aws:eks:us-west-2:2:cluster/prod", "prod"),
        ];

        let err = resolve_selector(&views, "prod").unwrap_err().to_string();
        assert!(err.contains("matches 2 clusters"), "{err}");
        assert!(err.contains("us-west-2"), "{err}");
    }

    #[test]
    fn unknown_selector_suggests_near_matches() {
        let views = views(&config());

        let err = resolve_selector(&views, "stag").unwrap_err().to_string();
        assert!(err.contains("Did you mean: staging"), "{err}");

        let err = resolve_selector(&views, "zzz").unwrap_err().to_string();
        assert!(err.contains("eks contexts"), "{err}");
    }

    #[test]
    fn switch_writes_the_new_context_to_the_primary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(&path, TWO_CLUSTERS).unwrap();

        let loaded = KubeConfig::load_from(std::slice::from_ref(&path)).unwrap();
        let message = switch(&loaded, "staging").unwrap();
        assert_eq!(message, "Now using staging (us-west-2)");

        let reloaded = KubeConfig::load_from(&[path]).unwrap();
        assert_eq!(reloaded.current_context.as_deref(), Some("staging"));
    }

    #[test]
    fn switching_to_the_active_cluster_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(&path, TWO_CLUSTERS).unwrap();

        let loaded = KubeConfig::load_from(std::slice::from_ref(&path)).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let message = switch(&loaded, "prod").unwrap();

        assert!(message.starts_with("Already using"), "{message}");
        assert_eq!(
            before,
            std::fs::read_to_string(&path).unwrap(),
            "an already-current switch must not rewrite the file"
        );
    }
}
