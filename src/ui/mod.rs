//! The interactive dashboard.
//!
//! State and input handling live in [`App`], deliberately free of any terminal
//! I/O, so navigation can be tested by feeding it key events. Only [`run`]
//! touches the real terminal.

use std::str::FromStr;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::cluster::ClusterView;
use crate::commands::nodes::NodesFetch;
use crate::k8s::page::{Budget, ParseError};
use crate::theme::Theme;

mod nodes;

use nodes::NodesState;

/// How long to wait for input before waking up to redraw. Short enough that
/// live data will feel immediate once it exists, long enough to stay at
/// effectively zero CPU while idle.
const TICK: Duration = Duration::from_millis(250);

/// How often the dashboard automatically starts a new node fetch, on top of
/// pressing `r` to refresh on demand.
///
/// Delegates entirely to [`Budget`]'s grammar and round trip — `30s`, `500ms`,
/// `2m`, a bare number of seconds — because a second parser for the same
/// durations would only be a second place for it to drift from `--timeout`'s.
/// The number means something different here, though: `0` turns automatic
/// refresh off rather than "wait forever" for one request, so this stays its
/// own type rather than reusing `Budget` at the call site, where a field
/// named `refresh: Budget` would read as a request timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshInterval(Budget);

impl RefreshInterval {
    /// Refresh every `duration`.
    #[must_use]
    pub fn every(duration: Duration) -> Self {
        Self(Budget::of(duration))
    }

    /// Never refresh automatically; `r` still works.
    #[must_use]
    pub fn never() -> Self {
        Self(Budget::unlimited())
    }

    /// How long to wait between automatic refreshes, or `None` for never.
    #[must_use]
    pub fn interval(self) -> Option<Duration> {
        self.0.limit()
    }
}

impl Default for RefreshInterval {
    /// Fifteen seconds: often enough that a pane feels alive, rarely enough
    /// that an idle dashboard is not a standing drain on the API server.
    fn default() -> Self {
        Self::every(Duration::from_secs(15))
    }
}

impl FromStr for RefreshInterval {
    type Err = ParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        Budget::from_str(input).map(Self)
    }
}

impl std::fmt::Display for RefreshInterval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// What the event loop should do after handling an input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Stay in the loop.
    Continue,
    /// Tear down and exit.
    Quit,
}

/// Dashboard state.
#[derive(Debug, Clone)]
pub struct App {
    clusters: Vec<ClusterView>,
    selected: usize,
    theme: Theme,
    nodes: NodesState,
}

impl App {
    /// Create an app over the clusters found in the kubeconfig, starting with
    /// the active one selected and its node pane loading.
    #[must_use]
    pub fn new(clusters: Vec<ClusterView>) -> Self {
        let selected = clusters.iter().position(|c| c.is_current).unwrap_or(0);
        Self {
            clusters,
            selected,
            theme: Theme::dark(),
            nodes: NodesState::default(),
        }
    }

    /// The clusters shown in the sidebar.
    #[must_use]
    pub fn clusters(&self) -> &[ClusterView] {
        &self.clusters
    }

    /// Index of the highlighted row.
    #[must_use]
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// The highlighted cluster, if there is one.
    #[must_use]
    pub fn selected_cluster(&self) -> Option<&ClusterView> {
        self.clusters.get(self.selected)
    }

    /// What the node pane is showing.
    #[must_use]
    pub fn nodes(&self) -> &NodesState {
        &self.nodes
    }

