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
use crate::commands::pods::PodsFetch;
use crate::k8s::page::{Budget, ParseError};
use crate::theme::Theme;

mod nodes;
mod pods;

use nodes::NodesState;
use pods::PodsState;

/// How long to wait for input before waking up to redraw. Short enough that
/// live data will feel immediate once it exists, long enough to stay at
/// effectively zero CPU while idle.
const TICK: Duration = Duration::from_millis(250);

/// Starts a node fetch for the named context. Boxed rather than a type
/// parameter on [`run`] and the event loop it drives: a second pane wanting its own fetch
/// trigger (this change adds one, for pods) would otherwise grow a type
/// parameter on both every time, for a distinction — which closure a
/// function happens to be — nothing outside `main` cares about.
pub type NodesFetcher = Box<dyn Fn(&str) -> mpsc::Receiver<Result<NodesFetch, String>>>;

/// Starts a fetch of the pods on one node of one cluster.
pub type PodsFetcher = Box<dyn Fn(&str, &str) -> mpsc::Receiver<Result<PodsFetch, String>>>;

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

/// Which pane `j`/`k`/`Home`/`End` currently move the highlight in.
///
/// `Tab` toggles between the two; the focused pane draws its border in the
/// theme's focus colour, the same way [`Theme::pane_border`] already did
/// when the sidebar was the only thing that could hold focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    /// The cluster list. `j`/`k` change which cluster is selected.
    #[default]
    Sidebar,
    /// The detail pane. `j`/`k` move a highlight within whatever list it is
    /// currently showing — the node list, or a node's pods.
    Detail,
}

/// What the detail pane is showing, independent of which cluster is
/// selected.
///
/// A drill-down rather than a stack, because there is exactly one level of
/// it today: a node's pods. A pod's containers — the next level the roadmap
/// asks for — is the natural place this grows into a `Vec<View>` instead of
/// gaining a third variant, but building that now would be guessing at a
/// shape one more case cannot justify yet.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum View {
    /// The selected cluster's node list.
    #[default]
    Overview,
    /// The pods placed on one node of the selected cluster.
    NodePods { node: String },
}

/// Dashboard state.
#[derive(Debug, Clone)]
pub struct App {
    clusters: Vec<ClusterView>,
    selected: usize,
    theme: Theme,
    nodes: NodesState,
    pods: PodsState,
    focus: Focus,
    view: View,
    /// The highlighted row within whichever list [`View`] is currently
    /// showing in the detail pane — node rows under [`View::Overview`], pod
    /// rows under [`View::NodePods`]. Reset to `0` on every view change and
    /// every fresh load, so it can never point past the end of a shorter
    /// list that just arrived.
    detail_selected: usize,
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
            pods: PodsState::default(),
            focus: Focus::default(),
            view: View::default(),
            detail_selected: 0,
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

    /// What the pod-drilldown pane is showing. Only meaningful while
    /// [`Self::view`] is [`View::NodePods`]; `Overview` simply does not read
    /// it.
    #[must_use]
    pub fn pods(&self) -> &PodsState {
        &self.pods
    }

    /// Which pane `j`/`k`/`Home`/`End` currently move the highlight in.
    #[must_use]
    pub fn focus(&self) -> Focus {
        self.focus
    }

    /// What the detail pane is currently showing.
    #[must_use]
    pub fn view(&self) -> &View {
        &self.view
    }

