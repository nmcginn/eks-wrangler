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
use crate::k8s::pods::containers::resources_summary;
use crate::k8s::pods::row;
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
    ///
    /// `ip`, `nominated_node`, and `readiness_gates` are pod-level, not
    /// per-container — the same three facts `eks pods --wide` holds back into
    /// its own columns, shown here instead of behind a wide mode this pane
    /// does not have. See decision 72.
    Loaded {
        rows: Vec<ContainerRow>,
        ip: String,
        nominated_node: String,
        readiness_gates: Option<String>,
    },
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
            Self::Loaded { rows, .. } => rows,
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
/// of order would cost more than it answered. `filter` is the `/` query,
/// empty when no filter is active — see [`super::nodes::draw`]'s doc comment
/// for the same split between what stays over the full `rows` and what
/// narrows.
pub(super) fn draw(
    frame: &mut Frame,
    area: Rect,
    state: &ContainersState,
    selected: Option<usize>,
    filter: &str,
    theme: Theme,
) {
    let lines: Vec<Line> = match state {
        ContainersState::Loading => vec![Line::styled("Loading containers…", theme.dim())],
        ContainersState::Error(message) => vec![Line::styled(
            message.clone(),
            theme.severity(Severity::Critical),
        )],
        ContainersState::Loaded { rows, .. } if rows.is_empty() => {
            vec![Line::styled("This pod has no containers.", theme.dim())]
        }
        ContainersState::Loaded {
            rows,
            ip,
            nominated_node,
            readiness_gates,
        } => {
            let mut lines = identity_lines(ip, nominated_node, readiness_gates.as_deref(), theme);
            lines.push(Line::styled("CONTAINERS", theme.heading()));
            if !filter.is_empty() {
                lines.push(Line::styled(format!("Filter: \"{filter}\""), theme.dim()));
            }
            let visible = crate::fuzzy::rank(filter, rows, |row| row.name.as_str());
            if !filter.is_empty() && visible.is_empty() {
                lines.push(Line::styled(
                    format!("No containers match \"{filter}\"."),
                    theme.dim(),
                ));
            } else {
                lines.extend(
                    visible.into_iter().enumerate().flat_map(|(index, row)| {
                        container_lines(row, Some(index) == selected, theme)
                    }),
                );
            }
            lines
        }
    };

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
}

/// The pod-level facts above the container list.
///
/// `IP` is always shown, including `-` for a pod the CNI has not reached yet
/// — that is itself the answer to "why can nothing route to this pod".
/// `NOMINATED NODE` and `READINESS GATES` follow the CLI table's own
/// judgement of when they are worth a line: nearly every pod has neither, so
/// they appear only when there is something to say, the same `any`-not-`all`
/// rule the columns they came from are built on.
fn identity_lines(
    ip: &str,
    nominated_node: &str,
    readiness_gates: Option<&str>,
    theme: Theme,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(format!("IP: {ip}"), theme.dim())];
    if nominated_node != row::UNKNOWN {
        lines.push(Line::styled(
            format!("Nominated node: {nominated_node}"),
            theme.dim(),
        ));
    }
    if let Some(gates) = readiness_gates {
        lines.push(Line::styled(
            format!("Readiness gates: {gates}"),
            theme.dim(),
        ));
    }
    lines
}

