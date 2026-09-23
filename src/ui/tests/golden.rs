//! Golden-file rendering tests: every main view of the dashboard, drawn
//! whole into a `TestBackend` and compared against a snapshot committed under
//! `snapshots/` beside this file.
//!
//! The `contains` assertions in the parent module pin one guarantee each —
//! "the footer offers `L`", "the title names the pod". What none of them can
//! catch is a change that keeps every one of those strings and still moves
//! everything else: a column that lost its padding, a border that now eats a
//! row, a note that pushed the table off the bottom. A snapshot of the whole
//! frame is the only assertion that notices those, and reading its diff is
//! how a reviewer sees a layout change without running a cluster.
//!
//! Most snapshots are text only, because that is what a reviewer can read in
//! a diff. Colour gets its own, fewer, snapshots through [`styled`] — one per
//! theme over the same busy frame — so a widget that stops going through
//! `Theme` shows up without every layout snapshot also churning whenever a
//! palette entry is tuned. See `docs/ARCHITECTURE.md` for the `cargo insta`
//! workflow.

use std::fmt::Write as _;

use ratatui::buffer::{Buffer, Cell};
use ratatui::style::Color;

use super::*;
use crate::k8s::nodes::{Capacity, NodeRow, Pressure, Share};
use crate::k8s::pods::{ContainerRow, EventRow, PodRow, Requests};
use crate::k8s::quantity::Quantity;
use crate::theme::Severity;

/// A terminal a little larger than the 80x24 floor, so every pane has room to
/// show the columns it drops first on a narrow one. The narrow cases are
/// their own snapshots below.
const WIDTH: u16 = 100;
const HEIGHT: u16 = 24;

/// One drawn frame: the buffer, for [`styled`] to read colours from, and the
/// text `TestBackend` itself would print for it.
struct Screen {
    buffer: Buffer,
    text: String,
}

/// Draw `app` into a fresh `width` x `height` terminal.
fn frame(app: &App, width: u16, height: u16) -> Screen {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw(frame, app)).unwrap();
    let backend = terminal.backend();
    Screen {
        buffer: backend.buffer().clone(),
        text: backend.to_string(),
    }
}

/// The frame's text, one terminal row per line, without the quotes
/// `TestBackend` wraps each row in — so the snapshot file reads as the screen
/// did.
///
/// Taken from `TestBackend`'s own `Display` rather than rebuilt from the
/// cells, because that is what knows how wide each glyph is: a two-column
/// symbol leaves a hidden cell behind it, which `TestBackend` leaves out and
/// then lists after the row. That note is kept as it is, since a snapshot
/// that suddenly has one is worth a reviewer's second look.
fn text(screen: &Screen) -> String {
    let mut out = String::new();
    for row in screen.text.lines() {
        let row = row.strip_prefix('"').unwrap_or(row);
        let row = match row.split_once("\" Hidden by") {
            Some((cells, note)) => format!("{}  (hidden by{note})", cells.trim_end()),
            None => row.strip_suffix('"').unwrap_or(row).trim_end().to_owned(),
        };
        out.push_str(&row);
        out.push('\n');
    }
    out
}