    /// The highlighted row within whichever list the detail pane is
    /// currently showing.
    #[must_use]
    pub fn detail_selected(&self) -> usize {
        self.detail_selected
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

    /// Apply the outcome of a fetch for one node's pods.
    ///
    /// Unlike [`Self::apply_nodes`], a failure always overwrites: the
    /// pod-drilldown pane fetches once per node it is asked to show rather
    /// than refreshing in the background, so there is no earlier good
    /// listing for this node worth keeping over a failed one.
    pub fn apply_pods(&mut self, result: Result<PodsFetch, String>) {
        self.pods = match result {
            Ok(fetch) => PodsState::Loaded { rows: fetch.rows },
            Err(message) => PodsState::Error(message),
        };
    }

    /// Toggle which pane `j`/`k`/`Home`/`End` move the highlight in.
    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Sidebar => Focus::Detail,
            Focus::Detail => Focus::Sidebar,
        };
    }

    /// Return the detail pane to the node list, discarding any drill-down
    /// into a node's pods.
    ///
    /// Called both when `Esc` backs out of [`View::NodePods`] and when the
    /// sidebar selects a different cluster: a pods listing for a node in the
    /// *previous* cluster is not an answer for the newly selected one.
    pub fn leave_node_pods(&mut self) {
        self.view = View::Overview;
        self.detail_selected = 0;
        self.pods = PodsState::default();
    }

    /// Drill into the highlighted node's pods, if the detail pane is focused
    /// on the node list and a node is actually highlighted.
    ///
    /// A no-op otherwise — pressing `Enter` with the sidebar focused, or
    /// while a node fetch is still loading and there is nothing to
    /// highlight yet, changes nothing. Starting the pod fetch itself is the
    /// event loop's job, once it sees the view
    /// change this causes; this method only decides *that* it happened.
    pub fn drill_into_pods(&mut self) {
        if self.focus != Focus::Detail {
            return;
        }
        if !matches!(self.view, View::Overview) {
            return;
        }
        let Some(node) = self.nodes.rows().get(self.detail_selected) else {
            return;
        };
        self.view = View::NodePods {
            node: node.name.clone(),
        };
        self.detail_selected = 0;
        self.pods = PodsState::Loading;
    }

    /// `Esc`: back out of a drill-down, or quit if there is nowhere to back
    /// out to.
    fn back_or_quit(&mut self) -> Flow {
        if matches!(self.view, View::NodePods { .. }) {
            self.leave_node_pods();
            Flow::Continue
        } else {
            Flow::Quit
        }
    }

    /// How many rows the detail pane's current view could highlight.
    fn detail_row_count(&self) -> usize {
        match &self.view {
            View::Overview => self.nodes.rows().len(),
            View::NodePods { .. } => self.pods.rows().len(),
        }
    }

    /// Move the detail pane's highlight down, wrapping at the end.
    fn select_next_detail_row(&mut self) {
        let len = self.detail_row_count();
        if len == 0 {
            return;
        }
        self.detail_selected = (self.detail_selected + 1) % len;
    }

    /// Move the detail pane's highlight up, wrapping at the start.
    fn select_previous_detail_row(&mut self) {
        let len = self.detail_row_count();
        if len == 0 {
            return;
        }
        self.detail_selected = self.detail_selected.checked_sub(1).unwrap_or(len - 1);
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
            KeyCode::Char('q') => return Flow::Quit,
            KeyCode::Esc => return self.back_or_quit(),
            KeyCode::Tab => self.toggle_focus(),
            KeyCode::Enter => self.drill_into_pods(),
            KeyCode::Char('j') | KeyCode::Down => match self.focus {
                Focus::Sidebar => self.select_next(),
                Focus::Detail => self.select_next_detail_row(),
            },
            KeyCode::Char('k') | KeyCode::Up => match self.focus {
                Focus::Sidebar => self.select_previous(),
                Focus::Detail => self.select_previous_detail_row(),
            },
            KeyCode::Home => match self.focus {
                Focus::Sidebar => self.selected = 0,
                Focus::Detail => self.detail_selected = 0,
            },
            KeyCode::End => match self.focus {
                Focus::Sidebar => self.selected = self.clusters.len().saturating_sub(1),
                Focus::Detail => {
                    self.detail_selected = self.detail_row_count().saturating_sub(1);
                }
            },
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
/// `spawn_pods` is the same idea for a node's pods, called with the selected
/// cluster's context and the drilled-into node's name whenever the detail
/// pane's view changes to [`View::NodePods`].
/// This function never awaits a fetch: each iteration only polls for a result
/// that has already arrived, which is what keeps a hung request from blocking
/// a keypress.
pub fn run(
    app: App,
    nodes_rx: Option<mpsc::Receiver<Result<NodesFetch, String>>>,
    spawn_nodes: &NodesFetcher,
    spawn_pods: &PodsFetcher,
    refresh: RefreshInterval,
) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(
        &mut terminal,
        app,
        nodes_rx,
        spawn_nodes,
        spawn_pods,
        refresh,
    );
    ratatui::restore();
    result
}