    /// Apply the outcome of a node fetch.
    ///
    /// One of the two state transitions the background channel can cause,
    /// kept beside [`on_key`](Self::on_key) so both are tested the same way:
    /// build an `App`, call the method, assert what changed. A failure after
    /// an earlier fetch had already loaded keeps the last good rows on
    /// screen rather than blanking them — background refresh means a
    /// transient failure is no longer the *first* answer a pane can get, and
    /// the pane should not read as "the cluster lost every node" over one
    /// missed poll.
    pub fn apply_nodes(&mut self, result: Result<NodesFetch, String>) {
        self.nodes = match (result, std::mem::take(&mut self.nodes)) {
            (Ok(fetch), _) => NodesState::Loaded {
                rows: fetch.rows,
                usage_note: fetch.usage_note,
                refresh_error: None,
            },
            (
                Err(message),
                NodesState::Loaded {
                    rows, usage_note, ..
                },
            ) => NodesState::Loaded {
                rows,
                usage_note,
                refresh_error: Some(message),
            },
            (Err(message), _) => NodesState::Error(message),
        };
    }

    /// Reset the node pane to `Loading`.
    ///
    /// Called before a fetch starts for a cluster the pane has not shown
    /// data for yet — selecting a different cluster in the sidebar — so the
    /// pane does not keep displaying the previous cluster's rows while a
    /// different one's request is in flight, which would read as the new
    /// cluster's data rather than stale leftovers.
    pub fn start_loading_nodes(&mut self) {
        self.nodes = NodesState::Loading;
    }

    /// Highlight the cluster with this context name.
    ///
    /// Returns `false` when no such cluster is loaded, leaving the selection
    /// untouched.
    pub fn select_context(&mut self, context_name: &str) -> bool {
        match self
            .clusters
            .iter()
            .position(|c| c.context_name == context_name)
        {
            Some(index) => {
                self.selected = index;
                true
            }
            None => false,
        }
    }

    /// Move the highlight down, wrapping at the end.
    pub fn select_next(&mut self) {
        if self.clusters.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.clusters.len();
    }

    /// Move the highlight up, wrapping at the start.
    pub fn select_previous(&mut self) {
        if self.clusters.is_empty() {
            return;
        }
        self.selected = self
            .selected
            .checked_sub(1)
            .unwrap_or(self.clusters.len() - 1);
    }

    /// Handle a key press.
    ///
    /// Supports both arrow keys and vim-style `j`/`k`, because the people who
    /// live in this kind of tool expect the latter.
    pub fn on_key(&mut self, key: KeyEvent) -> Flow {
        // Key *release* events arrive on Windows and modern terminals; acting on
        // both would move the selection twice per press.
        if key.kind == KeyEventKind::Release {
            return Flow::Continue;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            return Flow::Quit;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Flow::Quit,
            KeyCode::Char('j') | KeyCode::Down => self.select_next(),
            KeyCode::Char('k') | KeyCode::Up => self.select_previous(),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.clusters.len().saturating_sub(1),
            _ => {}
        }
        Flow::Continue
    }
}

/// Run the dashboard against the real terminal.
///
/// Terminal setup and teardown are handled by `ratatui`, which installs a panic
/// hook so a crash cannot leave the user staring at a wedged shell.
///
/// `nodes_rx` is the background fetch `main` already started for the selected
/// cluster before the terminal took over, if there is one, so the first frame
/// never waits on it. `spawn_nodes` is how every fetch after that one is
/// started — on `r`, on the refresh interval, and when the sidebar selects a
/// different cluster — built by the caller over the config, kubeconfig paths,
/// and request budget the CLI itself uses (see
/// [`commands::nodes::spawn_gather`](crate::commands::nodes::spawn_gather)).
/// This function never awaits a fetch: each iteration only polls for a result
/// that has already arrived, which is what keeps a hung request from blocking
/// a keypress.
pub fn run<F>(
    app: App,
    nodes_rx: Option<mpsc::Receiver<Result<NodesFetch, String>>>,
    spawn_nodes: F,
    refresh: RefreshInterval,
) -> Result<()>
where
    F: Fn(&str) -> mpsc::Receiver<Result<NodesFetch, String>>,
{
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, app, nodes_rx, spawn_nodes, refresh);
    ratatui::restore();
    result
}