/// [`text`], followed by every run of cells whose ink differs from the
/// terminal's default: `row:start-end fg bg modifiers`, one per line, with
/// each colour named by the `theme` field it came from.
///
/// Role names rather than RGB values, for two reasons. A reviewer can read
/// `fg=muted` in a diff and cannot read `Rgb(138, 143, 152)`. And a colour
/// that did not come from `theme` has no name to print, so it shows up in the
/// snapshot as the one raw `Rgb(..)` among roles — which is exactly the
/// "never hardcode a `Color` in a widget" slip `CLAUDE.md` asks us to catch.
///
/// Our own format rather than `Buffer`'s `Debug`, too, so a `ratatui` upgrade
/// that only rewords that output cannot churn every styled snapshot at once.
fn styled(screen: &Screen, theme: Theme) -> String {
    let buffer = &screen.buffer;
    let width = usize::from(buffer.area.width).max(1);
    let mut out = text(screen);
    out.push_str("\n--- styles ---\n");

    let ink = |cell: &Cell| (cell.fg, cell.bg, cell.modifier);
    let plain = ink(&Cell::EMPTY);
    for (y, row) in buffer.content().chunks(width).enumerate() {
        let mut start = 0;
        while start < row.len() {
            let style = ink(&row[start]);
            let end = row[start..]
                .iter()
                .position(|cell| ink(cell) != style)
                .map_or(row.len(), |offset| start + offset);
            if style != plain {
                let (fg, bg, modifier) = style;
                writeln!(
                    out,
                    "{y}:{start}-{} fg={} bg={} mod={modifier:?}",
                    end - 1,
                    role(theme, fg),
                    role(theme, bg),
                )
                .unwrap();
            }
            start = end;
        }
    }
    out
}

/// The names [`role`] can print, in the order it tries them.
const ROLES: [&str; 11] = [
    "default",
    "background",
    "text",
    "muted",
    "accent",
    "border",
    "border_focused",
    "success",
    "warning",
    "danger",
    "selection_bg",
];

/// The name of the `theme` field `colour` is, or its `Debug` spelling if it
/// is none of them.
///
/// `accent` is checked before `border_focused` because both themes give them
/// the same value today; a focused border therefore reads as `accent` here,
/// which is what it looks like on screen as well.
fn role(theme: Theme, colour: Color) -> String {
    // In the same order as `ROLES`; `default` is the terminal's own colour,
    // which every cell a widget never painted is left in.
    let colours = [
        Color::Reset,
        theme.background,
        theme.text,
        theme.muted,
        theme.accent,
        theme.border,
        theme.border_focused,
        theme.success,
        theme.warning,
        theme.danger,
        theme.selection_bg,
    ];
    ROLES
        .iter()
        .zip(colours)
        .find(|(_, candidate)| *candidate == colour)
        .map_or_else(|| format!("{colour:?}"), |(name, _)| (*name).to_owned())
}

fn quantity(text: &str) -> Quantity {
    Quantity::parse(text).unwrap()
}

fn share(amount: &str, allocatable: &str) -> Share {
    Share {
        amount: Some(quantity(amount)),
        allocatable: Some(quantity(allocatable)),
    }
}

fn capacity(allocatable: &str, capacity: &str) -> Capacity {
    Capacity {
        allocatable: Some(quantity(allocatable)),
        capacity: Some(quantity(capacity)),
    }
}

/// A node that has been running a while and is doing real work, so the
/// overview's bars and percentages have something to draw.
fn busy_node(name: &str, cpu: &str, memory: &str) -> NodeRow {
    NodeRow {
        cpu: capacity("3920m", "4"),
        memory: capacity("14Gi", "16Gi"),
        cpu_requested: share("2500m", "3920m"),
        memory_requested: share("9Gi", "14Gi"),
        cpu_used: share(cpu, "3920m"),
        memory_used: share(memory, "14Gi"),
        pods: share("23", "58"),
        age: "12d".to_owned(),
        internal_ip: "10.0.1.17".to_owned(),
        os_image: "Amazon Linux 2023".to_owned(),
        kernel_version: "6.1.109".to_owned(),
        container_runtime: "containerd://1.7.22".to_owned(),
        ..node_row(name)
    }
}

/// Three nodes in three conditions: comfortable, hot, and not ready.
fn fleet() -> Vec<NodeRow> {
    vec![
        busy_node("ip-10-0-1-17.ec2.internal", "900m", "6Gi"),
        NodeRow {
            pressure: Pressure {
                memory: true,
                ..Pressure::default()
            },
            ..busy_node("ip-10-0-2-40.ec2.internal", "3500m", "13Gi")
        },
        NodeRow {
            status: "NotReady".to_owned(),
            severity: Severity::Critical,
            cpu_used: Share::default(),
            memory_used: Share::default(),
            age: "4h".to_owned(),
            ..busy_node("ip-10-0-3-8.ec2.internal", "0", "0")
        },
    ]
}

