//! The pod-drilldown pane: the pods placed on one node.
//!
//! Fetching lives in [`crate::commands::pods::spawn_gather_for_node`]; this
//! module only draws whatever [`PodsState`] `App` was last handed — the same
//! split [`super::nodes`] keeps between computation and rendering.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::k8s::nodes::{self as k8s_nodes, NodeRow};
use crate::k8s::order::{self, Direction};
use crate::k8s::pods::{Missing, Order, PodRow, cause, distinguishes, ranks_any};
use crate::theme::{Severity, Theme};

/// What the pod-drilldown pane is showing, independent of how it is drawn.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PodsState {
    /// The fetch is still in flight.
    #[default]
    Loading,
    /// The cluster answered — possibly with zero pods, a real answer for a
    /// node that is cordoned or has just joined.
    Loaded {
        rows: Vec<PodRow>,
        /// What to say instead of "this node has no pods" when a `-l`/
        /// `--field-selector` the user typed is why the list is empty. See
        /// [`crate::commands::pods::PodsFetch::selector_note`].
        selector_note: Option<String>,
    },
    /// The fetch failed; the message is already a full sentence, via
    /// `k8s::explain`.
    Error(String),
}

impl PodsState {
    /// The rows this state is showing, or an empty slice for every state
    /// that has none — which is what lets `App` bound its row selection
    /// against "however many rows there are" without matching on the state
    /// itself.
    #[must_use]
    pub fn rows(&self) -> &[PodRow] {
        match self {
            Self::Loaded { rows, .. } => rows,
            Self::Loading | Self::Error(_) => &[],
        }
    }
}

/// Draw whatever the pod pane currently knows.
///
/// `node` is the [`NodeRow`] behind the node drilled into, from the node
/// pane's own listing — `None` when it has since left that listing (a node
/// that scaled down while its pods were open) or, in a test, was never given
/// one. Its `--wide` facts are drawn above the pod list regardless of whether
/// the pod fetch itself has finished: they were already known before this
/// pane's own fetch started, and there is no reason to make them wait on a
/// second request. `selected` highlights a row — `None` when the pane does
/// not currently hold keyboard focus, so the highlight disappears the moment
/// `Tab` moves it back to the sidebar. `order` and `direction` are the pane's
/// own ordering, changed by `s`/`S` in [`super::App`] rather than by a
/// request — see [`super::nodes::draw`], whose node-pane counterpart this
/// mirrors. `filter` is the `/` query, empty when no filter is active — see
/// that same doc comment for why every footnote above still reads off the
/// full `rows` and only the drawn rows themselves narrow.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw(
    frame: &mut Frame,
    area: Rect,
    state: &PodsState,
    node: Option<&NodeRow>,
    selected: Option<usize>,
    order: Order,
    direction: Direction,
    filter: &str,
    theme: Theme,
) {
    let mut lines: Vec<Line> = node_facts_lines(node, theme);
    lines.extend(match state {
        PodsState::Loading => vec![Line::styled("Loading pods…", theme.dim())],
        PodsState::Error(message) => vec![Line::styled(
            message.clone(),
            theme.severity(Severity::Critical),
        )],
        PodsState::Loaded {
            rows,
            selector_note,
        } if rows.is_empty() => {
            // A selector that matched nothing must not read like an empty
            // node, or the user goes looking for pods that are there but
            // filtered out — the same reasoning `k8s::pods::row::empty`
            // follows for the CLI table.
            let message = selector_note.as_ref().map_or_else(
                || "This node has no pods.".to_owned(),
                |note| format!("No pods here match {note}."),
            );
            vec![Line::styled(message, theme.dim())]
        }
        PodsState::Loaded { rows, .. } => {
            let mut lines = vec![Line::styled("PODS", theme.heading())];
            if !filter.is_empty() {
                lines.push(Line::styled(format!("Filter: \"{filter}\""), theme.dim()));
            }
            if let Some(note) = order::note(order, direction) {
                lines.push(Line::styled(note, theme.dim()));
            }
            // This pane never samples usage for its own rows yet (see
            // `spawn_gather_for_node`), so `Missing::default()` — `usage:
            // false` — is always the honest reading: nothing above these
            // rows explains why `cpu`/`memory` ranked nothing, because
            // nothing is printed about metrics here at all.
            if let Some(note) = order::unranked_note(
                order,
                cause(order, Missing::default()),
                |candidate| ranks_any(rows, candidate),
                |candidate| distinguishes(rows, candidate),
            ) {
                lines.extend(
                    note.lines()
                        .map(|line| Line::styled(line.to_owned(), theme.dim())),
                );
            }
            let visible = crate::fuzzy::rank(filter, rows, |row| row.name.as_str());
            if !filter.is_empty() && visible.is_empty() {
                lines.push(Line::styled(
                    format!("No pods here match \"{filter}\"."),
                    theme.dim(),
                ));
            } else {
                lines.extend(
                    visible
                        .into_iter()
                        .enumerate()
                        .map(|(index, row)| pod_line(row, Some(index) == selected, theme)),
                );
            }
            lines
        }
    });

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
}

