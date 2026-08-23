//! The node pane: a live list of a cluster's nodes.
//!
//! Fetching lives in [`crate::commands::nodes::spawn_gather`]; this module
//! only draws whatever [`NodesState`] `App` was last handed — the same split
//! the rest of `ui::` keeps, computation and rendering apart.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::k8s::nodes::{Capacity, NodeRow, Order, Share};
use crate::k8s::order::{self, Direction};
use crate::k8s::quantity::{self, Quantity};
use crate::theme::{Severity, Theme};

/// How many terminal cells wide a utilisation bar is drawn.
const BAR_WIDTH: u16 = 10;

/// What the node pane is showing, independent of how it is drawn.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum NodesState {
    /// The fetch is still in flight.
    #[default]
    Loading,
    /// The cluster answered — possibly with zero nodes, a real answer for a
    /// control-plane-only or still-provisioning cluster.
    Loaded {
        rows: Vec<NodeRow>,
        /// How stale the usage bars are, worded through
        /// `k8s::nodes::usage_note` — the CLI table's freshness/unsampled
        /// footnote, carried into the pane instead of a footnote list. `None`
        /// when the columns have nothing to date, including when the metrics
        /// read failed outright: a bar reading `-` already says so.
        usage_note: Option<String>,
        /// Set when the most recent *background* refresh failed after an
        /// earlier fetch had already succeeded. The rows shown are still the
        /// last good listing — wiping them because one poll failed would
        /// make a transient network blip look like the cluster lost every
        /// node.
        refresh_error: Option<String>,
    },
    /// The fetch failed; the message is already a full sentence, via
    /// `k8s::explain`.
    Error(String),
}

impl NodesState {
    /// The rows this state is showing, or an empty slice for every state
    /// that has none — which is what lets `App` bound its row selection
    /// against "however many rows there are" without matching on the state
    /// itself.
    #[must_use]
    pub fn rows(&self) -> &[NodeRow] {
        match self {
            Self::Loaded { rows, .. } => rows,
            Self::Loading | Self::Error(_) => &[],
        }
    }
}

/// Draw whatever the node pane currently knows.
///
/// `selected` highlights a row — `None` when the pane does not currently
/// hold keyboard focus, so the highlight disappears the moment `Tab` moves
/// it back to the sidebar. `order` and `direction` are the pane's own
/// ordering, `s`/`S` in [`super::App`] rather than a request — the note they
/// produce is silent on the default order, exactly as it is under the CLI
/// table, and it disappears along with the rest of the pane's chrome when
/// there are no rows to be sorted.
pub(super) fn draw(
    frame: &mut Frame,
    area: Rect,
    state: &NodesState,
    selected: Option<usize>,
    order: Order,
    direction: Direction,
    theme: Theme,
) {
    let lines: Vec<Line> = match state {
        NodesState::Loading => vec![Line::styled("Loading nodes…", theme.dim())],
        NodesState::Error(message) => vec![Line::styled(
            message.clone(),
            theme.severity(Severity::Critical),
        )],
        NodesState::Loaded { rows, .. } if rows.is_empty() => {
            vec![Line::styled("This cluster has no nodes.", theme.dim())]
        }
        NodesState::Loaded {
            rows,
            usage_note,
            refresh_error,
        } => {
            let mut lines = vec![Line::styled("NODES", theme.heading())];
            if let Some(error) = refresh_error {
                lines.push(Line::styled(
                    format!("Last refresh failed: {error}"),
                    theme.severity(Severity::Warn),
                ));
            }
            // Split rather than handed straight to one `Line`: a stale sample
            // earns a second sentence of advice, and `ratatui` does not treat
            // an embedded `\n` as a line break the way a terminal does.
            if let Some(note) = usage_note {
                lines.extend(
                    note.lines()
                        .map(|line| Line::styled(line.to_owned(), theme.dim())),
                );
            }
            if let Some(note) = order::note(order, direction) {
                lines.push(Line::styled(note, theme.dim()));
            }
            lines.extend(
                rows.iter()
                    .enumerate()
                    .map(|(index, row)| node_line(row, Some(index) == selected, theme)),
            );
            lines
        }
    };

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
}