/// The overview with [`fleet`] loaded and the detail pane focused on its
/// first row — the frame a user sees most.
fn overview() -> App {
    let mut app = app();
    app.apply_nodes(Ok(NodesFetch {
        rows: fleet(),
        usage_note: None,
        requests_note: None,
    }));
    app.toggle_focus();
    app
}

fn busy_pod(name: &str, cpu: &str, memory: &str) -> PodRow {
    PodRow {
        namespace: "payments".to_owned(),
        cpu_used: Some(quantity(cpu)),
        memory_used: Some(quantity(memory)),
        cpu_requested: quantity("250m"),
        memory_requested: quantity("512Mi"),
        cpu_limit: Some(quantity("1")),
        memory_limit: Some(quantity("1Gi")),
        node: "ip-10-0-1-17.ec2.internal".to_owned(),
        ..pod_row(name)
    }
}

/// [`overview`] drilled into its first node, with three pods loaded.
fn node_pods() -> App {
    let mut app = overview();
    app.on_key(press(KeyCode::Enter));
    app.apply_pods(Ok(PodsFetch {
        rows: vec![
            busy_pod("api-7d9f8b6c4-x2kqp", "120m", "300Mi"),
            PodRow {
                status: "CrashLoopBackOff".to_owned(),
                severity: Severity::Critical,
                ready: "0/1".to_owned(),
                restarts: 14,
                restart_age: Some("2m".to_owned()),
                ..busy_pod("worker-5b8c7d9f6-hj4lm", "5m", "40Mi")
            },
            busy_pod("ledger-0", "400m", "900Mi"),
        ],
        selector_note: None,
        usage_note: None,
    }));
    app
}

/// [`node_pods`] drilled into the crash-looping pod, with its containers and
/// events loaded.
fn pod_containers() -> App {
    let mut app = node_pods();
    // Rows are in name order, so the crash-looping `worker-…` is third.
    app.on_key(press(KeyCode::Char('j')));
    app.on_key(press(KeyCode::Char('j')));
    app.on_key(press(KeyCode::Enter));
    app.apply_containers(Ok(ContainersFetch {
        rows: vec![
            ContainerRow {
                init: true,
                state: "Completed".to_owned(),
                ..container_row("migrate")
            },
            ContainerRow {
                ready: false,
                restarts: 14,
                state: "CrashLoopBackOff".to_owned(),
                severity: Severity::Critical,
                requests: Requests {
                    cpu: quantity("250m"),
                    memory: quantity("512Mi"),
                    ..Requests::default()
                },
                ..container_row("worker")
            },
        ],
        ip: "10.0.1.203".to_owned(),
        nominated_node: "-".to_owned(),
        readiness_gates: None,
        events: vec![
            EventRow {
                reason: "BackOff".to_owned(),
                message: "Back-off restarting failed container worker".to_owned(),
                warning: true,
                count: 61,
                last_seen_age: Some("40s".to_owned()),
                last_seen: None,
            },
            EventRow {
                reason: "Pulled".to_owned(),
                message: "Container image \"app:1.0\" already present on machine".to_owned(),
                warning: false,
                count: 15,
                last_seen_age: Some("2m".to_owned()),
                last_seen: None,
            },
        ],
        events_error: None,
        events_empty_note: String::new(),
    }));
    app
}