fn event_loop<B, F>(
    terminal: &mut Terminal<B>,
    mut app: App,
    mut nodes_rx: Option<mpsc::Receiver<Result<NodesFetch, String>>>,
    spawn_nodes: F,
    refresh: RefreshInterval,
) -> Result<()>
where
    B: ratatui::backend::Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
    F: Fn(&str) -> mpsc::Receiver<Result<NodesFetch, String>>,
{
    // What the pane's rows currently belong to, so a change is detectable
    // without the fetch itself carrying its own request back to compare —
    // `App` only ever knows the cluster it is showing *now*.
    let mut selected_context = app.selected_cluster().map(|c| c.context_name.clone());
    let mut next_refresh = schedule(refresh);

    loop {
        // Non-blocking: a fetch that has not finished yet leaves the pane
        // exactly as it was, and one that finished while the user was
        // pressing keys is picked up on the very next frame rather than
        // waiting for a quiet moment.
        if let Some(rx) = &nodes_rx
            && let Ok(result) = rx.try_recv()
        {
            app.apply_nodes(result);
        }

        terminal.draw(|frame| draw(frame, &app))?;

        if next_refresh.is_some_and(|at| Instant::now() >= at) {
            refetch(&spawn_nodes, &mut nodes_rx, selected_context.as_deref());
            next_refresh = schedule(refresh);
        }

        if !event::poll(TICK)? {
            continue;
        }

        let Event::Key(key) = event::read()? else {
            continue;
        };

        if app.on_key(key) == Flow::Quit {
            return Ok(());
        }

        if is_refresh_key(key) {
            refetch(&spawn_nodes, &mut nodes_rx, selected_context.as_deref());
            next_refresh = schedule(refresh);
        }

        // A selection change is a fetch trigger in its own right, and an
        // immediate one: waiting for the interval would leave the pane
        // showing the *previous* cluster's rows under the newly selected
        // cluster's name for however long that takes.
        let now_selected = app.selected_cluster().map(|c| c.context_name.clone());
        if now_selected != selected_context {
            selected_context = now_selected;
            app.start_loading_nodes();
            refetch(&spawn_nodes, &mut nodes_rx, selected_context.as_deref());
            next_refresh = schedule(refresh);
        }
    }
}

/// Start a fetch for whichever cluster is selected, replacing whatever was
/// in flight. A no-op when nothing is selected — an empty kubeconfig has no
/// cluster to fetch.
fn refetch<F>(
    spawn_nodes: &F,
    nodes_rx: &mut Option<mpsc::Receiver<Result<NodesFetch, String>>>,
    selected_context: Option<&str>,
) where
    F: Fn(&str) -> mpsc::Receiver<Result<NodesFetch, String>>,
{
    if let Some(context) = selected_context {
        *nodes_rx = Some(spawn_nodes(context));
    }
}

/// When the next automatic refresh is due, or never.
fn schedule(refresh: RefreshInterval) -> Option<Instant> {
    refresh.interval().map(|interval| Instant::now() + interval)
}

/// Whether this key asks for a refresh right now, independent of the
/// interval. Release events are excluded for the same reason
/// [`App::on_key`] excludes them: they would otherwise fire a second fetch
/// per press on platforms that report both halves of a keystroke.
fn is_refresh_key(key: KeyEvent) -> bool {
    key.kind != KeyEventKind::Release && key.code == KeyCode::Char('r')
}

/// Draw one frame.
pub fn draw(frame: &mut Frame, app: &App) {
    let theme = app.theme;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Min(0),    // body
            Constraint::Length(1), // footer
        ])
        .split(frame.area());

    draw_header(frame, chunks[0], app);
    draw_body(frame, chunks[1], app);
    draw_footer(frame, chunks[2], theme);
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme;
    let cluster = app
        .selected_cluster()
        .map_or_else(|| "no cluster".to_owned(), ClusterView::label);

    let line = Line::from(vec![
        Span::styled(" eks ", theme.heading()),
        Span::styled("│ ", theme.dim()),
        Span::styled(cluster, theme.body().bold()),
    ]);

    frame.render_widget(Paragraph::new(line), area);
}