fn node_line(row: &NodeRow, selected: bool, theme: Theme) -> Line<'static> {
    let mut spans = vec![
        Span::styled(row.name.clone(), theme.body()),
        Span::raw("  "),
        Span::styled(row.status.clone(), theme.severity(row.severity)),
        Span::raw("  "),
    ];
    spans.extend(bar("CPU", row.cpu_used, row.cpu, quantity::cpu, theme));
    spans.push(Span::raw("  "));
    spans.extend(bar(
        "MEM",
        row.memory_used,
        row.memory,
        quantity::memory,
        theme,
    ));
    spans.push(Span::raw("  "));
    spans.push(Span::styled(pods_text(row.pods), theme.dim()));

    let line = Line::from(spans);
    if selected {
        line.style(theme.selected())
    } else {
        line
    }
}

/// One labelled utilisation bar: `CPU ███████░░░ 1.5/4`.
///
/// The CLI's `CPU USE` column reads `share` against *allocatable* — the
/// right denominator for "will another pod fit". A bar is asking "is this
/// machine busy", so it fills and colours against `capacity`'s raw
/// `capacity` figure instead: a node pinned at 100% of allocatable still has
/// the kubelet's own reserve behind it, and should not draw as a full bar for
/// headroom nothing can schedule into. The figure printed beside the bar is
/// still `share.amount` — the two readings never disagree about what is
/// actually being used, only about what it is a share of.
fn bar(
    label: &'static str,
    share: Share,
    capacity: Capacity,
    show: fn(Quantity) -> String,
    theme: Theme,
) -> Vec<Span<'static>> {
    let ratio = share.ratio_of(capacity.capacity);
    let filled = filled_cells(ratio, BAR_WIDTH);
    let empty = BAR_WIDTH - filled;
    let text = share.amount.map_or_else(|| "-".to_owned(), show);

    vec![
        Span::styled(format!("{label} "), theme.dim()),
        Span::styled(
            "█".repeat(usize::from(filled)),
            theme.severity(share.severity_of(capacity.capacity)),
        ),
        Span::styled("░".repeat(usize::from(empty)), theme.dim()),
        Span::raw(" "),
        Span::styled(text, theme.body()),
    ]
}

/// How many of `width` cells a ratio fills.
///
/// Rounded rather than truncated, so a bar at 86% reads as "nearly full"
/// rather than "mostly empty" at low widths. Clamped at both ends: `None` (no
/// reading yet) draws empty, and a ratio over 1.0 — usage above allocatable is
/// a real reading a node can have — draws full rather than overflowing the
/// bar.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn filled_cells(ratio: Option<f64>, width: u16) -> u16 {
    let Some(ratio) = ratio else { return 0 };
    (ratio.clamp(0.0, 1.0) * f64::from(width)).round() as u16
}