/// [`pod_containers`] drilled into the crashing container's log, which has
/// printed a few lines and then ended.
fn container_logs() -> App {
    let mut app = pod_containers();
    app.on_key(press(KeyCode::Char('j')));
    app.on_key(press(KeyCode::Enter));
    for line in [
        "2026-09-23T02:14:07Z INFO  starting worker v2.4.1",
        "2026-09-23T02:14:07Z INFO  connecting to queue at amqp://queue.payments:5672",
        "2026-09-23T02:14:12Z ERROR connection refused (os error 111)",
        "2026-09-23T02:14:12Z ERROR giving up after 5 attempts",
    ] {
        app.apply_log_event(LogEvent::Line(line.to_owned()));
    }
    app.apply_log_event(LogEvent::Ended(None));
    app
}

#[test]
fn the_overview_while_the_first_node_listing_is_in_flight() {
    insta::assert_snapshot!(text(&frame(&app(), WIDTH, HEIGHT)));
}

#[test]
fn the_overview_with_a_fleet_loaded() {
    insta::assert_snapshot!(text(&frame(&overview(), WIDTH, HEIGHT)));
}

#[test]
fn the_overview_of_a_cluster_with_no_nodes() {
    let mut app = app();
    app.apply_nodes(Ok(NodesFetch::default()));
    insta::assert_snapshot!(text(&frame(&app, WIDTH, HEIGHT)));
}

#[test]
fn the_overview_when_the_cluster_rejects_the_credentials() {
    let mut app = app();
    app.apply_nodes(Err(refused(
        "beta rejected your credentials (401 Unauthorized). \
         Your AWS session has probably expired.",
    )));
    insta::assert_snapshot!(text(&frame(&app, WIDTH, HEIGHT)));
}

#[test]
fn the_overview_when_the_cluster_cannot_be_reached() {
    let mut app = app();
    app.apply_nodes(Err(failed(
        "could not reach beta's API server within 10s. \
         Check your VPN, or that the cluster's endpoint is reachable from here.",
    )));
    insta::assert_snapshot!(text(&frame(&app, WIDTH, HEIGHT)));
}

#[test]
fn the_overview_with_metrics_server_missing() {
    let mut app = app();
    app.apply_nodes(Ok(NodesFetch {
        rows: fleet()
            .into_iter()
            .map(|row| NodeRow {
                cpu_used: Share::default(),
                memory_used: Share::default(),
                ..row
            })
            .collect(),
        usage_note: Some(
            "usage unavailable: metrics-server is not installed on this cluster".to_owned(),
        ),
        requests_note: None,
    }));
    insta::assert_snapshot!(text(&frame(&app, WIDTH, HEIGHT)));
}

#[test]
fn the_overview_while_a_filter_is_being_typed() {
    let mut app = overview();
    for key in "/2-40".chars() {
        app.on_key(press(KeyCode::Char(key)));
    }
    insta::assert_snapshot!(text(&frame(&app, WIDTH, HEIGHT)));
}

#[test]
fn the_overview_with_a_quit_armed() {
    let mut app = overview();
    app.on_key(press(KeyCode::Char('q')));
    insta::assert_snapshot!(text(&frame(&app, WIDTH, HEIGHT)));
}

#[test]
fn the_dashboard_with_no_clusters_in_the_kubeconfig() {
    insta::assert_snapshot!(text(&frame(&App::new(Vec::new()), WIDTH, HEIGHT)));
}

#[test]
fn the_pods_on_a_node() {
    insta::assert_snapshot!(text(&frame(&node_pods(), WIDTH, HEIGHT)));
}

#[test]
fn the_containers_and_events_of_a_pod() {
    insta::assert_snapshot!(text(&frame(&pod_containers(), WIDTH, HEIGHT)));
}

#[test]
fn the_log_of_a_container_that_has_exited() {
    insta::assert_snapshot!(text(&frame(&container_logs(), WIDTH, HEIGHT)));
}

#[test]
fn the_overview_on_an_80_by_24_terminal() {
    insta::assert_snapshot!(text(&frame(&overview(), 80, 24)));
}

#[test]
fn the_overview_on_a_terminal_too_small_to_hold_it() {
    insta::assert_snapshot!(text(&frame(&overview(), 30, 6)));
}