fn event_loop<B>(
    terminal: &mut Terminal<B>,
    mut app: App,
    mut nodes_rx: Option<mpsc::Receiver<Result<NodesFetch, String>>>,
    spawn_nodes: &NodesFetcher,
    spawn_pods: &PodsFetcher,
    refresh: RefreshInterval,
) -> Result<()>
where
    B: ratatui::backend::Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    // What the pane's rows currently belong to, so a change is detectable
    // without the fetch itself carrying its own request back to compare —
    // `App` only ever knows the cluster it is showing *now*.
    let mut selected_context = app.selected_cluster().map(|c| c.context_name.clone());
    let mut next_refresh = schedule(refresh);
    let mut pods_rx: Option<mpsc::Receiver<Result<PodsFetch, String>>> = None;

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
        if let Some(rx) = &pods_rx
            && let Ok(result) = rx.try_recv()
        {
            app.apply_pods(result);
        }

        terminal.draw(|frame| draw(frame, &app))?;

        if next_refresh.is_some_and(|at| Instant::now() >= at) {
            refetch(spawn_nodes, &mut nodes_rx, selected_context.as_deref());
            next_refresh = schedule(refresh);
        }

        if !event::poll(TICK)? {
            continue;
        }

        let Event::Key(key) = event::read()? else {
            continue;
        };

        let view_before = app.view().clone();

        if app.on_key(key) == Flow::Quit {
            return Ok(());
        }

        if is_refresh_key(key) {
            refetch(spawn_nodes, &mut nodes_rx, selected_context.as_deref());
            next_refresh = schedule(refresh);
        }

        // A selection change is a fetch trigger in its own right, and an
        // immediate one: waiting for the interval would leave the pane
        // showing the *previous* cluster's rows under the newly selected
        // cluster's name for however long that takes. It also drops any
        // drill-down into that previous cluster's nodes.
        let now_selected = app.selected_cluster().map(|c| c.context_name.clone());
        if now_selected != selected_context {
            selected_context = now_selected;
            app.start_loading_nodes();
            app.leave_node_pods();
            pods_rx = None;
            refetch(spawn_nodes, &mut nodes_rx, selected_context.as_deref());
            next_refresh = schedule(refresh);
        } else if *app.view() != view_before {
            // Not an `else if` on the selection check above by accident: a
            // cluster change already forces the view back to `Overview`
            // through `leave_node_pods`, so re-deriving the same outcome
            // here would just repeat it.
            match app.view() {
                View::NodePods { node } => {
                    if let Some(context) = selected_context.as_deref() {
                        pods_rx = Some(spawn_pods(context, node));
                    }
                }
                View::Overview => pods_rx = None,
            }
        }
    }
}

