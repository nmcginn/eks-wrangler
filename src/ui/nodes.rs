//! The node pane: a live list of a cluster's nodes.
//!
//! Fetching lives in [`crate::commands::nodes::spawn_gather`]; this module
//! only draws whatever [`NodesState`] `App` was last handed — the same split
//! the rest of `ui::` keeps, computation and rendering apart.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::k8s::nodes::{NodeRow, Share};
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
    Loaded(Vec<NodeRow>),
    /// The fetch failed; the message is already a full sentence, via
    /// `k8s::explain`.
    Error(String),
}

/// Draw whatever the node pane currently knows.
pub(super) fn draw(frame: &mut Frame, area: Rect, state: &NodesState, theme: Theme) {
    let lines: Vec<Line> = match state {
        NodesState::Loading => vec![Line::styled("Loading nodes…", theme.dim())],
        NodesState::Error(message) => vec![Line::styled(
            message.clone(),
            theme.severity(Severity::Critical),
        )],
        NodesState::Loaded(rows) if rows.is_empty() => {
            vec![Line::styled("This cluster has no nodes.", theme.dim())]
        }
        NodesState::Loaded(rows) => {
            let mut lines = vec![Line::styled("NODES", theme.heading())];
            lines.extend(rows.iter().map(|row| node_line(row, theme)));
            lines
        }
    };

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
}

fn node_line(row: &NodeRow, theme: Theme) -> Line<'static> {
    let mut spans = vec![
        Span::styled(row.name.clone(), theme.body()),
        Span::raw("  "),
        Span::styled(row.status.clone(), theme.severity(row.severity)),
        Span::raw("  "),
    ];
    spans.extend(bar("CPU", row.cpu_used, quantity::cpu, theme));
    spans.push(Span::raw("  "));
    spans.extend(bar("MEM", row.memory_used, quantity::memory, theme));
    spans.push(Span::raw("  "));
    spans.push(Span::styled(pods_text(row.pods), theme.dim()));
    Line::from(spans)
}

/// One labelled utilisation bar: `CPU ███████░░░ 1.5/4`, filled and coloured
/// by [`Share::ratio`]/[`Share::severity`] — the same figures the CLI's
/// `CPU USE` column reads, divided by the same allocatable denominator, so
/// the bar and the table never disagree about one node.
fn bar(
    label: &'static str,
    share: Share,
    show: fn(Quantity) -> String,
    theme: Theme,
) -> Vec<Span<'static>> {
    let filled = filled_cells(share.ratio(), BAR_WIDTH);
    let empty = BAR_WIDTH - filled;
    let text = share.amount.map_or_else(|| "-".to_owned(), show);

    vec![
        Span::styled(format!("{label} "), theme.dim()),
        Span::styled(
            "█".repeat(usize::from(filled)),
            theme.severity(share.severity()),
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
    use crate::k8s::nodes::Capacity;

    fn share(amount: &str, allocatable: &str) -> Share {
        Share {
            amount: Some(Quantity::parse(amount).unwrap()),
            allocatable: Some(Quantity::parse(allocatable).unwrap()),
        }
    }

    fn node(name: &str) -> NodeRow {
        NodeRow {
            name: name.to_owned(),
            status: "Ready".to_owned(),
            severity: Severity::Ok,
            version: "v1.31".to_owned(),
            cpu: Capacity::default(),
            memory: Capacity::default(),
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

    fn render(state: &NodesState) -> String {
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw(frame, area, state, Theme::dark());
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
    fn loading_state_renders_before_any_row_exists() {
        let rendered = render(&NodesState::Loading);
        assert!(rendered.contains("Loading nodes"), "{rendered}");
    }

    #[test]
    fn loaded_state_shows_each_nodes_name_and_status() {
        let rendered = render(&NodesState::Loaded(vec![
            node("worker-1"),
            node("worker-2"),
        ]));
        assert!(rendered.contains("worker-1"), "{rendered}");
        assert!(rendered.contains("worker-2"), "{rendered}");
        assert!(rendered.contains("Ready"), "{rendered}");
    }

    #[test]
    fn an_empty_node_list_says_so_rather_than_rendering_nothing() {
        let rendered = render(&NodesState::Loaded(Vec::new()));
        assert!(rendered.contains("no nodes"), "{rendered}");
    }

    #[test]
    fn error_state_renders_the_message_instead_of_a_table() {
        let rendered = render(&NodesState::Error("could not list nodes: nope".to_owned()));
        assert!(rendered.contains("could not list nodes"), "{rendered}");
    }

    #[test]
    fn rendering_the_node_pane_survives_a_tiny_terminal() {
        let state = NodesState::Loaded(vec![node("worker-1")]);
        for (width, height) in [(1, 1), (8, 3), (20, 2), (200, 60)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    draw(frame, area, &state, Theme::dark());
                })
                .unwrap();
        }
    }
}
