//! The pod-drilldown pane: the pods placed on one node.
//!
//! Fetching lives in [`crate::commands::pods::spawn_gather_for_node`]; this
//! module only draws whatever [`PodsState`] `App` was last handed — the same
//! split [`super::nodes`] keeps between computation and rendering.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::k8s::pods::PodRow;
use crate::theme::{Severity, Theme};

/// What the pod-drilldown pane is showing, independent of how it is drawn.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PodsState {
    /// The fetch is still in flight.
    #[default]
    Loading,
    /// The cluster answered — possibly with zero pods, a real answer for a
    /// node that is cordoned or has just joined.
    Loaded { rows: Vec<PodRow> },
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
            Self::Loaded { rows } => rows,
            Self::Loading | Self::Error(_) => &[],
        }
    }
}

/// Draw whatever the pod pane currently knows.
///
/// `selected` highlights a row — `None` when the pane does not currently
/// hold keyboard focus, so the highlight disappears the moment `Tab` moves
/// it back to the sidebar.
pub(super) fn draw(
    frame: &mut Frame,
    area: Rect,
    state: &PodsState,
    selected: Option<usize>,
    theme: Theme,
) {
    let lines: Vec<Line> = match state {
        PodsState::Loading => vec![Line::styled("Loading pods…", theme.dim())],
        PodsState::Error(message) => vec![Line::styled(
            message.clone(),
            theme.severity(Severity::Critical),
        )],
        PodsState::Loaded { rows } if rows.is_empty() => {
            vec![Line::styled("This node has no pods.", theme.dim())]
        }
        PodsState::Loaded { rows } => {
            let mut lines = vec![Line::styled("PODS", theme.heading())];
            lines.extend(
                rows.iter()
                    .enumerate()
                    .map(|(index, row)| pod_line(row, Some(index) == selected, theme)),
            );
            lines
        }
    };

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
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

    fn render(state: &PodsState, selected: Option<usize>) -> String {
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw(frame, area, state, selected, Theme::dark());
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
        };
        let rendered = render(&state, None);

        assert!(rendered.contains("api-1"), "{rendered}");
        assert!(rendered.contains("api-2"), "{rendered}");
        assert!(rendered.contains("Running"), "{rendered}");
    }

    #[test]
    fn an_empty_pod_list_says_so_rather_than_rendering_nothing() {
        let rendered = render(&PodsState::Loaded { rows: Vec::new() }, None);
        assert!(rendered.contains("no pods"), "{rendered}");
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
        };
        assert_eq!(state.rows().len(), 1);
    }

    #[test]
    fn rendering_the_pod_pane_survives_a_tiny_terminal() {
        let state = PodsState::Loaded {
            rows: vec![pod("api-1")],
        };
        for (width, height) in [(1, 1), (8, 3), (20, 2), (200, 60)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    draw(frame, area, &state, Some(0), Theme::dark());
                })
                .unwrap();
        }
    }
}