/// The two lines one container occupies: its identity and state, then its
/// requests and limits underneath. A second line rather than more columns on
/// the first — `resources_summary` already produces two full sentences, and
/// a row this wide would either truncate on any real terminal or force every
/// other row's columns to make room for a detail most of them will not need
/// to read closely.
fn container_lines(row: &ContainerRow, selected: bool, theme: Theme) -> Vec<Line<'static>> {
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

    let identity = Line::from(spans);
    let identity = if selected {
        identity.style(theme.selected())
    } else {
        identity
    };

    let (requests, limits) = resources_summary(row);
    let resources = Line::styled(format!("  {requests}  {limits}"), theme.dim());

    vec![identity, resources]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::k8s::pods::Requests;
    use crate::k8s::quantity::Quantity;

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
            requests: Requests::default(),
            cpu_limit: None,
            memory_limit: None,
        }
    }

    /// A `Loaded` state with no pod-level facts worth naming in a test that is
    /// only asking about the container list — the identity-line tests below
    /// use [`loaded_with`] instead, to set `ip`/`nominated_node`/
    /// `readiness_gates` directly.
    fn loaded(rows: Vec<ContainerRow>) -> ContainersState {
        loaded_with(rows, row::UNKNOWN, row::UNKNOWN, None)
    }

    /// [`loaded`] with every pod-level fact spelled out. A plain function
    /// rather than `ContainersState::Loaded { .., ..loaded(rows) }`: functional
    /// record update only works against a struct literal, not a call that
    /// happens to return one, and `ContainersState::Loaded` is an enum
    /// variant either way.
    fn loaded_with(
        rows: Vec<ContainerRow>,
        ip: &str,
        nominated_node: &str,
        readiness_gates: Option<&str>,
    ) -> ContainersState {
        ContainersState::Loaded {
            rows,
            ip: ip.to_owned(),
            nominated_node: nominated_node.to_owned(),
            readiness_gates: readiness_gates.map(str::to_owned),
        }
    }

    fn render(state: &ContainersState, selected: Option<usize>) -> String {
        render_filtered(state, selected, "")
    }

    fn render_filtered(state: &ContainersState, selected: Option<usize>, filter: &str) -> String {
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw(frame, area, state, selected, filter, Theme::dark());
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
        let state = loaded(vec![container("app"), container("sidecar")]);
        let rendered = render(&state, None);

        assert!(rendered.contains("app"), "{rendered}");
        assert!(rendered.contains("sidecar"), "{rendered}");
        assert!(rendered.contains("Running"), "{rendered}");
        assert!(rendered.contains("app:1.0"), "{rendered}");
    }

    #[test]
    fn an_init_container_is_marked_as_one() {
        let state = loaded(vec![ContainerRow {
            init: true,
            ..container("migrate")
        }]);
        let rendered = render(&state, None);
        assert!(rendered.contains("migrate (init)"), "{rendered}");
    }

    #[test]
    fn an_empty_container_list_says_so_rather_than_rendering_nothing() {
        let state = loaded(Vec::new());
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
        let state = loaded(vec![container("app")]);
        assert_eq!(state.rows().len(), 1);
    }

    #[test]
    fn rendering_the_containers_pane_survives_a_tiny_terminal() {
        let state = loaded(vec![container("app")]);
        for (width, height) in [(1, 1), (8, 3), (20, 2), (200, 60)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    draw(frame, area, &state, Some(0), "", Theme::dark());
                })
                .unwrap();
        }
    }

    #[test]
    fn a_containers_requests_and_limits_appear_under_its_identity_line() {
        let row = ContainerRow {
            requests: Requests {
                cpu: Quantity::parse("250m").unwrap(),
                memory: Quantity::parse("512Mi").unwrap(),
                ..Default::default()
            },
            cpu_limit: Some(Quantity::parse("500m").unwrap()),
            memory_limit: None,
            ..container("app")
        };
        let state = loaded(vec![row]);
        let rendered = render(&state, None);

        assert!(
            rendered.contains("requests: cpu 250m, memory 512Mi"),
            "{rendered}"
        );
        assert!(
            rendered.contains("limits: cpu 500m, memory unlimited"),
            "{rendered}"
        );
    }

    #[test]
    fn a_long_resources_line_wraps_at_80_columns_instead_of_truncating() {
        let row = ContainerRow {
            requests: Requests {
                cpu: Quantity::parse("250m").unwrap(),
                memory: Quantity::parse("512Mi").unwrap(),
                extended: [(
                    "example.com/a-very-long-extended-resource-name".to_owned(),
                    Quantity::parse("4").unwrap(),
                )]
                .into_iter()
                .collect(),
            },
            cpu_limit: Some(Quantity::parse("500m").unwrap()),
            memory_limit: Some(Quantity::parse("1Gi").unwrap()),
            ..container("app")
        };
        let state = loaded(vec![row]);

        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw(frame, area, &state, None, "", Theme::dark());
            })
            .unwrap();
        let rendered = terminal.backend().to_string();

        // No line is long enough to have been cut, and the whole resource
        // name survives somewhere in the wrapped output rather than being
        // replaced by an ellipsis.
        assert!(
            rendered.contains("example.com/a-very-long-extended-resource-name"),
            "{rendered}"
        );
        assert!(!rendered.contains('…'), "{rendered}");
    }

    #[test]
    fn an_empty_filter_renders_exactly_as_no_filter_does() {
        let state = loaded(vec![container("app"), container("sidecar")]);
        assert_eq!(render_filtered(&state, None, ""), render(&state, None));
    }

    #[test]
    fn a_filter_narrows_the_rows_shown() {
        // Distinct images, not just distinct names: `container()` always
        // gives every row the same `app:1.0` image, which would leave "app"
        // in the rendered text of the row this filter is supposed to drop.
        let state = loaded(vec![
            ContainerRow {
                image: "app-image:1.0".to_owned(),
                ..container("app")
            },
            ContainerRow {
                image: "sidecar-image:1.0".to_owned(),
                ..container("sidecar")
            },
        ]);
        let rendered = render_filtered(&state, None, "side");

        assert!(rendered.contains("sidecar"), "{rendered}");
        assert!(!rendered.contains("app-image"), "{rendered}");
    }

    #[test]
    fn a_filter_with_no_match_says_so_rather_than_this_pod_has_no_containers() {
        let state = loaded(vec![container("app")]);
        let rendered = render_filtered(&state, None, "nope");

        assert!(
            rendered.contains("No containers match \"nope\"."),
            "{rendered}"
        );
        assert!(
            !rendered.contains("This pod has no containers"),
            "{rendered}"
        );
    }

    #[test]
    fn the_ip_line_always_shows_even_before_the_cni_has_assigned_one() {
        let rendered = render(&loaded(vec![container("app")]), None);
        assert!(
            rendered.contains(&format!("IP: {}", row::UNKNOWN)),
            "{rendered}"
        );
    }

    #[test]
    fn the_ip_line_shows_the_pods_assigned_address() {
        let state = loaded_with(vec![container("app")], "10.0.4.12", row::UNKNOWN, None);
        let rendered = render(&state, None);
        assert!(rendered.contains("IP: 10.0.4.12"), "{rendered}");
    }

    #[test]
    fn nominated_node_says_nothing_for_a_pod_that_is_not_being_preempted_onto_one() {
        let rendered = render(&loaded(vec![container("app")]), None);
        assert!(!rendered.contains("Nominated node"), "{rendered}");
    }

    #[test]
    fn a_pod_being_preempted_onto_a_node_names_it() {
        let state = loaded_with(vec![container("app")], row::UNKNOWN, "worker-9", None);
        let rendered = render(&state, None);
        assert!(rendered.contains("Nominated node: worker-9"), "{rendered}");
    }

    #[test]
    fn readiness_gates_say_nothing_for_a_pod_that_declares_none() {
        let rendered = render(&loaded(vec![container("app")]), None);
        assert!(!rendered.contains("Readiness gates"), "{rendered}");
    }

    #[test]
    fn a_pods_readiness_gates_show_how_many_are_satisfied() {
        let state = loaded_with(
            vec![container("app")],
            row::UNKNOWN,
            row::UNKNOWN,
            Some("1/2"),
        );
        let rendered = render(&state, None);
        assert!(rendered.contains("Readiness gates: 1/2"), "{rendered}");
    }

    #[test]
    fn an_empty_container_list_shows_no_pod_level_facts_either() {
        let state = loaded_with(Vec::new(), "10.0.4.12", "worker-9", Some("1/2"));
        let rendered = render(&state, None);
        assert!(!rendered.contains("IP:"), "{rendered}");
        assert!(!rendered.contains("Nominated node"), "{rendered}");
        assert!(!rendered.contains("Readiness gates"), "{rendered}");
    }
}