fn draw_body(frame: &mut Frame, area: Rect, app: &App) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(32), Constraint::Min(0)])
        .split(area);

    draw_cluster_list(frame, columns[0], app);
    draw_detail(frame, columns[1], app);
}

fn draw_cluster_list(frame: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme;

    let items: Vec<ListItem> = app
        .clusters()
        .iter()
        .map(|cluster| {
            let marker = if cluster.is_current { "● " } else { "  " };
            ListItem::new(Line::from(vec![
                Span::styled(marker, theme.severity(crate::theme::Severity::Ok)),
                Span::styled(cluster.display_name.clone(), theme.body()),
                Span::raw(" "),
                Span::styled(cluster.region.clone().unwrap_or_default(), theme.dim()),
            ]))
        })
        .collect();

    let block = Block::bordered()
        .title(" Clusters ")
        .border_style(theme.pane_border(true))
        .title_style(theme.heading());

    let mut state = ListState::default().with_selected(Some(app.selected_index()));
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .highlight_style(theme.selected()),
        area,
        &mut state,
    );
}

fn draw_detail(frame: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme;
    let block = Block::bordered()
        .title(" Overview ")
        .border_style(theme.pane_border(false))
        .title_style(theme.heading());

    let Some(cluster) = app.selected_cluster() else {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "No clusters in your kubeconfig. Run `aws eks update-kubeconfig --name <cluster>`.",
                theme.dim(),
            ))
            .block(block)
            .wrap(Wrap { trim: true }),
            area,
        );
        return;
    };

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut summary = vec![
        detail_row("Context", &cluster.context_name, theme),
        detail_row("Namespace", &cluster.namespace, theme),
    ];
    if let Some(region) = &cluster.region {
        summary.push(detail_row("Region", region, theme));
    }
    if let Some(account) = &cluster.account_id {
        summary.push(detail_row("Account", account, theme));
    }
    summary.push(Line::raw(""));
    let summary_height = u16::try_from(summary.len()).unwrap_or(u16::MAX);

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(summary_height), Constraint::Min(0)])
        .split(inner);

    frame.render_widget(Paragraph::new(summary), sections[0]);
    nodes::draw(frame, sections[1], app.nodes(), theme);
}

fn detail_row<'a>(label: &'a str, value: &'a str, theme: Theme) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{label:<10}"), theme.dim()),
        Span::styled(value, theme.body()),
    ])
}