/// Start a fetch for whichever cluster is selected, replacing whatever was
/// in flight. A no-op when nothing is selected — an empty kubeconfig has no
/// cluster to fetch.
fn refetch(
    spawn_nodes: &NodesFetcher,
    nodes_rx: &mut Option<mpsc::Receiver<Result<NodesFetch, String>>>,
    selected_context: Option<&str>,
) {
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
        .border_style(theme.pane_border(app.focus() == Focus::Sidebar))
        .title_style(theme.heading());

    // The highlighted cluster stays visible however focus moves — it is
    // "what this whole dashboard is showing", not merely "where a `j`/`k`
    // press would land" — unlike the detail pane's row highlight, which
    // disappears the moment `Tab` moves focus away from it.
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
    // The breadcrumb the roadmap's pod-browsing task asks for: the block's
    // own title, so a drill-down needs no second line of chrome to say
    // where it is.
    let title = match app.view() {
        View::Overview => " Overview ".to_owned(),
        View::NodePods { node } => format!(" Overview › {node} "),
    };
    let block = Block::bordered()
        .title(title)
        .border_style(theme.pane_border(app.focus() == Focus::Detail))
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

    // The row highlight only appears while the detail pane holds focus —
    // with the sidebar focused, `detail_selected` names a row `Enter`
    // cannot reach right now, and showing it anyway would suggest otherwise.
    let highlighted = (app.focus() == Focus::Detail).then_some(app.detail_selected());
    match app.view() {
        View::Overview => nodes::draw(frame, sections[1], app.nodes(), highlighted, theme),
        View::NodePods { .. } => pods::draw(frame, sections[1], app.pods(), highlighted, theme),
    }
}

fn detail_row<'a>(label: &'a str, value: &'a str, theme: Theme) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{label:<10}"), theme.dim()),
        Span::styled(value, theme.body()),
    ])
}

