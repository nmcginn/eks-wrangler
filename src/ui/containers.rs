//! The pod-containers pane: the containers of one pod.
//!
//! Fetching lives in [`crate::commands::pods::spawn_gather_containers`]; this
//! module only draws whatever [`ContainersState`] `App` was last handed — the
//! same split [`super::pods`] and [`super::nodes`] keep between computation
//! and rendering.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::k8s::pods::ContainerRow;
use crate::theme::{Severity, Theme};

/// What the pod-containers pane is showing, independent of how it is drawn.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ContainersState {
    /// The fetch is still in flight.
    #[default]
    Loading,
    /// The cluster answered. Empty only for a pod with no containers at all,
    /// which should not arise in practice — every pod the reader could have
    /// drilled into has at least one.
    Loaded { rows: Vec<ContainerRow> },
    /// The fetch failed; the message is already a full sentence, via
    /// `k8s::explain`.
    Error(String),
}

impl ContainersState {
    /// The rows this state is showing, or an empty slice for every state that
    /// has none — the same shape [`super::pods::PodsState::rows`] and
    /// [`super::nodes::NodesState::rows`] have, so `App` can bound its row
    /// selection against "however many rows there are" without matching on
    /// the state itself.
    #[must_use]
    pub fn rows(&self) -> &[ContainerRow] {
        match self {
            Self::Loaded { rows } => rows,
            Self::Loading | Self::Error(_) => &[],
        }
    }
}

/// Draw whatever the pod-containers pane currently knows.
///
/// `selected` highlights a row — `None` when the pane does not currently
/// hold keyboard focus, matching [`super::pods::draw`]'s own rule. There is
/// no ordering here yet, unlike the node and pod panes: a pod rarely has more
/// than a handful of containers, in spec order already, and reading them out
/// of order would cost more than it answered.
pub(super) fn draw(
    frame: &mut Frame,
    area: Rect,
    state: &ContainersState,
    selected: Option<usize>,
    theme: Theme,
) {
    let lines: Vec<Line> = match state {
        ContainersState::Loading => vec![Line::styled("Loading containers…", theme.dim())],
        ContainersState::Error(message) => vec![Line::styled(
            message.clone(),
            theme.severity(Severity::Critical),
        )],
        ContainersState::Loaded { rows } if rows.is_empty() => {
            vec![Line::styled("This pod has no containers.", theme.dim())]
        }
        ContainersState::Loaded { rows } => {
            let mut lines = vec![Line::styled("CONTAINERS", theme.heading())];
            lines.extend(
                rows.iter()
                    .enumerate()
                    .map(|(index, row)| container_line(row, Some(index) == selected, theme)),
            );
            lines
        }
    };

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
}

fn container_line(row: &ContainerRow, selected: bool, theme: Theme) -> Line<'static> {
    let name = if row.init {
        format!("{} (init)", row.name)
    } else {
        row.name.clone()
    };
    let ready = if row.ready { "ready" } else { "not ready" };

    let spans = vec![
        Span::styled(name, theme.body()),
        Span::raw("  "),
        Span::styled(row.state.clone(), theme.severity(row.severity)),
        Span::raw("  "),
        Span::styled(ready.to_owned(), theme.dim()),
        Span::raw("  "),
        Span::styled(row.restarts.to_string(), theme.dim()),
        Span::raw("  "),
        Span::styled(row.image.clone(), theme.dim()),
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

    fn container(name: &str) -> ContainerRow {
        ContainerRow {
            name: name.to_owned(),
            image: "app:1.0".to_owned(),
            init: false,
            ready: true,
            restarts: 0,
            state: "Running".to_owned(),
            severity: Severity::Ok,
        }
    }

    fn render(state: &ContainersState, selected: Option<usize>) -> String {
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
        let rendered = render(&ContainersState::Loading, None);
        assert!(rendered.contains("Loading containers"), "{rendered}");
    }

    #[test]
    fn loaded_state_shows_each_containers_name_state_and_image() {
        let state = ContainersState::Loaded {
            rows: vec![container("app"), container("sidecar")],
        };
        let rendered = render(&state, None);

        assert!(rendered.contains("app"), "{rendered}");
        assert!(rendered.contains("sidecar"), "{rendered}");
        assert!(rendered.contains("Running"), "{rendered}");
        assert!(rendered.contains("app:1.0"), "{rendered}");
    }

    #[test]
    fn an_init_container_is_marked_as_one() {
        let state = ContainersState::Loaded {
            rows: vec![ContainerRow {
                init: true,
                ..container("migrate")
            }],
        };
        let rendered = render(&state, None);
        assert!(rendered.contains("migrate (init)"), "{rendered}");
    }

    #[test]
    fn an_empty_container_list_says_so_rather_than_rendering_nothing() {
        let state = ContainersState::Loaded { rows: Vec::new() };
        let rendered = render(&state, None);
        assert!(rendered.contains("no containers"), "{rendered}");
    }

    #[test]
    fn error_state_renders_the_message_instead_of_a_list() {
        let rendered = render(
            &ContainersState::Error("could not get pod: nope".to_owned()),
            None,
        );
        assert!(rendered.contains("could not get pod"), "{rendered}");
    }

    #[test]
    fn rows_returns_nothing_for_loading_or_error() {
        assert!(ContainersState::Loading.rows().is_empty());
        assert!(ContainersState::Error("nope".to_owned()).rows().is_empty());
    }

    #[test]
    fn rows_returns_the_loaded_rows() {
        let state = ContainersState::Loaded {
            rows: vec![container("app")],
        };
        assert_eq!(state.rows().len(), 1);
    }

    #[test]
    fn rendering_the_containers_pane_survives_a_tiny_terminal() {
        let state = ContainersState::Loaded {
            rows: vec![container("app")],
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
