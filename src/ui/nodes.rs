//! The node pane: a live list of a cluster's nodes.
//!
//! Fetching lives in [`crate::commands::nodes::spawn_gather`]; this module
//! only draws whatever [`NodesState`] `App` was last handed — the same split
//! the rest of `ui::` keeps, computation and rendering apart.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::k8s::nodes::{
    Capacity, Missing, NodeRow, Order, Share, cause, distinguishes, ranks_any,
    usage_missing_explained,
};
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

/// The line offering `L`, when the failure on screen is one a login would fix.
///
/// Beside the message rather than inside it: `k8s::client::explain` writes one
/// wording for both surfaces, and "press L" is true of the dashboard and
/// nonsense on the command line. Returning lines rather than pushing them keeps
/// the two places the hint can appear — a pane that never loaded and a refresh
/// that failed over good rows — from wording it twice.
fn hint_lines(hint: Option<&str>, theme: Theme) -> Vec<Line<'static>> {
    hint.into_iter()
        .map(|text| Line::styled(text.to_owned(), theme.severity(Severity::Warn)))
        .collect()
}

/// Draw whatever the node pane currently knows.
///
/// `selected` highlights a row — `None` when the pane does not currently
/// hold keyboard focus, so the highlight disappears the moment `Tab` moves
/// it back to the sidebar. `order` and `direction` are the pane's own
/// ordering, `s`/`S` in [`super::App`] rather than a request — the note they
/// produce is silent on the default order, exactly as it is under the CLI
/// table, and it disappears along with the rest of the pane's chrome when
/// there are no rows to be sorted. `filter` is the `/` query, empty when no
/// filter is active — every footnote above still reads off the full `rows`,
/// since what a listing's ordering could and could not rank is a fact about
/// the whole pane rather than about whatever it is narrowed to right now;
/// only which rows are actually drawn, through
/// [`crate::fuzzy::rank`], changes with it.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw(
    frame: &mut Frame,
    area: Rect,
    state: &NodesState,
    selected: Option<usize>,
    order: Order,
    direction: Direction,
    filter: &str,
    login_hint: Option<&str>,
    theme: Theme,
) {
    let lines: Vec<Line> = match state {
        NodesState::Loading => vec![Line::styled("Loading nodes…", theme.dim())],
        NodesState::Error(message) => {
            // Split on newlines for the reason `usage_note` below is: every
            // message from `k8s::client::explain` is a diagnosis and then a
            // sentence of advice, and `ratatui` draws an embedded `\n` as one
            // unbroken line — which put the half that says what to do next off
            // the right-hand edge of the pane.
            let mut lines: Vec<Line> = message
                .lines()
                .map(|line| Line::styled(line.to_owned(), theme.severity(Severity::Critical)))
                .collect();
            lines.extend(hint_lines(login_hint, theme));
            lines
        }
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
                let mut wrapped = error.lines();
                if let Some(first) = wrapped.next() {
                    lines.push(Line::styled(
                        format!("Last refresh failed: {first}"),
                        theme.severity(Severity::Warn),
                    ));
                }
                lines.extend(
                    wrapped
                        .map(|line| Line::styled(line.to_owned(), theme.severity(Severity::Warn))),
                );
                lines.extend(hint_lines(login_hint, theme));
            }
            if !filter.is_empty() {
                lines.push(Line::styled(format!("Filter: \"{filter}\""), theme.dim()));
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
            // The case where that line on its own misleads: an ordering that
            // ranked nothing at all describes a listing the alphabet
            // arranged. `Missing::requests` stays `false` deliberately — the
            // CLI's `requests_unavailable` footnote has nowhere to live in
            // this pane yet, so the booked orderings never claim to be
            // explained by a note that was never printed.
            let missing = Missing {
                requests: false,
                usage: usage_missing_explained(rows, usage_note.as_deref()),
            };
            if let Some(note) = order::unranked_note(
                order,
                cause(order, missing),
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
                    format!("No nodes match \"{filter}\"."),
                    theme.dim(),
                ));
            } else {
                lines.extend(
                    visible
                        .into_iter()
                        .enumerate()
                        .map(|(index, row)| node_line(row, Some(index) == selected, theme)),
                );
            }
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
            usage_stale: false,
            pods: share("12", "58"),
            age: "3d".to_owned(),
            created_at: None,
            internal_ip: "-".to_owned(),
            external_ip: "-".to_owned(),
            os_image: "-".to_owned(),
            kernel_version: "-".to_owned(),
            container_runtime: "-".to_owned(),
            devices: BTreeMap::new(),
            ephemeral_storage: Capacity::default(),
            hugepages: BTreeMap::new(),
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
        render_filtered(state, order, direction, "")
    }

    fn render_filtered(
        state: &NodesState,
        order: Order,
        direction: Direction,
        filter: &str,
    ) -> String {
        render_with_hint(state, order, direction, filter, None)
    }

    /// The full-fat renderer the three above narrow: `login_hint` is what
    /// `App::login_hint` would have handed in.
    fn render_with_hint(
        state: &NodesState,
        order: Order,
        direction: Direction,
        filter: &str,
        login_hint: Option<&str>,
    ) -> String {
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw(
                    frame,
                    area,
                    state,
                    None,
                    order,
                    direction,
                    filter,
                    login_hint,
                    Theme::dark(),
                );
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
    fn a_two_sentence_failure_is_drawn_as_two_lines() {
        // Every message from `k8s::client::explain` diagnoses and then advises,
        // and `ratatui` draws an embedded newline as one unbroken line — which
        // pushed the advice off the right-hand edge of the pane.
        let rendered = render(&NodesState::Error(
            "prod rejected your credentials.\nRefresh them and try again.".to_owned(),
        ));

        assert!(
            rendered.contains("prod rejected your credentials."),
            "{rendered}"
        );
        assert!(
            rendered.contains("Refresh them and try again."),
            "{rendered}"
        );
    }

    #[test]
    fn the_login_offer_is_drawn_under_a_failure_that_never_loaded() {
        let rendered = render_with_hint(
            &NodesState::Error("prod rejected your credentials.".to_owned()),
            Order::default(),
            Direction::default(),
            "",
            Some("Press L to log in to AWS and try again."),
        );

        assert!(rendered.contains("Press L to log in"), "{rendered}");
    }

    #[test]
    fn the_login_offer_is_drawn_under_a_failed_refresh_over_good_rows() {
        // The other place the pane can be showing a credential failure, and
        // the more likely one: a session that died while the dashboard was
        // open still has yesterday's rows on screen.
        let state = NodesState::Loaded {
            rows: vec![node("worker-1")],
            usage_note: None,
            refresh_error: Some("prod rejected your credentials.".to_owned()),
        };

        let rendered = render_with_hint(
            &state,
            Order::default(),
            Direction::default(),
            "",
            Some("Press L to log in to AWS and try again."),
        );

        assert!(rendered.contains("Last refresh failed"), "{rendered}");
        assert!(rendered.contains("Press L to log in"), "{rendered}");
        assert!(rendered.contains("worker-1"), "{rendered}");
    }

    #[test]
    fn no_login_offer_is_drawn_when_there_is_nothing_to_offer() {
        let rendered = render(&NodesState::Error("could not reach prod.".to_owned()));

        assert!(!rendered.contains("Press L"), "{rendered}");
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
                        "",
                        None,
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
                    "",
                    None,
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
    fn a_pane_ordering_that_ranked_and_distinguished_something_says_nothing_extra() {
        // Two rows with different `cpu_used` figures: `--sort cpu` (`s` in the
        // pane) both ranks and rearranges them, so the diagnosis has nothing
        // to add. A single row would not prove this — see the tests below.
        let busy = NodeRow {
            cpu_used: share("100m", "4"),
            ..node("worker-2")
        };
        let rendered = render_ordered(
            &loaded(vec![node("worker-1"), busy]),
            Order::Cpu,
            Direction::Natural,
        );
        assert!(!rendered.contains("Nothing here"), "{rendered}");
        assert!(!rendered.contains("ranks the same"), "{rendered}");
    }

    #[test]
    fn a_pane_with_one_row_never_calls_its_own_ordering_useful() {
        // `node("worker-1")` carries a real `cpu_used` figure, so `ranks_any`
        // says yes — but a pane with one row can never be *rearranged* by
        // anything, so the diagnosis fires anyway: sorting it was a no-op.
        let rendered = render_ordered(
            &loaded(vec![node("worker-1")]),
            Order::Cpu,
            Direction::Natural,
        );
        assert!(
            rendered.contains(
                "Every row here ranks the same under cpu, so sorting by it \
                                changed nothing."
            ),
            "{rendered}"
        );
    }

    #[test]
    fn a_pane_ordering_that_ranks_two_tied_rows_says_so() {
        // Two rows with the *same* `cpu_used` figure: `ranks_any` says yes for
        // both, but sorting between them changes nothing, and the pane says
        // so exactly as the CLI table does.
        let rendered = render_ordered(
            &loaded(vec![node("worker-1"), node("worker-2")]),
            Order::Cpu,
            Direction::Natural,
        );
        assert!(
            rendered.contains(
                "Every row here ranks the same under cpu, so sorting by it \
                                changed nothing."
            ),
            "{rendered}"
        );
    }

    #[test]
    fn a_pane_ordering_that_ranked_nothing_says_so() {
        let mut unsampled = node("worker-1");
        unsampled.cpu_used = Share::default();
        let state = NodesState::Loaded {
            rows: vec![unsampled],
            usage_note: None,
            refresh_error: None,
        };

        let rendered = render_ordered(&state, Order::Cpu, Direction::Natural);

        assert!(
            rendered.contains("Nothing here has cpu to sort by."),
            "{rendered}"
        );
    }

    #[test]
    fn an_unsampled_pane_ordering_points_at_the_usage_note_above_it() {
        // The pane's own usage note already explains why `CPU`/`MEM` are
        // blank, so the diagnosis points at it instead of repeating itself.
        // Both columns unsampled, not just `cpu_used`: `shows_usage` is `any`
        // over the two, so a row with a memory figure still counts as shown
        // and this fixture would otherwise land on `Cause::Unexplained`.
        let mut unsampled = node("worker-1");
        unsampled.cpu_used = Share::default();
        unsampled.memory_used = Share::default();
        let state = NodesState::Loaded {
            rows: vec![unsampled],
            usage_note: Some("metrics-server has not sampled anything here yet.".to_owned()),
            refresh_error: None,
        };

        let rendered = render_ordered(&state, Order::Cpu, Direction::Natural);

        assert!(
            rendered.contains("Nothing here has cpu to sort by, for the reason above."),
            "{rendered}"
        );
    }

    #[test]
    fn a_booked_ordering_never_points_at_the_usage_note() {
        // Unlike the CLI table, this pane has no footnote explaining a
        // failed pod listing, so `cpu-requested`/`memory-requested`/`pods`
        // must never claim one exists — even when a usage note happens to be
        // on screen for an unrelated reason.
        let mut unbooked = node("worker-1");
        unbooked.cpu_requested = Share::default();
        let state = NodesState::Loaded {
            rows: vec![unbooked],
            usage_note: Some("metrics-server has not sampled anything here yet.".to_owned()),
            refresh_error: None,
        };

        let rendered = render_ordered(&state, Order::CpuRequested, Direction::Natural);

        assert!(
            rendered.contains("Nothing here has cpu-requested to sort by.")
                && !rendered.contains("for the reason above"),
            "{rendered}"
        );
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
                    "",
                    None,
                    Theme::dark(),
                );
            })
            .unwrap();
    }

    #[test]
    fn an_empty_filter_renders_exactly_as_no_filter_does() {
        let state = loaded(vec![node("worker-1"), node("worker-2")]);
        assert_eq!(
            render_filtered(&state, Order::default(), Direction::default(), ""),
            render(&state)
        );
    }

    #[test]
    fn a_filter_narrows_the_rows_shown() {
        let state = loaded(vec![node("worker-1"), node("worker-2")]);
        let rendered = render_filtered(&state, Order::default(), Direction::default(), "worker-2");

        assert!(rendered.contains("worker-2"), "{rendered}");
        assert!(!rendered.contains("worker-1"), "{rendered}");
    }

    #[test]
    fn a_filter_with_no_match_says_so_rather_than_this_cluster_has_no_nodes() {
        let state = loaded(vec![node("worker-1")]);
        let rendered = render_filtered(&state, Order::default(), Direction::default(), "nope");

        assert!(rendered.contains("No nodes match \"nope\"."), "{rendered}");
        assert!(
            !rendered.contains("This cluster has no nodes"),
            "{rendered}"
        );
    }

    #[test]
    fn an_active_filter_names_itself_above_the_rows() {
        let state = loaded(vec![node("worker-1")]);
        let rendered = render_filtered(&state, Order::default(), Direction::default(), "work");

        assert!(rendered.contains("Filter: \"work\""), "{rendered}");
    }

    #[test]
    fn no_filter_line_is_shown_when_the_filter_is_empty() {
        let rendered = render(&loaded(vec![node("worker-1")]));
        assert!(!rendered.contains("Filter:"), "{rendered}");
    }
}