fn pods_text(pods: Share) -> String {
    match (pods.amount, pods.allocatable) {
        (Some(used), Some(total)) => {
            format!("{}/{} pods", quantity::count(used), quantity::count(total))
        }
        (Some(used), None) => format!("{} pods", quantity::count(used)),
        _ => "- pods".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::BTreeMap;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn share(amount: &str, allocatable: &str) -> Share {
        Share {
            amount: Some(Quantity::parse(amount).unwrap()),
            allocatable: Some(Quantity::parse(allocatable).unwrap()),
        }
    }

    fn capacity(allocatable: &str, capacity: &str) -> Capacity {
        Capacity {
            allocatable: Some(Quantity::parse(allocatable).unwrap()),
            capacity: Some(Quantity::parse(capacity).unwrap()),
        }
    }

    fn node(name: &str) -> NodeRow {
        NodeRow {
            name: name.to_owned(),
            status: "Ready".to_owned(),
            severity: Severity::Ok,
            version: "v1.31".to_owned(),
            // Allocatable a little below capacity, the ordinary kubelet
            // reserve, so this fixture's bars exercise both denominators
            // rather than only the one every other field happens to share.
            cpu: capacity("4", "4.2"),
            memory: capacity("8Gi", "8.5Gi"),
            cpu_requested: Share::default(),
            memory_requested: Share::default(),
            cpu_used: share("1500m", "4"),
            memory_used: share("2Gi", "8Gi"),
            pods: share("12", "58"),
            age: "3d".to_owned(),
            created_at: None,
            internal_ip: "-".to_owned(),
            external_ip: "-".to_owned(),
            os_image: "-".to_owned(),
            kernel_version: "-".to_owned(),
            container_runtime: "-".to_owned(),
            devices: BTreeMap::new(),
        }
    }

    fn loaded(rows: Vec<NodeRow>) -> NodesState {
        NodesState::Loaded {
            rows,
            usage_note: None,
            refresh_error: None,
        }
    }

    fn render(state: &NodesState) -> String {
        render_ordered(state, Order::default(), Direction::default())
    }

    fn render_ordered(state: &NodesState, order: Order, direction: Direction) -> String {
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw(frame, area, state, None, order, direction, Theme::dark());
            })
            .unwrap();
        terminal.backend().to_string()
    }

    #[test]
    fn filled_cells_is_zero_when_the_ratio_is_unknown() {
        assert_eq!(filled_cells(None, 10), 0);
    }

    #[test]
    fn filled_cells_clamps_to_the_bar_width_when_usage_exceeds_allocatable() {
        assert_eq!(filled_cells(Some(1.5), 10), 10);
    }

    #[test]
    fn filled_cells_rounds_rather_than_truncates() {
        // 0.86 of 10 cells is 8.6, which reads as "nearly full" rounded to 9,
        // not "mostly empty" truncated to 8.
        assert_eq!(filled_cells(Some(0.86), 10), 9);
    }

    #[test]
    fn the_bar_fills_and_colours_against_capacity_rather_than_allocatable() {
        // 5.5 of 6 allocatable cores is 92% — over the critical threshold,
        // the reading the CLI's `CPU USE` column would show. The same 5.5
        // against the node's raw 8-core capacity is 69%, comfortably ok: the
        // kubelet's own reserve is headroom nothing can schedule into, and
        // the bar should read the second number, not the first.
        let hot_by_allocatable = capacity("6", "8");
        let used = share("5500m", "6");

        let spans = bar(
            "CPU",
            used,
            hot_by_allocatable,
            quantity::cpu,
            Theme::dark(),
        );

        assert_eq!(
            spans[1].content.chars().count(),
            7,
            "5.5/8 = 69%, rounds to 7 of 10 cells, not 9 of 10 for 5.5/6"
        );
        assert_eq!(spans[1].style, Theme::dark().severity(Severity::Ok));
    }

    #[test]
    fn loading_state_renders_before_any_row_exists() {
        let rendered = render(&NodesState::Loading);
        assert!(rendered.contains("Loading nodes"), "{rendered}");
    }

    #[test]
    fn loaded_state_shows_each_nodes_name_and_status() {
        let rendered = render(&loaded(vec![node("worker-1"), node("worker-2")]));
        assert!(rendered.contains("worker-1"), "{rendered}");
        assert!(rendered.contains("worker-2"), "{rendered}");
        assert!(rendered.contains("Ready"), "{rendered}");
    }

    #[test]
    fn an_empty_node_list_says_so_rather_than_rendering_nothing() {
        let rendered = render(&loaded(Vec::new()));
        assert!(rendered.contains("no nodes"), "{rendered}");
    }

    #[test]
    fn error_state_renders_the_message_instead_of_a_table() {
        let rendered = render(&NodesState::Error("could not list nodes: nope".to_owned()));
        assert!(rendered.contains("could not list nodes"), "{rendered}");
    }

    #[test]
    fn rendering_the_node_pane_survives_a_tiny_terminal() {
        let state = loaded(vec![node("worker-1")]);
        for (width, height) in [(1, 1), (8, 3), (20, 2), (200, 60)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    draw(
                        frame,
                        area,
                        &state,
                        None,
                        Order::default(),
                        Direction::default(),
                        Theme::dark(),
                    );
                })
                .unwrap();
        }
    }

    #[test]
    fn a_usage_note_appears_above_the_rows_it_dates() {
        let state = NodesState::Loaded {
            rows: vec![node("worker-1")],
            usage_note: Some("Usage is up to 8s old, averaged over 20s.".to_owned()),
            refresh_error: None,
        };

        let rendered = render(&state);

        assert!(rendered.contains("Usage is up to 8s old"), "{rendered}");
        assert!(rendered.contains("worker-1"), "{rendered}");
    }

    #[test]
    fn a_multi_line_usage_note_renders_as_separate_lines() {
        // `freshness_note`'s stale wording is two sentences joined by `\n`;
        // `ratatui` does not treat that as a line break on its own, so `draw`
        // has to split it before handing it to a `Line`.
        let state = NodesState::Loaded {
            rows: vec![node("worker-1")],
            usage_note: Some(
                "Usage is up to 6m10s old, averaged over 20s \u{2014} more than two sampling \
                 windows, so these figures are stale.\n\
                 metrics-server can stop scraping without failing this request; check its pod \
                 in kube-system."
                    .to_owned(),
            ),
            refresh_error: None,
        };

        // Wide enough that neither sentence wraps a second time on top of the
        // split this test is actually about.
        let mut terminal = Terminal::new(TestBackend::new(200, 20)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw(
                    frame,
                    area,
                    &state,
                    None,
                    Order::default(),
                    Direction::default(),
                    Theme::dark(),
                );
            })
            .unwrap();
        let rendered = terminal.backend().to_string();

        assert!(rendered.contains("these figures are stale"), "{rendered}");
        assert!(rendered.contains("kube-system"), "{rendered}");
    }

    #[test]
    fn the_default_order_says_nothing_about_sorting() {
        let rendered = render(&loaded(vec![node("worker-1")]));
        assert!(!rendered.contains("Sorted by"), "{rendered}");
    }

    #[test]
    fn a_reordered_pane_names_the_ordering_under_its_rows() {
        let rendered = render_ordered(
            &loaded(vec![node("worker-1")]),
            Order::Cpu,
            Direction::Natural,
        );
        assert!(rendered.contains("Sorted by cpu."), "{rendered}");
    }

    #[test]
    fn a_reversed_pane_says_which_way_round_it_ran() {
        let rendered = render_ordered(
            &loaded(vec![node("worker-1")]),
            Order::Cpu,
            Direction::Reversed,
        );
        assert!(rendered.contains("Sorted by cpu, reversed."), "{rendered}");
    }

    #[test]
    fn an_empty_pane_says_nothing_about_the_ordering_it_was_asked_for() {
        // "This cluster has no nodes." is the whole answer; naming an
        // ordering over zero rows would be noise under it, exactly as the
        // CLI table drops every footnote over an empty listing.
        let rendered = render_ordered(&loaded(Vec::new()), Order::Cpu, Direction::Natural);
        assert!(!rendered.contains("Sorted by"), "{rendered}");
    }

    #[test]
    fn no_usage_note_is_shown_when_there_is_nothing_to_date() {
        let rendered = render(&loaded(vec![node("worker-1")]));
        assert!(!rendered.contains("Usage is up to"), "{rendered}");
    }

    #[test]
    fn a_usage_note_is_not_shown_over_an_empty_node_list() {
        let state = NodesState::Loaded {
            rows: Vec::new(),
            usage_note: Some("Usage is up to 8s old, averaged over 20s.".to_owned()),
            refresh_error: None,
        };

        let rendered = render(&state);

        assert!(rendered.contains("no nodes"), "{rendered}");
        assert!(!rendered.contains("Usage is up to"), "{rendered}");
    }

    #[test]
    fn a_failed_refresh_keeps_the_last_good_rows_visible() {
        let state = NodesState::Loaded {
            rows: vec![node("worker-1")],
            usage_note: None,
            refresh_error: Some("could not list nodes: nope".to_owned()),
        };

        let rendered = render(&state);

        assert!(rendered.contains("Last refresh failed"), "{rendered}");
        assert!(rendered.contains("worker-1"), "{rendered}");
    }

    #[test]
    fn no_refresh_error_is_shown_when_the_last_fetch_succeeded() {
        let rendered = render(&loaded(vec![node("worker-1")]));
        assert!(!rendered.contains("Last refresh failed"), "{rendered}");
    }

    #[test]
    fn rows_returns_nothing_for_loading_or_error() {
        assert!(NodesState::Loading.rows().is_empty());
        assert!(NodesState::Error("nope".to_owned()).rows().is_empty());
    }

    #[test]
    fn rows_returns_the_loaded_rows() {
        let state = loaded(vec![node("worker-1"), node("worker-2")]);
        assert_eq!(state.rows().len(), 2);
    }

    #[test]
    fn drawing_a_selected_row_does_not_panic_at_any_width() {
        // The highlight is a background colour patched onto the row's own
        // spans, not a second widget — this is the case that would panic if
        // the index it names were out of bounds.
        let state = loaded(vec![node("worker-1")]);
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw(
                    frame,
                    area,
                    &state,
                    Some(0),
                    Order::default(),
                    Direction::default(),
                    Theme::dark(),
                );
            })
            .unwrap();
    }
}