fn draw_footer(frame: &mut Frame, area: Rect, theme: Theme) {
    let hints = [
        ("tab", "switch pane"),
        ("j/k", "move"),
        ("enter", "open"),
        ("esc", "back"),
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

    fn node_row(name: &str) -> crate::k8s::nodes::NodeRow {
        use std::collections::BTreeMap;

        use crate::k8s::nodes::{Capacity, Share};

        crate::k8s::nodes::NodeRow {
            name: name.to_owned(),
            status: "Ready".to_owned(),
            severity: crate::theme::Severity::Ok,
            version: "v1.31".to_owned(),
            cpu: Capacity::default(),
            memory: Capacity::default(),
            cpu_requested: Share::default(),
            memory_requested: Share::default(),
            cpu_used: Share::default(),
            memory_used: Share::default(),
            pods: Share::default(),
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

    /// An app with one node already loaded and the detail pane focused on
    /// it, ready to drill into.
    fn app_with_node() -> App {
        let mut app = app();
        app.apply_nodes(Ok(NodesFetch {
            rows: vec![node_row("worker-1")],
            usage_note: None,
        }));
        app.toggle_focus();
        app
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

    #[test]
    fn tab_toggles_focus_between_sidebar_and_detail() {
        let mut app = app();
        assert_eq!(app.focus(), Focus::Sidebar);

        app.on_key(press(KeyCode::Tab));
        assert_eq!(app.focus(), Focus::Detail);

        app.on_key(press(KeyCode::Tab));
        assert_eq!(app.focus(), Focus::Sidebar);
    }

    #[test]
    fn j_and_k_move_the_detail_highlight_instead_of_the_sidebar_when_focused() {
        let mut app = app_with_node();
        app.apply_nodes(Ok(NodesFetch {
            rows: vec![node_row("worker-1"), node_row("worker-2")],
            usage_note: None,
        }));
        let selected_cluster_before = app.selected_index();

        app.on_key(press(KeyCode::Char('j')));

        assert_eq!(
            app.selected_index(),
            selected_cluster_before,
            "the sidebar must not move while the detail pane has focus"
        );
        assert_eq!(app.detail_selected(), 1);
    }

    #[test]
    fn enter_does_nothing_while_the_sidebar_is_focused() {
        let mut app = app();
        app.apply_nodes(Ok(NodesFetch {
            rows: vec![node_row("worker-1")],
            usage_note: None,
        }));

        app.on_key(press(KeyCode::Enter));

        assert_eq!(app.view(), &View::Overview);
    }

    #[test]
    fn enter_drills_into_the_highlighted_nodes_pods() {
        let mut app = app_with_node();

        app.on_key(press(KeyCode::Enter));

        assert_eq!(
            app.view(),
            &View::NodePods {
                node: "worker-1".to_owned()
            }
        );
        assert_eq!(app.pods(), &PodsState::Loading);
        assert_eq!(
            app.detail_selected(),
            0,
            "drilling in starts with nothing highlighted in the new list"
        );
    }

    #[test]
    fn enter_is_a_no_op_while_the_node_list_is_still_loading() {
        // Nothing to drill into yet — `rows()` is empty on `Loading`, so
        // `detail_selected` cannot name a row.
        let mut app = app();
        app.toggle_focus();

        app.on_key(press(KeyCode::Enter));

        assert_eq!(app.view(), &View::Overview);
    }

    #[test]
    fn esc_backs_out_of_a_drill_down_rather_than_quitting() {
        let mut app = app_with_node();
        app.on_key(press(KeyCode::Enter));
        assert_eq!(app.pods(), &PodsState::Loading);

        let flow = app.on_key(press(KeyCode::Esc));

        assert_eq!(flow, Flow::Continue);
        assert_eq!(app.view(), &View::Overview);
    }

    #[test]
    fn esc_quits_once_there_is_nowhere_left_to_back_out_to() {
        let mut app = app_with_node();
        app.on_key(press(KeyCode::Enter));
        app.on_key(press(KeyCode::Esc));

        assert_eq!(app.on_key(press(KeyCode::Esc)), Flow::Quit);
    }

    #[test]
    fn q_always_quits_even_while_drilled_into_a_node() {
        let mut app = app_with_node();
        app.on_key(press(KeyCode::Enter));

        assert_eq!(app.on_key(press(KeyCode::Char('q'))), Flow::Quit);
    }

    #[test]
    fn leave_node_pods_resets_the_view_and_the_pods_pane() {
        let mut app = app_with_node();
        app.on_key(press(KeyCode::Enter));

        app.leave_node_pods();

        assert_eq!(app.view(), &View::Overview);
        assert_eq!(app.pods(), &PodsState::Loading);
        assert_eq!(app.detail_selected(), 0);
    }

    #[test]
    fn apply_pods_moves_a_success_into_the_loaded_state() {
        let mut app = app();

        app.apply_pods(Ok(PodsFetch::default()));

        assert_eq!(app.pods(), &PodsState::Loaded { rows: Vec::new() });
    }

    #[test]
    fn apply_pods_moves_a_failure_into_the_error_state_even_after_a_success() {
        // Unlike the node pane, the pod pane fetches once per node rather
        // than refreshing in the background, so there is no earlier good
        // listing for *this* node worth keeping over a failed one.
        let mut app = app();
        app.apply_pods(Ok(PodsFetch::default()));

        app.apply_pods(Err("could not list pods".to_owned()));

        assert_eq!(
            app.pods(),
            &PodsState::Error("could not list pods".to_owned())
        );
    }

    #[test]
    fn detail_row_movement_is_harmless_with_nothing_loaded() {
        let mut app = app();
        app.toggle_focus();

        app.on_key(press(KeyCode::Char('j')));
        app.on_key(press(KeyCode::Char('k')));
        app.on_key(press(KeyCode::Home));
        app.on_key(press(KeyCode::End));

        assert_eq!(app.detail_selected(), 0);
    }

    #[test]
    fn home_and_end_bound_the_detail_highlight_too() {
        let mut app = app_with_node();
        app.apply_nodes(Ok(NodesFetch {
            rows: vec![
                node_row("worker-1"),
                node_row("worker-2"),
                node_row("worker-3"),
            ],
            usage_note: None,
        }));

        app.on_key(press(KeyCode::End));
        assert_eq!(app.detail_selected(), 2);

        app.on_key(press(KeyCode::Home));
        assert_eq!(app.detail_selected(), 0);
    }
}