#[test]
fn a_one_by_one_terminal_draws_its_single_cell_without_panicking() {
    insta::assert_snapshot!(text(&frame(&overview(), 1, 1)));
}

/// Every fixture above, for the tests that sweep all of them.
fn every_view() -> Vec<(&'static str, App)> {
    vec![
        ("loading", app()),
        ("overview", overview()),
        ("node pods", node_pods()),
        ("pod containers", pod_containers()),
        ("container logs", container_logs()),
    ]
}

#[test]
fn the_overview_in_colour() {
    insta::assert_snapshot!(styled(&frame(&overview(), WIDTH, HEIGHT), Theme::dark()));
}

#[test]
fn the_pods_on_a_node_in_colour() {
    insta::assert_snapshot!(styled(&frame(&node_pods(), WIDTH, HEIGHT), Theme::dark()));
}

#[test]
fn the_containers_and_events_of_a_pod_in_colour() {
    insta::assert_snapshot!(styled(
        &frame(&pod_containers(), WIDTH, HEIGHT),
        Theme::dark()
    ));
}

#[test]
fn the_log_of_a_container_in_colour() {
    insta::assert_snapshot!(styled(
        &frame(&container_logs(), WIDTH, HEIGHT),
        Theme::dark()
    ));
}

#[test]
fn every_view_draws_only_in_its_theme_colours() {
    // The coloured snapshots above cover the dark theme by inspection; this
    // covers both, mechanically, so a raw `Color` added to any pane fails a
    // test rather than waiting for somebody to spot an `Rgb(..)` in a diff.
    for (theme_name, theme) in [("dark", Theme::dark()), ("light", Theme::light())] {
        for (name, mut app) in every_view() {
            app.set_theme(theme);
            let rendered = styled(&frame(&app, WIDTH, HEIGHT), theme);
            let (_, styles) = rendered.split_once("--- styles ---").unwrap();
            let strays: Vec<&str> = styles
                .split_whitespace()
                .filter_map(|field| {
                    field
                        .strip_prefix("fg=")
                        .or_else(|| field.strip_prefix("bg="))
                })
                .filter(|colour| !ROLES.contains(colour))
                .collect();
            assert!(
                strays.is_empty(),
                "{name} in the {theme_name} theme drew {strays:?}, which are not in its theme:\n{styles}"
            );
        }
    }
}

#[test]
fn the_light_theme_inks_every_cell_in_the_same_role_as_the_dark_one() {
    // Named by role, the two themes' frames should be indistinguishable: the
    // light theme is a second palette for the same roles, not a second
    // design. A pane that picked a different role under one theme — say, a
    // `match` on `Background` somewhere a `Theme` accessor should have been —
    // would show up here as the one differing run.
    for ((name, dark), (_, mut light)) in every_view().into_iter().zip(every_view()) {
        light.set_theme(Theme::light());
        assert_eq!(
            styled(&frame(&dark, WIDTH, HEIGHT), Theme::dark()),
            styled(&frame(&light, WIDTH, HEIGHT), Theme::light()),
            "{name}"
        );
    }
}

#[test]
fn text_keeps_a_note_for_cells_a_wide_glyph_hides() {
    // No pane draws a two-column glyph today, so no snapshot exercises this
    // path of `text` — but a cluster name in CJK would, and a snapshot that
    // silently lost a column's worth of alignment would be worse than none.
    let mut terminal = Terminal::new(TestBackend::new(6, 1)).unwrap();
    terminal
        .draw(|frame| frame.render_widget(Paragraph::new("日本 x"), frame.area()))
        .unwrap();
    let backend = terminal.backend();
    let screen = Screen {
        buffer: backend.buffer().clone(),
        text: backend.to_string(),
    };

    let rendered = text(&screen);

    assert!(rendered.starts_with("日本 x  (hidden by"), "{rendered}");
    assert_eq!(rendered.lines().count(), 1, "{rendered}");
}