/// The node's `--wide` facts above the pod list — empty when `node` is
/// `None`, so a pane that never had one to draw is unchanged.
///
/// Unconditional once a `NodeRow` is in hand, the way [`k8s_nodes::wide_facts`]
/// itself is: every one of the five lines is drawn whatever is in it, `-`
/// included, rather than the `any`-not-`all` rule the pod-level facts beside
/// [`super::containers::draw`] follow for the two that are usually absent.
fn node_facts_lines(node: Option<&NodeRow>, theme: Theme) -> Vec<Line<'static>> {
    let Some(node) = node else {
        return Vec::new();
    };
    k8s_nodes::wide_facts(node)
        .into_iter()
        .map(|(label, value)| Line::styled(format!("{label}: {value}"), theme.dim()))
        .collect()
}

fn pod_line(row: &PodRow, selected: bool, theme: Theme) -> Line<'static> {
    let restarts = match &row.restart_age {
        Some(age) => format!("{} ({age} ago)", row.restarts),
        None => row.restarts.to_string(),
    };

    let spans = vec![
        Span::styled(row.name.clone(), theme.body()),
        Span::raw("  "),
        Span::styled(row.status.clone(), theme.severity(row.severity)),
        Span::raw("  "),
        Span::styled(row.ready.clone(), theme.dim()),
        Span::raw("  "),
        Span::styled(restarts, theme.dim()),
        Span::raw("  "),
        Span::styled(row.age.clone(), theme.dim()),
    ];

    let line = Line::from(spans);
    if selected {
        line.style(theme.selected())
    } else {
        line
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::k8s::quantity::Quantity;
    use crate::theme::Severity;

    fn pod(name: &str) -> PodRow {
        PodRow {
            namespace: "default".to_owned(),
            name: name.to_owned(),
            ready: "1/1".to_owned(),
            status: "Running".to_owned(),
            severity: Severity::Ok,
            restarts: 0,
            restart_age: None,
            last_restart: None,
            age: "3d".to_owned(),
            created_at: None,
            cpu_used: None,
            memory_used: None,
            cpu_requested: Quantity::default(),
            memory_requested: Quantity::default(),
            node: "worker-1".to_owned(),
            ip: "-".to_owned(),
            nominated_node: "-".to_owned(),
            readiness_gates: None,
        }
    }

    /// A `NodeRow` with every `--wide` fact filled in, for the tests below
    /// that draw the pane's own node facts.
    fn node_row(name: &str) -> NodeRow {
        NodeRow {
            name: name.to_owned(),
            status: "Ready".to_owned(),
            severity: Severity::Ok,
            version: "v1.31".to_owned(),
            cpu: crate::k8s::nodes::Capacity::default(),
            memory: crate::k8s::nodes::Capacity::default(),
            cpu_requested: crate::k8s::nodes::Share::default(),
            memory_requested: crate::k8s::nodes::Share::default(),
            cpu_used: crate::k8s::nodes::Share::default(),
            memory_used: crate::k8s::nodes::Share::default(),
            pods: crate::k8s::nodes::Share::default(),
            age: "3d".to_owned(),
            created_at: None,
            internal_ip: "10.0.1.9".to_owned(),
            external_ip: "-".to_owned(),
            os_image: "Amazon Linux 2023".to_owned(),
            kernel_version: "6.1.148".to_owned(),
            container_runtime: "containerd://1.7.28".to_owned(),
            devices: std::collections::BTreeMap::new(),
            ephemeral_storage: crate::k8s::nodes::Capacity::default(),
            hugepages: std::collections::BTreeMap::new(),
        }
    }

    fn render(state: &PodsState, selected: Option<usize>) -> String {
        render_ordered(state, selected, Order::default(), Direction::default())
    }

    fn render_ordered(
        state: &PodsState,
        selected: Option<usize>,
        order: Order,
        direction: Direction,
    ) -> String {
        render_filtered(state, selected, order, direction, "")
    }

    fn render_filtered(
        state: &PodsState,
        selected: Option<usize>,
        order: Order,
        direction: Direction,
        filter: &str,
    ) -> String {
        render_full(state, None, selected, order, direction, filter)
    }

    fn render_with_node(state: &PodsState, node: Option<&NodeRow>) -> String {
        render_full(
            state,
            node,
            None,
            Order::default(),
            Direction::default(),
            "",
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn render_full(
        state: &PodsState,
        node: Option<&NodeRow>,
        selected: Option<usize>,
        order: Order,
        direction: Direction,
        filter: &str,
    ) -> String {
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw(
                    frame,
                    area,
                    state,
                    node,
                    selected,
                    order,
                    direction,
                    filter,
                    Theme::dark(),
                );
            })
            .unwrap();
        terminal.backend().to_string()
    }

    #[test]
    fn loading_state_renders_before_any_row_exists() {
        let rendered = render(&PodsState::Loading, None);
        assert!(rendered.contains("Loading pods"), "{rendered}");
    }

    #[test]
    fn loaded_state_shows_each_pods_name_and_status() {
        let state = PodsState::Loaded {
            rows: vec![pod("api-1"), pod("api-2")],
            selector_note: None,
        };
        let rendered = render(&state, None);

        assert!(rendered.contains("api-1"), "{rendered}");
        assert!(rendered.contains("api-2"), "{rendered}");
        assert!(rendered.contains("Running"), "{rendered}");
    }

    #[test]
    fn an_empty_pod_list_says_so_rather_than_rendering_nothing() {
        let state = PodsState::Loaded {
            rows: Vec::new(),
            selector_note: None,
        };
        let rendered = render(&state, None);
        assert!(rendered.contains("no pods"), "{rendered}");
    }

    #[test]
    fn the_default_order_says_nothing_about_sorting() {
        let state = PodsState::Loaded {
            rows: vec![pod("api-1")],
            selector_note: None,
        };
        let rendered = render(&state, None);
        assert!(!rendered.contains("Sorted by"), "{rendered}");
    }

    #[test]
    fn a_reordered_pane_names_the_ordering_under_its_rows() {
        let state = PodsState::Loaded {
            rows: vec![pod("api-1")],
            selector_note: None,
        };
        let rendered = render_ordered(&state, None, Order::Restarts, Direction::Reversed);
        assert!(
            rendered.contains("Sorted by restarts, reversed."),
            "{rendered}"
        );
    }

    #[test]
    fn an_empty_pane_says_nothing_about_the_ordering_it_was_asked_for() {
        let state = PodsState::Loaded {
            rows: Vec::new(),
            selector_note: None,
        };
        let rendered = render_ordered(&state, None, Order::Restarts, Direction::Natural);
        assert!(!rendered.contains("Sorted by"), "{rendered}");
    }

    #[test]
    fn a_pane_ordering_that_ranked_nothing_says_so() {
        // `pod("api-1")` has never restarted and carries no usage sample, so
        // every ordering but `Name` ranks nothing on it.
        let state = PodsState::Loaded {
            rows: vec![pod("api-1")],
            selector_note: None,
        };

        let rendered = render_ordered(&state, None, Order::Cpu, Direction::Natural);

        assert!(
            rendered.contains("Nothing here has cpu to sort by."),
            "{rendered}"
        );
    }

    #[test]
    fn a_pane_ordering_that_ranked_and_distinguished_something_says_nothing_extra() {
        // Two rows with different `cpu_used` figures: `--sort cpu` (`s` in the
        // pane) both ranks and rearranges them, so the diagnosis has nothing
        // to add. A single row would not prove this — see the tests below.
        let state = PodsState::Loaded {
            rows: vec![
                PodRow {
                    cpu_used: Some(Quantity::parse("250m").unwrap()),
                    ..pod("api-1")
                },
                PodRow {
                    cpu_used: Some(Quantity::parse("50m").unwrap()),
                    ..pod("api-2")
                },
            ],
            selector_note: None,
        };

        let rendered = render_ordered(&state, None, Order::Cpu, Direction::Natural);

        assert!(!rendered.contains("Nothing here"), "{rendered}");
        assert!(!rendered.contains("ranks the same"), "{rendered}");
    }

    #[test]
    fn a_pane_with_one_row_never_calls_its_own_ordering_useful() {
        // A single sampled pod ranks under `cpu` — there is a real figure —
        // but a pane with one row can never be *rearranged* by anything, so
        // the diagnosis fires anyway: sorting it was a no-op.
        let sampled = PodRow {
            cpu_used: Some(Quantity::parse("250m").unwrap()),
            ..pod("api-1")
        };
        let state = PodsState::Loaded {
            rows: vec![sampled],
            selector_note: None,
        };

        let rendered = render_ordered(&state, None, Order::Cpu, Direction::Natural);

        assert!(
            rendered.contains(
                "Every row here ranks the same under cpu, so sorting by it changed nothing."
            ),
            "{rendered}"
        );
    }

    #[test]
    fn a_pane_ordering_that_ranks_two_tied_rows_says_so() {
        // Two rows with the *same* `cpu_used` figure: `ranks_any` says yes for
        // both, but sorting between them changes nothing, and the pane says
        // so exactly as the CLI table does.
        let state = PodsState::Loaded {
            rows: vec![
                PodRow {
                    cpu_used: Some(Quantity::parse("250m").unwrap()),
                    ..pod("api-1")
                },
                PodRow {
                    cpu_used: Some(Quantity::parse("250m").unwrap()),
                    ..pod("api-2")
                },
            ],
            selector_note: None,
        };

        let rendered = render_ordered(&state, None, Order::Cpu, Direction::Natural);

        assert!(
            rendered.contains(
                "Every row here ranks the same under cpu, so sorting by it changed nothing."
            ),
            "{rendered}"
        );
    }

    #[test]
    fn this_pane_never_points_an_unranked_ordering_at_a_usage_note() {
        // Unlike the node pane, this one has no usage note above its rows at
        // all yet (see `spawn_gather_for_node`), so the diagnosis must never
        // claim one explains the empty column.
        let state = PodsState::Loaded {
            rows: vec![pod("api-1")],
            selector_note: None,
        };

        let rendered = render_ordered(&state, None, Order::Memory, Direction::Natural);

        assert!(!rendered.contains("for the reason above"), "{rendered}");
    }

    #[test]
    fn an_empty_listing_under_a_selector_blames_the_selector_not_the_node() {
        // The same reasoning `k8s::pods::row::empty` uses for the CLI table:
        // a filter that matched nothing must not read like a node with
        // nothing on it, or the user goes looking for pods that are there
        // but filtered out.
        let state = PodsState::Loaded {
            rows: Vec::new(),
            selector_note: Some("label selector `app=api`".to_owned()),
        };
        let rendered = render(&state, None);

        assert!(rendered.contains("label selector `app=api`"), "{rendered}");
        assert!(!rendered.contains("has no pods."), "{rendered}");
    }

    #[test]
    fn error_state_renders_the_message_instead_of_a_table() {
        let rendered = render(
            &PodsState::Error("could not list pods: nope".to_owned()),
            None,
        );
        assert!(rendered.contains("could not list pods"), "{rendered}");
    }

    #[test]
    fn rows_returns_nothing_for_loading_or_error() {
        assert!(PodsState::Loading.rows().is_empty());
        assert!(PodsState::Error("nope".to_owned()).rows().is_empty());
    }

    #[test]
    fn rows_returns_the_loaded_rows() {
        let state = PodsState::Loaded {
            rows: vec![pod("api-1")],
            selector_note: None,
        };
        assert_eq!(state.rows().len(), 1);
    }

    #[test]
    fn rendering_the_pod_pane_survives_a_tiny_terminal() {
        let state = PodsState::Loaded {
            rows: vec![pod("api-1")],
            selector_note: None,
        };
        let node = node_row("worker-1");
        for (width, height) in [(1, 1), (8, 3), (20, 2), (200, 60)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    draw(
                        frame,
                        area,
                        &state,
                        Some(&node),
                        Some(0),
                        Order::default(),
                        Direction::default(),
                        "",
                        Theme::dark(),
                    );
                })
                .unwrap();
        }
    }

    #[test]
    fn an_empty_filter_renders_exactly_as_no_filter_does() {
        let state = PodsState::Loaded {
            rows: vec![pod("api-1"), pod("api-2")],
            selector_note: None,
        };
        assert_eq!(
            render_filtered(&state, None, Order::default(), Direction::default(), ""),
            render(&state, None)
        );
    }

    #[test]
    fn a_filter_narrows_the_rows_shown() {
        let state = PodsState::Loaded {
            rows: vec![pod("api-1"), pod("api-2")],
            selector_note: None,
        };
        let rendered = render_filtered(&state, None, Order::default(), Direction::default(), "2");

        assert!(rendered.contains("api-2"), "{rendered}");
        assert!(!rendered.contains("api-1"), "{rendered}");
    }

    #[test]
    fn a_filter_with_no_match_says_so_rather_than_this_node_has_no_pods() {
        let state = PodsState::Loaded {
            rows: vec![pod("api-1")],
            selector_note: None,
        };
        let rendered =
            render_filtered(&state, None, Order::default(), Direction::default(), "nope");

        assert!(
            rendered.contains("No pods here match \"nope\"."),
            "{rendered}"
        );
        assert!(!rendered.contains("This node has no pods"), "{rendered}");
    }

    #[test]
    fn a_filter_with_no_match_is_distinguishable_from_an_empty_selector_result() {
        // The selector-driven empty message names the selector, not the
        // literal word "filter" — a `/` filter with no match must not be
        // confused with it even though both read "no pods here match …".
        let state = PodsState::Loaded {
            rows: vec![pod("api-1")],
            selector_note: Some("label selector `app=api`".to_owned()),
        };
        let rendered =
            render_filtered(&state, None, Order::default(), Direction::default(), "nope");

        assert!(
            rendered.contains("No pods here match \"nope\"."),
            "{rendered}"
        );
        assert!(!rendered.contains("label selector"), "{rendered}");
    }

    #[test]
    fn the_drilled_into_nodes_wide_facts_are_drawn_above_the_pod_list() {
        let state = PodsState::Loaded {
            rows: vec![pod("api-1")],
            selector_note: None,
        };
        let node = node_row("worker-1");

        let rendered = render_with_node(&state, Some(&node));

        assert!(rendered.contains("INTERNAL-IP: 10.0.1.9"), "{rendered}");
        assert!(rendered.contains("EXTERNAL-IP: -"), "{rendered}");
        assert!(
            rendered.contains("OS-IMAGE: Amazon Linux 2023"),
            "{rendered}"
        );
        assert!(rendered.contains("KERNEL-VERSION: 6.1.148"), "{rendered}");
        assert!(
            rendered.contains("CONTAINER-RUNTIME: containerd://1.7.28"),
            "{rendered}"
        );
        assert!(rendered.contains("api-1"), "{rendered}");
    }

    #[test]
    fn no_node_facts_are_drawn_when_the_node_has_left_the_node_panes_listing() {
        let state = PodsState::Loaded {
            rows: vec![pod("api-1")],
            selector_note: None,
        };

        let rendered = render_with_node(&state, None);

        assert!(!rendered.contains("INTERNAL-IP"), "{rendered}");
        assert_eq!(rendered, render(&state, None));
    }

    #[test]
    fn the_nodes_wide_facts_are_drawn_even_while_its_pods_are_still_loading() {
        // Known before this pane's own fetch even started — see `draw`'s doc
        // comment — so there is no reason to wait for the pod listing before
        // showing them.
        let node = node_row("worker-1");

        let rendered = render_with_node(&PodsState::Loading, Some(&node));

        assert!(rendered.contains("INTERNAL-IP: 10.0.1.9"), "{rendered}");
        assert!(rendered.contains("Loading pods"), "{rendered}");
    }

    #[test]
    fn a_pane_with_no_node_never_prints_a_blank_facts_line() {
        // `node_facts_lines` returns nothing at all for `None`, rather than
        // five dashes — there is a real difference between "this node has
        // reported none of its wide facts" and "there is no node to ask".
        assert!(node_facts_lines(None, Theme::dark()).is_empty());
    }
}