fn draw_footer(frame: &mut Frame, area: Rect, theme: Theme) {
    let hints = [
        ("j/k", "move"),
        ("enter", "open"),
        ("r", "refresh"),
        ("q", "quit"),
    ];

    let mut spans = vec![Span::raw(" ")];
    for (key, action) in hints {
        spans.push(Span::styled(key, theme.heading()));
        spans.push(Span::styled(format!(" {action}   "), theme.dim()));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn cluster(name: &str, is_current: bool) -> ClusterView {
        ClusterView {
            context_name: format!("arn:aws:eks:us-east-1:1234:cluster/{name}"),
            display_name: name.to_owned(),
            region: Some("us-east-1".to_owned()),
            account_id: Some("1234".to_owned()),
            namespace: "default".to_owned(),
            is_current,
        }
    }

    fn app() -> App {
        App::new(vec![
            cluster("alpha", false),
            cluster("beta", true),
            cluster("gamma", false),
        ])
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn selection_starts_on_the_active_cluster() {
        assert_eq!(app().selected_cluster().unwrap().display_name, "beta");
    }

    #[test]
    fn selection_starts_at_the_top_when_nothing_is_active() {
        let app = App::new(vec![cluster("alpha", false), cluster("beta", false)]);
        assert_eq!(app.selected_index(), 0);
    }

    #[test]
    fn select_context_targets_a_named_cluster() {
        let mut app = app();

        assert!(app.select_context("arn:aws:eks:us-east-1:1234:cluster/gamma"));
        assert_eq!(app.selected_cluster().unwrap().display_name, "gamma");

        assert!(!app.select_context("nope"));
        assert_eq!(
            app.selected_cluster().unwrap().display_name,
            "gamma",
            "a failed lookup must not move the selection"
        );
    }

    #[test]
    fn j_and_k_move_the_selection() {
        let mut app = app();

        app.on_key(press(KeyCode::Char('j')));
        assert_eq!(app.selected_cluster().unwrap().display_name, "gamma");

        app.on_key(press(KeyCode::Char('k')));
        assert_eq!(app.selected_cluster().unwrap().display_name, "beta");
    }

    #[test]
    fn arrow_keys_match_vim_keys() {
        let mut arrows = app();
        let mut vim = app();

        arrows.on_key(press(KeyCode::Down));
        vim.on_key(press(KeyCode::Char('j')));

        assert_eq!(arrows.selected_index(), vim.selected_index());
    }

    #[test]
    fn selection_wraps_at_both_ends() {
        let mut app = app();

        app.on_key(press(KeyCode::Home));
        app.on_key(press(KeyCode::Char('k')));
        assert_eq!(app.selected_cluster().unwrap().display_name, "gamma");

        app.on_key(press(KeyCode::Char('j')));
        assert_eq!(app.selected_cluster().unwrap().display_name, "alpha");
    }

    #[test]
    fn home_and_end_jump_to_the_edges() {
        let mut app = app();

        app.on_key(press(KeyCode::End));
        assert_eq!(app.selected_index(), 2);

        app.on_key(press(KeyCode::Home));
        assert_eq!(app.selected_index(), 0);
    }

    #[test]
    fn q_esc_and_ctrl_c_quit() {
        assert_eq!(app().on_key(press(KeyCode::Char('q'))), Flow::Quit);
        assert_eq!(app().on_key(press(KeyCode::Esc)), Flow::Quit);
        assert_eq!(
            app().on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Flow::Quit
        );
    }

    #[test]
    fn key_releases_are_ignored() {
        let mut app = app();
        let mut release = press(KeyCode::Char('j'));
        release.kind = KeyEventKind::Release;

        app.on_key(release);

        assert_eq!(app.selected_index(), 1, "release events must not navigate");
    }

    #[test]
    fn navigating_an_empty_cluster_list_is_harmless() {
        let mut app = App::new(Vec::new());

        assert_eq!(app.on_key(press(KeyCode::Char('j'))), Flow::Continue);
        assert_eq!(app.on_key(press(KeyCode::Char('k'))), Flow::Continue);
        assert_eq!(app.on_key(press(KeyCode::End)), Flow::Continue);
        assert!(app.selected_cluster().is_none());
    }

    #[test]
    fn a_new_app_starts_loading_its_nodes() {
        assert_eq!(app().nodes(), &NodesState::Loading);
    }

    #[test]
    fn apply_nodes_moves_a_success_into_the_loaded_state() {
        let mut app = app();

        app.apply_nodes(Ok(NodesFetch::default()));

        assert_eq!(
            app.nodes(),
            &NodesState::Loaded {
                rows: Vec::new(),
                usage_note: None,
                refresh_error: None,
            }
        );
    }

    #[test]
    fn apply_nodes_moves_a_failure_into_the_error_state() {
        let mut app = app();

        app.apply_nodes(Err("could not list nodes".to_owned()));

        assert_eq!(
            app.nodes(),
            &NodesState::Error("could not list nodes".to_owned())
        );
    }

    #[test]
    fn a_failed_refresh_after_a_loaded_pane_keeps_its_rows() {
        // Background refresh means a failure is no longer necessarily the
        // *first* answer a pane gets: one bad poll after a good one must not
        // blank a working dashboard back to an error screen.
        let mut app = app();
        app.apply_nodes(Ok(NodesFetch::default()));

        app.apply_nodes(Err("could not list nodes: nope".to_owned()));

        assert_eq!(
            app.nodes(),
            &NodesState::Loaded {
                rows: Vec::new(),
                usage_note: None,
                refresh_error: Some("could not list nodes: nope".to_owned()),
            }
        );
    }

    #[test]
    fn a_successful_refresh_clears_an_earlier_refresh_failure() {
        let mut app = app();
        app.apply_nodes(Ok(NodesFetch::default()));
        app.apply_nodes(Err("could not list nodes: nope".to_owned()));

        app.apply_nodes(Ok(NodesFetch::default()));

        assert_eq!(
            app.nodes(),
            &NodesState::Loaded {
                rows: Vec::new(),
                usage_note: None,
                refresh_error: None,
            }
        );
    }

    #[test]
    fn start_loading_nodes_resets_a_loaded_pane_to_loading() {
        let mut app = app();
        app.apply_nodes(Ok(NodesFetch::default()));

        app.start_loading_nodes();

        assert_eq!(app.nodes(), &NodesState::Loading);
    }

    #[test]
    fn a_frame_renders_the_selected_cluster() {
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        let app = app();

        terminal.draw(|frame| draw(frame, &app)).unwrap();

        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Clusters"), "{rendered}");
        assert!(rendered.contains("beta"), "{rendered}");
        assert!(rendered.contains("us-east-1"), "{rendered}");
        assert!(rendered.contains("quit"), "{rendered}");
    }

    #[test]
    fn first_paint_shows_loading_before_any_fetch_completes() {
        // The acceptance criterion, literally: a freshly built `App` has
        // never received a result over the channel, and the very first frame
        // must not be blank while one is in flight.
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        let app = app();

        terminal.draw(|frame| draw(frame, &app)).unwrap();

        assert!(
            terminal.backend().to_string().contains("Loading nodes"),
            "{}",
            terminal.backend().to_string()
        );
    }

    #[test]
    fn rendering_survives_a_tiny_terminal() {
        // Users do resize their terminals to absurd sizes; a panic here would
        // leave the shell in raw mode.
        for (width, height) in [(1, 1), (8, 3), (20, 2), (200, 60)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw(frame, &app())).unwrap();
        }
    }

    #[test]
    fn rendering_an_empty_cluster_list_explains_itself() {
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        let app = App::new(Vec::new());

        terminal.draw(|frame| draw(frame, &app)).unwrap();

        assert!(terminal.backend().to_string().contains("update-kubeconfig"));
    }

    #[test]
    fn r_requests_a_refresh() {
        assert!(is_refresh_key(press(KeyCode::Char('r'))));
        assert!(!is_refresh_key(press(KeyCode::Char('x'))));
    }

    #[test]
    fn a_release_of_r_does_not_request_a_refresh() {
        // The same double-fire release events would cause in `App::on_key`,
        // for the same reason: acting on both halves of one keystroke would
        // start two fetches per press on a platform that reports both.
        let mut release = press(KeyCode::Char('r'));
        release.kind = KeyEventKind::Release;

        assert!(!is_refresh_key(release));
    }

    #[test]
    fn schedule_is_none_when_automatic_refresh_is_off() {
        assert_eq!(schedule(RefreshInterval::never()), None);
    }

    #[test]
    fn schedule_is_due_after_the_configured_interval() {
        let before = Instant::now();
        let at = schedule(RefreshInterval::every(Duration::from_secs(15))).unwrap();

        assert!(at > before);
        assert!(at <= before + Duration::from_secs(15) + Duration::from_millis(50));
    }

    #[test]
    fn refresh_interval_parses_and_prints_the_same_grammar_timeout_does() {
        assert_eq!(
            RefreshInterval::from_str("15s").unwrap(),
            RefreshInterval::every(Duration::from_secs(15))
        );
        assert_eq!(
            RefreshInterval::from_str("0").unwrap(),
            RefreshInterval::never()
        );
        assert_eq!(RefreshInterval::default().to_string(), "15s");
    }
}
