//! The interactive dashboard.
//!
//! State and input handling live in [`App`], deliberately free of any terminal
//! I/O, so navigation can be tested by feeding it key events. Only [`run`]
//! touches the real terminal.

use std::str::FromStr;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::ValueEnum;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::{execute, terminal};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::cluster::ClusterView;
use crate::commands::nodes::NodesFetch;
use crate::commands::pods::{ContainersFetch, PodsFetch};
use crate::commands::{FetchError, StreamHandle};
use crate::k8s::nodes as k8s_nodes;
use crate::k8s::order::Direction as SortDirection;
use crate::k8s::page::{Budget, ParseError};
use crate::k8s::pods as k8s_pods;
use crate::k8s::pods::LogEvent;
use crate::theme::Theme;

mod containers;
mod logs;
mod nodes;
mod pods;

use containers::ContainersState;
use logs::LogsState;
use nodes::NodesState;
use pods::PodsState;

/// How long to wait for input before waking up to redraw. Short enough that
/// live data will feel immediate once it exists, long enough to stay at
/// effectively zero CPU while idle.
const TICK: Duration = Duration::from_millis(250);

/// How long a second `Esc`/`q` has to land after the first before it counts
/// as a confirming press rather than a fresh arm. Long enough that an
/// intentional double-press doesn't feel twitchy, short enough that an
/// unrelated later press doesn't quit by accident.
const QUIT_CONFIRM_WINDOW: Duration = Duration::from_millis(600);

/// Starts a node fetch for the named context. Boxed rather than a type
/// parameter on [`run`] and the event loop it drives: a second pane wanting its own fetch
/// trigger (this change adds one, for pods) would otherwise grow a type
/// parameter on both every time, for a distinction — which closure a
/// function happens to be — nothing outside `main` cares about.
pub type NodesFetcher = Box<dyn Fn(&str) -> mpsc::Receiver<Result<NodesFetch, FetchError>>>;

/// Log the selected cluster's AWS profile in, blocking until it is done.
///
/// Unlike every fetcher beside it this one runs on *this* thread, and that is
/// the point: `aws sso login` prints a device code and waits for a browser, so
/// it needs the terminal the dashboard is currently holding. [`run`] hands the
/// terminal back before calling it and takes it again afterwards. The `&str` is
/// the selected context's name; the `Err` is already a sentence.
pub type LoginRunner = Box<dyn Fn(&str) -> Result<(), String>>;

/// [`LoginRunner`] with the terminal handed back around it, as `event_loop`
/// sees it.
///
/// Borrowed rather than boxed because it closes over [`run`]'s own borrow of
/// the runner, and a `Box<dyn Fn>` would demand `'static` of it. A test builds
/// one that does nothing.
type Suspended<'a> = &'a dyn Fn(&str) -> Result<(), String>;

/// Starts a fetch of the pods on one node of one cluster.
pub type PodsFetcher = Box<dyn Fn(&str, &str) -> mpsc::Receiver<Result<PodsFetch, FetchError>>>;

/// Starts a fetch of one pod's containers, given its namespace and name.
pub type ContainersFetcher =
    Box<dyn Fn(&str, &str, &str) -> mpsc::Receiver<Result<ContainersFetch, FetchError>>>;

/// Starts streaming one container's log, given its pod's namespace and name,
/// the container's own name, and whether to open its previous instance's log
/// (`kubectl logs -p`) rather than its current one. Unlike the other
/// fetchers, the returned [`StreamHandle`] is not incidental — dropping it is
/// the only way the stream this starts ever stops.
pub type LogsFetcher =
    Box<dyn Fn(&str, &str, &str, &str, bool) -> (mpsc::Receiver<LogEvent>, StreamHandle)>;

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
    /// Give the terminal back, log in to AWS, take it again, and refetch.
    ///
    /// Its own variant rather than something [`App`] could do itself, for the
    /// reason [`Quit`](Self::Quit) is one: the state machine decides, and the
    /// event loop — the only thing here that owns a terminal — acts.
    Login,
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
/// A fixed enum rather than a `Vec<View>` stack: `back_or_quit` and
/// `draw_detail` are each one exhaustive `match` over a set of levels this
/// tool already knows about, rather than a loop over a stack whose depth
/// nothing bounds. See decision 60 for why two known levels did not earn one;
/// a container's log is the level that finally did — not because four
/// variants is where a stack starts paying for itself either, but because
/// nothing past it is on the roadmap yet, and growing the stack machinery on
/// spec for a fifth level that may never arrive would be exactly the
/// speculative generality `CLAUDE.md` asks this tool to avoid.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum View {
    /// The selected cluster's node list.
    #[default]
    Overview,
    /// The pods placed on one node of the selected cluster.
    NodePods { node: String },
    /// The containers of one pod placed on `node`.
    PodContainers {
        node: String,
        namespace: String,
        pod: String,
    },
    /// One container's log, followed live.
    ContainerLogs {
        node: String,
        namespace: String,
        pod: String,
        container: String,
        /// Whether this is the container's previous instance's log —
        /// `kubectl logs -p` — rather than its current one. Part of `View`
        /// rather than a separate field on `App`, so toggling it is a view
        /// change like drilling in or backing out, and reuses the same
        /// "view changed, so (re)fetch" wiring in `event_loop` instead of a
        /// second trigger.
        previous: bool,
    },
}

/// The `/` fuzzy filter over the detail pane's current row list.
///
/// A pane's rows never leave the state they were fetched into — filtering is
/// a display-time reduction through [`crate::fuzzy::rank`], the same
/// function whichever pane is showing decides its search key for. `Editing`
/// captures every subsequent keystroke as query text rather than a
/// navigation key, mirrored in the footer's hints; `Applied` is what typing
/// keys go back to meaning once `Enter` commits it, with the filter still in
/// effect until `Esc` clears it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum Filter {
    /// No filter is active; every row shows, in whatever order the pane's own
    /// sort already put them in. The common case, and the one this must cost
    /// nothing extra to draw: `crate::fuzzy::rank` returns its rows unchanged
    /// for an empty query, so a dashboard nobody has searched in renders
    /// exactly as it always has.
    #[default]
    Inactive,
    /// The query is being typed.
    Editing(String),
    /// The query was committed with `Enter`; typing keys resume their usual
    /// meaning while the filter it named stays applied.
    Applied(String),
}

impl Filter {
    /// The text a row is currently being matched against — empty when no
    /// filter is active, whether or not one is being typed.
    fn query(&self) -> &str {
        match self {
            Self::Inactive => "",
            Self::Editing(query) | Self::Applied(query) => query,
        }
    }

    fn is_editing(&self) -> bool {
        matches!(self, Self::Editing(_))
    }

    fn is_applied(&self) -> bool {
        matches!(self, Self::Applied(_))
    }
}

/// Dashboard state.
#[derive(Debug, Clone)]
pub struct App {
    clusters: Vec<ClusterView>,
    selected: usize,
    theme: Theme,
    nodes: NodesState,
    pods: PodsState,
    containers: ContainersState,
    logs: LogsState,
    focus: Focus,
    view: View,
    /// The highlighted row within whichever list [`View`] is currently
    /// showing in the detail pane — node rows under [`View::Overview`], pod
    /// rows under [`View::NodePods`], container rows under
    /// [`View::PodContainers`]. Reset to `0` on every view change, every
    /// fresh load, and every change to `filter`, so it can never point past
    /// the end of a shorter list that just arrived or was just narrowed.
    detail_selected: usize,
    /// The fuzzy filter over whichever row list the detail pane is currently
    /// showing. Reset to [`Filter::Inactive`] on every view change — a query
    /// typed against one node's pods has nothing to say about the next one
    /// drilled into.
    filter: Filter,
    /// The node pane's ordering. `k8s::nodes::sort` and `k8s::order::note`
    /// are the same functions `eks nodes --sort` uses — a pane sorting its
    /// own rows differently from the table would mean `cpu` sorts a listing
    /// two ways depending on which screen printed it.
    node_order: k8s_nodes::Order,
    node_direction: SortDirection,
    /// The pod-drilldown pane's ordering, independent of the node pane's:
    /// the two panes hold different rows and `s`/`S` act on whichever one
    /// [`View`] is currently showing.
    pod_order: k8s_pods::Order,
    pod_direction: SortDirection,
    /// When the last unconfirmed `Esc`/`q` at the top level was pressed —
    /// `None` when no quit is pending. A second press of either key within
    /// [`QUIT_CONFIRM_WINDOW`] confirms it; any other key clears it.
    quit_armed_at: Option<Instant>,
    /// Whether the failure currently on screen is one a fresh AWS login could
    /// fix, which is what `L` turns on.
    ///
    /// A flag rather than a fifth thing threaded through the pane states: the
    /// question `L` asks is about the session, which belongs to the cluster
    /// rather than to whichever pane happened to notice. Every `apply_*` sets
    /// it from the [`FetchError`] it was handed, so a success anywhere clears
    /// it and the key stops offering something there is no longer a reason to
    /// do.
    credentials_lost: bool,
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
            containers: ContainersState::default(),
            logs: LogsState::default(),
            focus: Focus::default(),
            view: View::default(),
            detail_selected: 0,
            filter: Filter::default(),
            node_order: k8s_nodes::Order::default(),
            node_direction: SortDirection::default(),
            pod_order: k8s_pods::Order::default(),
            pod_direction: SortDirection::default(),
            quit_armed_at: None,
            credentials_lost: false,
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

    /// What the pod-containers pane is showing. Only meaningful while
    /// [`Self::view`] is [`View::PodContainers`], the same rule
    /// [`Self::pods`] follows for [`View::NodePods`].
    #[must_use]
    pub fn containers(&self) -> &ContainersState {
        &self.containers
    }

    /// What the container-logs pane is showing. Only meaningful while
    /// [`Self::view`] is [`View::ContainerLogs`], the same rule
    /// [`Self::containers`] follows for [`View::PodContainers`].
    #[must_use]
    pub fn logs(&self) -> &LogsState {
        &self.logs
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

    /// The `/` filter's current query text, or the empty string when no
    /// filter is active — the same reading a pane's `draw` takes it under to
    /// decide what it is showing.
    #[must_use]
    pub fn filter_query(&self) -> &str {
        self.filter.query()
    }

    /// Whether the `/` filter is currently capturing keystrokes as query
    /// text, for the footer's hints.
    #[must_use]
    pub fn is_filtering(&self) -> bool {
        self.filter.is_editing()
    }

    /// The node pane's current ordering.
    #[must_use]
    pub fn node_order(&self) -> k8s_nodes::Order {
        self.node_order
    }

    /// The node pane's current direction.
    #[must_use]
    pub fn node_direction(&self) -> SortDirection {
        self.node_direction
    }

    /// The pod-drilldown pane's current ordering.
    #[must_use]
    pub fn pod_order(&self) -> k8s_pods::Order {
        self.pod_order
    }

    /// The pod-drilldown pane's current direction.
    #[must_use]
    pub fn pod_direction(&self) -> SortDirection {
        self.pod_direction
    }

    /// Whether a quit is armed and awaiting its confirming `Esc`/`q` within
    /// `QUIT_CONFIRM_WINDOW`, for the footer's hint.
    #[must_use]
    pub fn quit_pending(&self) -> bool {
        self.quit_armed_at.is_some_and(|armed| {
            Instant::now().saturating_duration_since(armed) <= QUIT_CONFIRM_WINDOW
        })
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
    pub fn apply_nodes(&mut self, result: Result<NodesFetch, FetchError>) {
        let (order, direction) = (self.node_order, self.node_direction);
        self.credentials_lost = result.as_ref().is_err_and(|error| error.credentials);
        self.nodes = match (result, std::mem::take(&mut self.nodes)) {
            (Ok(fetch), _) => {
                let mut rows = fetch.rows;
                k8s_nodes::sort(&mut rows, order, direction);
                NodesState::Loaded {
                    rows,
                    usage_note: fetch.usage_note,
                    refresh_error: None,
                }
            }
            (
                Err(error),
                NodesState::Loaded {
                    rows, usage_note, ..
                },
            ) => NodesState::Loaded {
                rows,
                usage_note,
                refresh_error: Some(error.message),
            },
            (Err(error), _) => NodesState::Error(error.message),
        };
    }

    /// Whether the failure on screen is one a fresh AWS login could fix.
    ///
    /// What gates the `L` key and the hint that advertises it. Read by the
    /// renderer rather than baked into the pane's message because
    /// `k8s::client::explain` writes one wording for both surfaces: "run `aws
    /// sso login`" is the right advice on the command line, and on a dashboard
    /// that can do it for you it is not.
    #[must_use]
    pub fn credentials_lost(&self) -> bool {
        self.credentials_lost
    }

    /// The line offering the login, or `None` when there is nothing to offer.
    #[must_use]
    pub fn login_hint(&self) -> Option<&'static str> {
        self.credentials_lost
            .then_some("Press L to log in to AWS and try again.")
    }

    /// Record that a login the user asked for did not happen.
    ///
    /// The message is already a sentence, from `aws::login::Error`. It lands
    /// where a failed refresh lands, so it is read in the same place the
    /// failure that prompted it was — and `credentials_lost` stays set, because
    /// a login that did not run has not fixed anything and `L` is still the
    /// thing to press.
    pub fn apply_login_failure(&mut self, message: String) {
        self.nodes = match std::mem::take(&mut self.nodes) {
            NodesState::Loaded {
                rows, usage_note, ..
            } => NodesState::Loaded {
                rows,
                usage_note,
                refresh_error: Some(message),
            },
            _ => NodesState::Error(message),
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
        // The login offer belonged to the cluster whose rows just left the
        // pane, and it does not transfer. This is the *only* thing that calls
        // this method, so it is precisely the cluster-changed case and nothing
        // else: `r` and the refresh interval refetch without coming through
        // here, which is what keeps the offer alive while somebody retries the
        // cluster that actually failed.
        //
        // Leaving it set would leave `L` armed over a pane that is loading
        // rather than failing — and `L` does not ask, so it would open a
        // browser for a different account's profile than the one the user was
        // told about. That is the one property `aws::decide` exists to protect
        // (decision 76), and a sidebar full of clusters in different accounts
        // is exactly where it would have gone wrong.
        self.credentials_lost = false;
    }

    /// Apply the outcome of a fetch for one node's pods.
    ///
    /// Unlike [`Self::apply_nodes`], a failure always overwrites: the
    /// pod-drilldown pane fetches once per node it is asked to show rather
    /// than refreshing in the background, so there is no earlier good
    /// listing for this node worth keeping over a failed one.
    pub fn apply_pods(&mut self, result: Result<PodsFetch, FetchError>) {
        self.credentials_lost = result.as_ref().is_err_and(|error| error.credentials);
        self.pods = match result {
            Ok(fetch) => {
                let mut rows = fetch.rows;
                k8s_pods::sort(&mut rows, self.pod_order, self.pod_direction);
                PodsState::Loaded {
                    rows,
                    selector_note: fetch.selector_note,
                }
            }
            Err(error) => PodsState::Error(error.message),
        };
    }

    /// Apply the outcome of a fetch for one pod's containers.
    ///
    /// Like [`Self::apply_pods`] and unlike [`Self::apply_nodes`]: this pane
    /// fetches once per pod it is asked to show rather than refreshing in the
    /// background, so a failure always overwrites — there is no earlier good
    /// listing for *this* pod worth keeping over a failed one.
    pub fn apply_containers(&mut self, result: Result<ContainersFetch, FetchError>) {
        self.credentials_lost = result.as_ref().is_err_and(|error| error.credentials);
        self.containers = match result {
            Ok(fetch) => ContainersState::Loaded {
                rows: fetch.rows,
                ip: fetch.ip,
                nominated_node: fetch.nominated_node,
                readiness_gates: fetch.readiness_gates,
            },
            Err(error) => ContainersState::Error(error.message),
        };
    }

    /// Apply one piece of a container's log stream.
    ///
    /// Unlike [`Self::apply_pods`] and [`Self::apply_containers`], this is
    /// not "the fetch finished, here is the answer" — a log has no natural
    /// end, so this is called once per line and again whenever the stream
    /// stops. `LogsState::apply` is where the actual state machine lives,
    /// for the same reason [`Self::cycle_sort`] delegates to `k8s_nodes::sort`
    /// rather than reordering rows itself: the shape of "what does this event
    /// do to what is already on screen" belongs beside the data it changes.
    pub fn apply_log_event(&mut self, event: LogEvent) {
        self.logs.apply(event);
    }

    /// `s`: cycle to the next ordering for whichever pane [`View`] is
    /// currently showing, and re-sort its already-fetched rows.
    ///
    /// No fetch: the rows are already on screen, and `--sort` never refetches
    /// a listing either — it only changes how the answer already in hand is
    /// read back. Like `r`, this acts on the pane's data regardless of which
    /// pane currently holds keyboard focus.
    pub fn cycle_sort(&mut self) {
        match &self.view {
            View::Overview => {
                self.node_order = next_variant(self.node_order);
                if let NodesState::Loaded { rows, .. } = &mut self.nodes {
                    k8s_nodes::sort(rows, self.node_order, self.node_direction);
                }
            }
            View::NodePods { .. } => {
                self.pod_order = next_variant(self.pod_order);
                if let PodsState::Loaded { rows, .. } = &mut self.pods {
                    k8s_pods::sort(rows, self.pod_order, self.pod_direction);
                }
            }
            // No ordering yet: a pod rarely has more than a handful of
            // containers, already in the spec's own order, and `s` has
            // nothing to do here rather than a third ordering invented for a
            // list this short.
            View::PodContainers { .. } | View::ContainerLogs { .. } => {}
        }
    }

    /// `S`: flip the direction of whichever ordering is currently active,
    /// leaving the rows it cannot rank in the tail either way — the same
    /// rule `--sort-reverse` follows.
    pub fn reverse_sort(&mut self) {
        match &self.view {
            View::Overview => {
                self.node_direction = reverse(self.node_direction);
                if let NodesState::Loaded { rows, .. } = &mut self.nodes {
                    k8s_nodes::sort(rows, self.node_order, self.node_direction);
                }
            }
            View::NodePods { .. } => {
                self.pod_direction = reverse(self.pod_direction);
                if let PodsState::Loaded { rows, .. } = &mut self.pods {
                    k8s_pods::sort(rows, self.pod_order, self.pod_direction);
                }
            }
            View::PodContainers { .. } | View::ContainerLogs { .. } => {}
        }
    }

    /// `/`: open the fuzzy filter over whichever pane [`View`] is currently
    /// showing a row list, seeded with whatever query was already applied so
    /// a second press refines it rather than starting over.
    ///
    /// A no-op on [`View::ContainerLogs`], which has no rows to filter — the
    /// same exception `cycle_sort`/`reverse_sort` make. Switches focus to the
    /// detail pane regardless of which pane held it: once this returns, every
    /// following keystroke is filter text belonging to that pane, so it
    /// should be the one drawing the focus border.
    fn start_filter(&mut self) {
        if matches!(self.view, View::ContainerLogs { .. }) {
            return;
        }
        self.focus = Focus::Detail;
        self.filter = Filter::Editing(self.filter.query().to_owned());
        self.detail_selected = 0;
    }

    /// `Esc`/`Left` while a filter is applied and not being edited: clear it,
    /// rather than backing out a drill-down level — the same "unwind the
    /// newest thing first" rule the quit-arm and the drill-down already
    /// follow. A second `Esc` then backs out as usual.
    fn clear_filter(&mut self) {
        self.filter = Filter::Inactive;
        self.detail_selected = 0;
    }

    /// Handle a key press while [`Filter::Editing`] is capturing text —
    /// split out of [`Self::on_key`] so every other key's handling does not
    /// have to share a function with this one. `Enter` commits the query
    /// (collapsing an empty one back to [`Filter::Inactive`] rather than
    /// leaving an `Applied("")` with nothing to show for it); `Esc` cancels
    /// outright; `Backspace` and any other character edit the text. Every
    /// other key — including the ones that would otherwise navigate or
    /// quit — is simply not one of those and does nothing.
    fn edit_filter(&mut self, key: KeyEvent) -> Flow {
        let Filter::Editing(query) = &self.filter else {
            return Flow::Continue;
        };
        let mut query = query.clone();
        match key.code {
            KeyCode::Enter => {
                self.filter = if query.is_empty() {
                    Filter::Inactive
                } else {
                    Filter::Applied(query)
                };
            }
            KeyCode::Esc => self.filter = Filter::Inactive,
            KeyCode::Backspace => {
                query.pop();
                self.filter = Filter::Editing(query);
            }
            KeyCode::Char(c) => {
                query.push(c);
                self.filter = Filter::Editing(query);
            }
            _ => {}
        }
        self.detail_selected = 0;
        Flow::Continue
    }

    /// Toggle which pane `j`/`k`/`Home`/`End` move the highlight in.
    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Sidebar => Focus::Detail,
            Focus::Detail => Focus::Sidebar,
        };
    }

    /// Return the detail pane all the way to the node list, discarding any
    /// drill-down into a node's pods or a pod's containers.
    ///
    /// Called when the sidebar selects a different cluster: a pods or
    /// containers listing that belongs to the *previous* cluster is not an
    /// answer for the newly selected one, however many levels deep it was.
    /// `Esc` does not call this — it backs out one level at a time instead,
    /// through `on_key` — because leaving a drill-down on purpose and having
    /// the ground move under it are different events with different answers
    /// to "how far back".
    pub fn leave_detail_view(&mut self) {
        self.view = View::Overview;
        self.detail_selected = 0;
        self.filter = Filter::Inactive;
        self.pods = PodsState::default();
        self.containers = ContainersState::default();
        self.logs = LogsState::default();
    }

    /// Drill one level into whatever the detail pane is currently showing —
    /// a highlighted node's pods, or a highlighted pod's containers — if the
    /// detail pane is focused and something is actually highlighted.
    ///
    /// A no-op otherwise: pressing `Enter` with the sidebar focused, while a
    /// fetch is still loading and there is nothing to highlight yet, or from
    /// [`View::PodContainers`], where there is nowhere further to drill.
    /// Starting the next fetch itself is the event loop's job, once it sees
    /// the view change this causes; this method only decides *that* it
    /// happened, and to what.
    pub fn drill_in(&mut self) {
        if self.focus != Focus::Detail {
            return;
        }
        let Some(next) = self.next_view() else {
            return;
        };
        self.view = next;
        self.detail_selected = 0;
        self.filter = Filter::Inactive;
        match &self.view {
            View::Overview => {}
            View::NodePods { .. } => self.pods = PodsState::Loading,
            View::PodContainers { .. } => self.containers = ContainersState::Loading,
            View::ContainerLogs { .. } => self.logs = LogsState::Loading,
        }
    }

    /// What drilling in from the current view would show, or `None` when
    /// there is nowhere to drill — the sidebar has nothing highlighted yet,
    /// or [`View::PodContainers`] has no further level.
    ///
    /// Split out of [`Self::drill_in`] so the "what would this show" question
    /// is answered before anything about `self` changes: reading
    /// `self.detail_selected` against `self.pods.rows()` while also wanting
    /// to reassign `self.view` in the same breath is exactly the borrow a
    /// pure lookup avoids.
    fn next_view(&self) -> Option<View> {
        match &self.view {
            View::Overview => {
                let node = self.visible_nodes().get(self.detail_selected).copied()?;
                Some(View::NodePods {
                    node: node.name.clone(),
                })
            }
            View::NodePods { node } => {
                let pod = self.visible_pods().get(self.detail_selected).copied()?;
                Some(View::PodContainers {
                    node: node.clone(),
                    namespace: pod.namespace.clone(),
                    pod: pod.name.clone(),
                })
            }
            View::PodContainers {
                node,
                namespace,
                pod,
            } => {
                let container = self
                    .visible_containers()
                    .get(self.detail_selected)
                    .copied()?;
                Some(View::ContainerLogs {
                    node: node.clone(),
                    namespace: namespace.clone(),
                    pod: pod.clone(),
                    container: container.name.clone(),
                    previous: false,
                })
            }
            View::ContainerLogs { .. } => None,
        }
    }

    /// The node pane's rows in the order shown right now: every row, in the
    /// pane's own sorted order, when no filter is active — or the
    /// fuzzy-ranked matches for the current query when one is. The same
    /// function [`super::nodes::draw`] uses to decide what it draws, so a
    /// highlighted row and the one `Enter` drills into can never disagree.
    fn visible_nodes(&self) -> Vec<&k8s_nodes::NodeRow> {
        crate::fuzzy::rank(self.filter.query(), self.nodes.rows(), |row| {
            row.name.as_str()
        })
    }

    /// The pod-drilldown pane's counterpart to [`Self::visible_nodes`].
    fn visible_pods(&self) -> Vec<&k8s_pods::PodRow> {
        crate::fuzzy::rank(self.filter.query(), self.pods.rows(), |row| {
            row.name.as_str()
        })
    }

    /// The pod-containers pane's counterpart to [`Self::visible_nodes`].
    fn visible_containers(&self) -> Vec<&k8s_pods::ContainerRow> {
        crate::fuzzy::rank(self.filter.query(), self.containers.rows(), |row| {
            row.name.as_str()
        })
    }

    /// The [`k8s_nodes::NodeRow`] behind the node currently drilled into, from
    /// the node pane's own listing — not a second fetch of the node, since
    /// [`View::NodePods`] already names it and [`Self::nodes`] already holds
    /// it, fetched before the drill-down happened. `None` once a node has
    /// left that listing after being drilled into (scaled down, for
    /// instance), and outside [`View::NodePods`] entirely, where there is
    /// nothing to look up. Read off the full, unfiltered rows rather than
    /// [`Self::visible_nodes`]: a `/` query typed after drilling in narrows
    /// the *pod* list this pane is now showing, not the node identity the
    /// breadcrumb and this lookup are about.
    fn drilled_node(&self) -> Option<&k8s_nodes::NodeRow> {
        let View::NodePods { node } = &self.view else {
            return None;
        };
        self.nodes.rows().iter().find(|row| row.name == *node)
    }

    /// `Right`/`Tab`: move toward the detail pane and deeper into it —
    /// switch focus to [`Focus::Detail`] if the sidebar has it, or drill in
    /// if the detail pane already does.
    fn advance(&mut self) {
        match self.focus {
            Focus::Sidebar => self.toggle_focus(),
            Focus::Detail => self.drill_in(),
        }
    }

    /// `Left`/`Esc`: back out of a drill-down one level at a time; once
    /// there is no view left to back out of, move focus back to the
    /// sidebar; once that's already true too, arm or confirm a quit.
    ///
    /// Backing out of the view always wins over moving focus, regardless of
    /// which pane is focused — exactly like the single-purpose `Esc` this
    /// replaces, so a user already mid-drill still backs out one level per
    /// press. The pane-switch and quit-arming steps only appear once
    /// there's no view depth left to unwind.
    fn retreat(&mut self) -> Flow {
        match &self.view {
            View::NodePods { .. } | View::PodContainers { .. } | View::ContainerLogs { .. } => {
                self.back_out_one_level();
                Flow::Continue
            }
            View::Overview => match self.focus {
                Focus::Detail => {
                    self.toggle_focus();
                    Flow::Continue
                }
                Focus::Sidebar => self.quit_or_arm(),
            },
        }
    }

    /// Back out of the current drill-down by one level. A pod's containers
    /// back out to that pod's node's pods without a fetch: [`Self::pods`]
    /// was not touched by drilling further in, so the listing is still the
    /// one already on screen. Backing out of a node's pods to the node
    /// list, by contrast, has always discarded that listing outright —
    /// there is no cheaper "the node list is still current" to fall back
    /// on, since it never stopped being fetched in the background.
    ///
    /// Only called from [`Self::retreat`], which has already matched the
    /// view to one of the two non-[`View::Overview`] variants this expects.
    fn back_out_one_level(&mut self) {
        match &self.view {
            View::ContainerLogs {
                node,
                namespace,
                pod,
                ..
            } => {
                // `previous` does not survive backing out — the container
                // list is not carrying a "which mode was it in" question of
                // its own, and re-entering later should start from the
                // current log, the same default a fresh drill-in gets.
                self.view = View::PodContainers {
                    node: node.clone(),
                    namespace: namespace.clone(),
                    pod: pod.clone(),
                };
                self.detail_selected = 0;
                self.filter = Filter::Inactive;
                self.logs = LogsState::default();
            }
            View::PodContainers { node, .. } => {
                self.view = View::NodePods { node: node.clone() };
                self.detail_selected = 0;
                self.filter = Filter::Inactive;
                self.containers = ContainersState::default();
            }
            View::NodePods { .. } => self.leave_detail_view(),
            View::Overview => {}
        }
    }

    /// `Esc`/`q` at the top level: arm a pending quit on the first press,
    /// confirm and quit on a second press of either key within
    /// [`QUIT_CONFIRM_WINDOW`]. Any other key clears the pending arm (see
    /// [`Self::on_key`]), so a stray press elsewhere doesn't leave a
    /// dangling "press again" state for a later, unrelated `Esc`/`q` to
    /// confirm.
    fn quit_or_arm(&mut self) -> Flow {
        let now = Instant::now();
        if self
            .quit_armed_at
            .is_some_and(|armed| now.saturating_duration_since(armed) <= QUIT_CONFIRM_WINDOW)
        {
            self.quit_armed_at = None;
            return Flow::Quit;
        }
        self.quit_armed_at = Some(now);
        Flow::Continue
    }

    /// How many rows the detail pane's current view could highlight — after
    /// the `/` filter, so a highlight can never point past the end of a
    /// narrowed list.
    fn detail_row_count(&self) -> usize {
        match &self.view {
            View::Overview => self.visible_nodes().len(),
            View::NodePods { .. } => self.visible_pods().len(),
            View::PodContainers { .. } => self.visible_containers().len(),
            // Not a row list: `j`/`k`/`Home`/`End` scroll the log itself in
            // this view rather than moving a highlight, so there is no count
            // for them to be bounded against.
            View::ContainerLogs { .. } => 0,
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

    /// Scroll the container-logs pane toward older lines, or do nothing
    /// outside [`View::ContainerLogs`] — the same shape
    /// [`Self::select_next_detail_row`] has for a pane with no rows loaded
    /// yet.
    fn scroll_logs_up(&mut self, amount: usize) {
        if let LogsState::Streaming(log) = &mut self.logs {
            log.scroll_up(amount);
        }
    }

    /// The other direction of [`Self::scroll_logs_up`].
    fn scroll_logs_down(&mut self, amount: usize) {
        if let LogsState::Streaming(log) = &mut self.logs {
            log.scroll_down(amount);
        }
    }

    /// Jump the container-logs pane to its oldest line and stop following.
    fn jump_logs_to_start(&mut self) {
        if let LogsState::Streaming(log) = &mut self.logs {
            log.jump_to_start();
        }
    }

    /// Jump the container-logs pane to its newest line and resume following.
    fn jump_logs_to_end(&mut self) {
        if let LogsState::Streaming(log) = &mut self.logs {
            log.jump_to_end();
        }
    }

    /// `f`: jump the container-logs pane to the newest line and resume
    /// following, or stop following if it already was.
    fn toggle_log_follow(&mut self) {
        if let LogsState::Streaming(log) = &mut self.logs {
            log.toggle_follow();
        }
    }

    /// `w`: toggle line wrap in the container-logs pane.
    fn toggle_log_wrap(&mut self) {
        if let LogsState::Streaming(log) = &mut self.logs {
            log.toggle_wrap();
        }
    }

    /// `p`: switch the container-logs pane between a container's current log
    /// and its previous instance's — `kubectl logs -p`'s connection mode, for
    /// the container that crashed and is worth reading the log of the
    /// attempt *before* the one currently running.
    ///
    /// A no-op outside [`View::ContainerLogs`], the same shape every other
    /// key this pane owns has. `previous` always flips, both directions, so
    /// a second press of `p` always undoes the first — including out of the
    /// refusal below, which would otherwise be a dead end with no key back to
    /// the log that was showing before it. A container that has never
    /// restarted has no previous instance to open, so switching *to*
    /// `previous` on one is refused: rather than starting a fetch that could
    /// only ever answer "not found" — which reads exactly like a slow
    /// connection until it does — [`LogsState::Unavailable`] says so
    /// immediately, and still ends the current log's stream the ordinary
    /// "view just changed" way, through `start_drill_fetch`'s unconditional
    /// drop. The restart count comes from [`Self::containers`], the listing
    /// this pane's own drill-down already left in place, rather than a
    /// second copy carried on `View` itself.
    fn toggle_log_previous(&mut self) {
        let View::ContainerLogs { container, .. } = &self.view else {
            return;
        };
        let has_previous = self
            .containers
            .rows()
            .iter()
            .any(|row| row.name == *container && row.restarts > 0);

        let View::ContainerLogs { previous, .. } = &mut self.view else {
            return;
        };
        *previous = !*previous;

        self.logs = if *previous && !has_previous {
            LogsState::Unavailable(
                "This container has never restarted, so it has no previous log.".to_owned(),
            )
        } else {
            LogsState::Loading
        };
    }

    /// Handle a key press.
    ///
    /// Supports both arrow keys and vim-style `j`/`k`, because the people who
    /// live in this kind of tool expect the latter. `Right`/`Tab` and
    /// `Left`/`Esc` are each two names for the same pane-switch-then-drill
    /// motion, in opposite directions (see `advance`/`retreat` below).
    /// `Esc`/`q` only quit once nothing is left to back out of, and only on
    /// a second press within `QUIT_CONFIRM_WINDOW`; `Ctrl+C` always quits
    /// immediately, checked before anything else below. `/` opens the fuzzy
    /// filter, and while it is capturing text every other key below —
    /// including `q` and `Esc` — is filter text instead of its usual meaning.
    pub fn on_key(&mut self, key: KeyEvent) -> Flow {
        // Key *release* events arrive on Windows and modern terminals; acting on
        // both would move the selection twice per press.
        if key.kind == KeyEventKind::Release {
            return Flow::Continue;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            return Flow::Quit;
        }

        // While the `/` filter is capturing text, every key below — `j`,
        // `k`, `s`, `q`, the lot — is a character in the query rather than a
        // navigation key, so this returns before any of it is reached.
        if self.filter.is_editing() {
            return self.edit_filter(key);
        }

        // Any key other than the quit-family ones clears a pending quit arm,
        // so a stray press elsewhere doesn't leave a dangling "press again"
        // state for a much later, unrelated Esc/q to confirm.
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Left => {}
            _ => self.quit_armed_at = None,
        }

        match key.code {
            KeyCode::Char('q') => {
                if self.view == View::Overview {
                    return self.quit_or_arm();
                }
            }
            KeyCode::Char('/') => self.start_filter(),
            // A filter clears before a drill-down backs out, the same
            // "unwind the newest thing first" order the quit arm already
            // follows — so leaving a search behind takes one extra press,
            // not zero.
            KeyCode::Esc | KeyCode::Left if self.filter.is_applied() => self.clear_filter(),
            KeyCode::Esc | KeyCode::Left => return self.retreat(),
            KeyCode::Tab | KeyCode::Right => self.advance(),
            KeyCode::Enter => self.drill_in(),
            KeyCode::Char('s') => self.cycle_sort(),
            KeyCode::Char('S') => self.reverse_sort(),
            // Only when there is something for it to fix. A key that silently
            // does nothing is worse than one that is not offered, so the
            // footer hint appears under exactly this condition too.
            KeyCode::Char('L') if self.credentials_lost => return Flow::Login,
            // The container-logs pane has no rows to move a highlight
            // through — `j`/`k`/`Home`/`End`/`PageUp`/`PageDown` scroll its
            // text instead, the same keys a pager uses.
            KeyCode::Char('j') | KeyCode::Down
                if self.focus == Focus::Detail
                    && matches!(self.view, View::ContainerLogs { .. }) =>
            {
                self.scroll_logs_down(1);
            }
            KeyCode::Char('k') | KeyCode::Up
                if self.focus == Focus::Detail
                    && matches!(self.view, View::ContainerLogs { .. }) =>
            {
                self.scroll_logs_up(1);
            }
            KeyCode::PageDown
                if self.focus == Focus::Detail
                    && matches!(self.view, View::ContainerLogs { .. }) =>
            {
                self.scroll_logs_down(logs::PAGE);
            }
            KeyCode::PageUp
                if self.focus == Focus::Detail
                    && matches!(self.view, View::ContainerLogs { .. }) =>
            {
                self.scroll_logs_up(logs::PAGE);
            }
            KeyCode::Char('f') => self.toggle_log_follow(),
            KeyCode::Char('w') => self.toggle_log_wrap(),
            KeyCode::Char('p') => self.toggle_log_previous(),
            KeyCode::Char('j') | KeyCode::Down => match self.focus {
                Focus::Sidebar => self.select_next(),
                Focus::Detail => self.select_next_detail_row(),
            },
            KeyCode::Char('k') | KeyCode::Up => match self.focus {
                Focus::Sidebar => self.select_previous(),
                Focus::Detail => self.select_previous_detail_row(),
            },
            KeyCode::Home
                if self.focus == Focus::Detail
                    && matches!(self.view, View::ContainerLogs { .. }) =>
            {
                self.jump_logs_to_start();
            }
            KeyCode::End
                if self.focus == Focus::Detail
                    && matches!(self.view, View::ContainerLogs { .. }) =>
            {
                self.jump_logs_to_end();
            }
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
/// pane's view changes to [`View::NodePods`]. `spawn_containers` is one level
/// further in: the selected cluster's context, and the namespace and name of
/// the drilled-into pod, whenever the view changes to [`View::PodContainers`].
/// `spawn_logs` is the last level: the selected cluster's context, the
/// drilled-into pod's namespace and name, the drilled-into container's name,
/// and whether to open its previous instance's log rather than its current
/// one, whenever the view changes to or within [`View::ContainerLogs`] — `p`
/// flipping that last flag counts as "within" the same way drilling in the
/// first time counts as "to". Unlike the other three, the [`StreamHandle`] it
/// hands back alongside the receiver has
/// to be held onto for as long as the pane is showing that stream and
/// dropped the moment it is not — see [`crate::commands::spawn_stream`]'s doc
/// comment for why dropping it is what actually ends the connection, rather
/// than merely this function losing interest in it. The three drill fetchers
/// travel together as [`DrillFetchers`], the same reason `Inflight` bundles
/// what they start: a fourth drill-down level should not have to grow every
/// caller's argument list past `clippy::too_many_arguments`' limit again.
///
/// This function never awaits a fetch: each iteration only polls for a result
/// that has already arrived, which is what keeps a hung request from blocking
/// a keypress.
pub fn run(
    app: App,
    nodes_rx: Option<mpsc::Receiver<Result<NodesFetch, FetchError>>>,
    spawn_nodes: &NodesFetcher,
    drill: &DrillFetchers<'_>,
    refresh: RefreshInterval,
    login: &LoginRunner,
) -> Result<()> {
    let mut terminal = ratatui::init();

    // The suspend-and-resume that `L` needs, built here because this is the
    // only function that knows a real terminal is involved. `event_loop` is
    // generic over the backend so it can be driven by `TestBackend`, and a
    // test's version of this does nothing at all.
    let suspended = |context: &str| -> Result<(), String> {
        // Give the shell its terminal back: `aws sso login` prints a device
        // code and may prompt, and neither is readable through an alternate
        // screen in raw mode.
        let left = leave_terminal();
        let outcome = login(context);
        // Retaken whatever happened. Leaving the user in a half-restored
        // terminal because a login failed would be worse than the failure, so
        // a failure to re-enter is reported *after* the login's own.
        let entered = enter_terminal();
        outcome.and(left).and(entered)
    };

    let result = event_loop(
        &mut terminal,
        app,
        nodes_rx,
        spawn_nodes,
        drill,
        refresh,
        &suspended,
    );
    ratatui::restore();
    result
}

/// Hand the terminal back to the shell, keeping the `Terminal` handle valid.
///
/// Deliberately not `ratatui::restore()` followed by `ratatui::init()`: that
/// pair hands back a *new* `Terminal`, and the one `event_loop` is holding —
/// along with everything it has cached about the screen — would have to be
/// replaced mid-loop. These are the two things `init` does that matter to a
/// suspended session, undone and redone around the handle we already have.
fn leave_terminal() -> Result<(), String> {
    terminal::disable_raw_mode().map_err(|error| format!("could not leave raw mode: {error}"))?;
    execute!(std::io::stdout(), terminal::LeaveAlternateScreen)
        .map_err(|error| format!("could not leave the alternate screen: {error}"))
}

/// The other half of [`leave_terminal`].
fn enter_terminal() -> Result<(), String> {
    terminal::enable_raw_mode().map_err(|error| format!("could not re-enter raw mode: {error}"))?;
    execute!(std::io::stdout(), terminal::EnterAlternateScreen)
        .map_err(|error| format!("could not re-open the alternate screen: {error}"))
}

/// The fetchers for the detail pane's three drill-down levels, bundled into
/// one parameter for the reason [`run`]'s doc comment gives.
#[derive(Clone, Copy)]
pub struct DrillFetchers<'a> {
    pub spawn_pods: &'a PodsFetcher,
    pub spawn_containers: &'a ContainersFetcher,
    pub spawn_logs: &'a LogsFetcher,
}

// `Box<dyn Fn(..) -> ..>` has no `Debug` impl for `#[derive(Debug)]` to call,
// so this satisfies `missing_debug_implementations` by hand rather than
// printing three closures nobody could read anyway.
impl std::fmt::Debug for DrillFetchers<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DrillFetchers").finish_non_exhaustive()
    }
}

/// The background fetches currently in flight for whichever drill-down level
/// the detail pane is showing.
///
/// Bundled into one type for the same reason [`DrillFetchers`] is: `event_loop`
/// otherwise grows one more local and one more thing to clear per level
/// `View` gains. `logs_handle` is held only so it is not dropped early and is
/// never read directly — dropping it, via [`Self::clear`] or by being
/// overwritten, is the cancellation itself (see
/// [`crate::commands::spawn_stream`]).
#[derive(Default)]
struct Inflight {
    pods: Option<mpsc::Receiver<Result<PodsFetch, FetchError>>>,
    containers: Option<mpsc::Receiver<Result<ContainersFetch, FetchError>>>,
    logs: Option<mpsc::Receiver<LogEvent>>,
    logs_handle: Option<StreamHandle>,
}

impl Inflight {
    /// Stop every drill-down fetch, whatever level it belongs to. Called on
    /// a cluster switch, and when the detail pane returns to
    /// [`View::Overview`], both of which drop every level at once rather
    /// than one.
    fn clear(&mut self) {
        self.pods = None;
        self.containers = None;
        self.logs = None;
        drop(self.logs_handle.take());
    }
}

fn event_loop<B>(
    terminal: &mut Terminal<B>,
    mut app: App,
    mut nodes_rx: Option<mpsc::Receiver<Result<NodesFetch, FetchError>>>,
    spawn_nodes: &NodesFetcher,
    drill: &DrillFetchers<'_>,
    refresh: RefreshInterval,
    login: Suspended<'_>,
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
    let mut inflight = Inflight::default();

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
        if let Some(rx) = &inflight.pods
            && let Ok(result) = rx.try_recv()
        {
            app.apply_pods(result);
        }
        if let Some(rx) = &inflight.containers
            && let Ok(result) = rx.try_recv()
        {
            app.apply_containers(result);
        }
        // Drained in a loop rather than one `try_recv` per frame: a log
        // sends many events, not one, and a burst of them arriving between
        // two frames must reach the buffer before the next paint rather than
        // trickling in one line every `TICK` — the whole point of the
        // acceptance criterion that a burst must not stall the UI is that it
        // catches up immediately once control comes back here.
        if let Some(rx) = &inflight.logs {
            while let Ok(event) = rx.try_recv() {
                app.apply_log_event(event);
            }
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

        match app.on_key(key) {
            Flow::Quit => return Ok(()),
            Flow::Login => {
                // The screen belonged to the AWS CLI for the duration, so
                // whatever `ratatui` last drew is gone from the real terminal
                // and its own idea of what is on screen is stale.
                let outcome = login(selected_context.as_deref().unwrap_or_default());
                terminal.clear()?;
                match outcome {
                    // A fresh token is worth nothing until something uses it:
                    // refetching immediately is what turns the banner back
                    // into rows without a second keystroke.
                    Ok(()) => {
                        refetch(spawn_nodes, &mut nodes_rx, selected_context.as_deref());
                        next_refresh = schedule(refresh);
                    }
                    Err(message) => app.apply_login_failure(message),
                }
            }
            Flow::Continue => {}
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
            app.leave_detail_view();
            inflight.clear();
            refetch(spawn_nodes, &mut nodes_rx, selected_context.as_deref());
            next_refresh = schedule(refresh);
        } else if *app.view() != view_before {
            // Not an `else if` on the selection check above by accident: a
            // cluster change already forces the view back to `Overview`
            // through `leave_detail_view`, so re-deriving the same outcome
            // here would just repeat it.
            start_drill_fetch(&app, drill, selected_context.as_deref(), &mut inflight);
        }
    }
}

/// Start whichever fetch the detail pane's new view needs, once
/// [`App::view`] has just changed to it.
///
/// Every branch but [`View::Overview`]'s also stops the fetches for the
/// levels this view is not — unconditionally, not only when backing out of
/// one: drilling *forward* past a level that never had a fetch running finds
/// nothing there to clear, and backing *out* of one is exactly the case that
/// has to end its stream (`ContainerLogs`'s, in particular — see
/// [`Inflight::logs_handle`]).
fn start_drill_fetch(
    app: &App,
    drill: &DrillFetchers<'_>,
    context: Option<&str>,
    inflight: &mut Inflight,
) {
    match app.view() {
        View::NodePods { node } => {
            inflight.containers = None;
            inflight.logs = None;
            drop(inflight.logs_handle.take());
            // Only when drilling *forward* into this node — `Esc` backing
            // out of that node's `PodContainers` also lands here, and the
            // listing it left behind is still current, so `apply_containers`
            // cleared it rather than `App::pods` moving to `Loading` the way
            // it does here.
            if matches!(app.pods(), PodsState::Loading)
                && let Some(context) = context
            {
                inflight.pods = Some((drill.spawn_pods)(context, node));
            }
        }
        View::PodContainers { namespace, pod, .. } => {
            inflight.logs = None;
            drop(inflight.logs_handle.take());
            if matches!(app.containers(), ContainersState::Loading)
                && let Some(context) = context
            {
                inflight.containers = Some((drill.spawn_containers)(context, namespace, pod));
            }
        }
        View::ContainerLogs {
            namespace,
            pod,
            container,
            previous,
            ..
        } => {
            // Unconditional, unlike the fetch itself below: this view can
            // change *within* itself — `p` flips `previous` without leaving
            // `ContainerLogs` — and the stream that answers is only ever
            // cancelled by dropping its `StreamHandle`, so switching modes
            // without dropping the old one first would leave it running
            // uselessly alongside the new one.
            inflight.logs = None;
            drop(inflight.logs_handle.take());
            if matches!(app.logs(), LogsState::Loading)
                && let Some(context) = context
            {
                let (rx, handle) =
                    (drill.spawn_logs)(context, namespace, pod, container, *previous);
                inflight.logs = Some(rx);
                inflight.logs_handle = Some(handle);
            }
        }
        View::Overview => inflight.clear(),
    }
}

/// Start a fetch for whichever cluster is selected, replacing whatever was
/// in flight. A no-op when nothing is selected — an empty kubeconfig has no
/// cluster to fetch.
fn refetch(
    spawn_nodes: &NodesFetcher,
    nodes_rx: &mut Option<mpsc::Receiver<Result<NodesFetch, FetchError>>>,
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

/// The next value after `current` in `O`'s declaration order, wrapping back
/// to the first. `--sort` takes a value; a pane cycles through the same set
/// one key press at a time, so this is the flag's value list read as a ring
/// rather than parsed from text.
fn next_variant<O: ValueEnum + Copy + PartialEq>(current: O) -> O {
    let variants = O::value_variants();
    let index = variants
        .iter()
        .position(|value| *value == current)
        .unwrap_or(0);
    variants[(index + 1) % variants.len()]
}

/// Flip a [`SortDirection`], the pane's counterpart to `--sort-reverse`.
fn reverse(direction: SortDirection) -> SortDirection {
    match direction {
        SortDirection::Natural => SortDirection::Reversed,
        SortDirection::Reversed => SortDirection::Natural,
    }
}

/// Draw one frame.
pub fn draw(frame: &mut Frame, app: &App) {
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
    draw_footer(frame, chunks[2], app);
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
        View::PodContainers { node, pod, .. } => format!(" Overview › {node} › {pod} "),
        View::ContainerLogs {
            node,
            pod,
            container,
            ..
        } => format!(" Overview › {node} › {pod} › {container} "),
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
        View::Overview => nodes::draw(
            frame,
            sections[1],
            app.nodes(),
            highlighted,
            app.node_order(),
            app.node_direction(),
            app.filter_query(),
            app.login_hint(),
            theme,
        ),
        View::NodePods { .. } => pods::draw(
            frame,
            sections[1],
            app.pods(),
            app.drilled_node(),
            highlighted,
            app.pod_order(),
            app.pod_direction(),
            app.filter_query(),
            theme,
        ),
        View::PodContainers { .. } => {
            containers::draw(
                frame,
                sections[1],
                app.containers(),
                highlighted,
                app.filter_query(),
                theme,
            );
        }
        View::ContainerLogs { previous, .. } => {
            logs::draw(frame, sections[1], app.logs(), *previous, theme);
        }
    }
}

fn detail_row<'a>(label: &'a str, value: &'a str, theme: Theme) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{label:<10}"), theme.dim()),
        Span::styled(value, theme.body()),
    ])
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme;

    if app.quit_pending() {
        let warning = Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "press esc/q again to quit",
                theme.severity(crate::theme::Severity::Warn),
            ),
        ]);
        frame.render_widget(Paragraph::new(warning), area);
        return;
    }

    let hints: &[(&str, &str)] = if app.is_filtering() {
        // Every other key below is filter text while this is showing — see
        // `App::on_key` — so the hints say that instead of listing keys that
        // do not mean what they usually do right now.
        &[("type", "filter"), ("enter", "apply"), ("esc", "cancel")]
    } else if app.credentials_lost() {
        // Its own list rather than one more hint on the end of the others,
        // for the reason the two branches around it are: this is a state that
        // changes what the keys are worth. Until the session is back, `j/k`
        // and `s/S` are moving around a listing nothing can refill, and the
        // hints that survive are the ones that lead somewhere. Keeping the
        // list short is also what keeps `q quit` on screen at eighty columns,
        // which the default list below is deliberately ordered to protect.
        &[
            ("L", "log in"),
            ("tab/→", "switch"),
            ("←/esc", "back"),
            ("r", "refresh"),
            ("q", "quit"),
        ]
    } else if matches!(app.view(), View::ContainerLogs { .. }) {
        // A log has nothing to `enter` further into and no ordering `s`/`S`
        // could apply to — `f`/`w`/`p` take their place, the three things
        // this pane's own keys change.
        &[
            ("tab/→", "switch"),
            ("j/k", "scroll"),
            ("f", "follow"),
            ("w", "wrap"),
            ("p", "previous"),
            ("←/esc", "back"),
            ("q", "quit"),
        ]
    } else {
        &[
            ("tab/→", "switch/drill"),
            ("j/k", "move"),
            ("enter", "open"),
            ("←/esc", "back"),
            ("r", "refresh"),
            ("s/S", "sort"),
            ("q", "quit"),
            // Last, not because it matters least, but so a narrow terminal
            // clips the newest hint before it clips `q quit` — the one this
            // tool can least afford to hide.
            ("/", "filter"),
        ]
    };

    let mut spans = vec![Span::raw(" ")];
    for &(key, action) in hints {
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

    /// A fetch that failed for a reason no login could fix — the ordinary
    /// case. `refused` below is the other one.
    fn failed(message: &str) -> FetchError {
        FetchError {
            message: message.to_owned(),
            credentials: false,
        }
    }

    /// A fetch the cluster refused for want of credentials, which is what
    /// puts `L` on the footer.
    fn refused(message: &str) -> FetchError {
        FetchError {
            message: message.to_owned(),
            credentials: true,
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

    /// Unwrap the `LogsState::Streaming` a test expects, via `expect` rather
    /// than a bare `panic!` — `clippy::panic` is denied crate-wide and does
    /// not carve out tests the way `unwrap_used`/`expect_used` do above.
    fn streaming(state: &LogsState) -> &logs::Log {
        match state {
            LogsState::Streaming(log) => Some(log),
            LogsState::Loading | LogsState::Error(_) | LogsState::Unavailable(_) => None,
        }
        .expect("expected Streaming")
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
            ephemeral_storage: Capacity::default(),
            hugepages: BTreeMap::new(),
        }
    }

    fn pod_row(name: &str) -> crate::k8s::pods::PodRow {
        use crate::k8s::quantity::Quantity;

        crate::k8s::pods::PodRow {
            namespace: "default".to_owned(),
            name: name.to_owned(),
            ready: "1/1".to_owned(),
            status: "Running".to_owned(),
            severity: crate::theme::Severity::Ok,
            restarts: 0,
            restart_age: None,
            last_restart: None,
            age: "3d".to_owned(),
            created_at: None,
            cpu_used: None,
            memory_used: None,
            cpu_requested: Quantity::default(),
            memory_requested: Quantity::default(),
            extended_requested: std::collections::BTreeMap::new(),
            node: "worker-1".to_owned(),
            ip: "-".to_owned(),
            nominated_node: "-".to_owned(),
            readiness_gates: None,
        }
    }

    /// A node with a measured CPU share, for the tests that sort on it.
    fn node_row_with_cpu(name: &str, used: &str, allocatable: &str) -> crate::k8s::nodes::NodeRow {
        use crate::k8s::nodes::Share;
        use crate::k8s::quantity::Quantity;

        crate::k8s::nodes::NodeRow {
            cpu_used: Share {
                amount: Some(Quantity::parse(used).unwrap()),
                allocatable: Some(Quantity::parse(allocatable).unwrap()),
            },
            ..node_row(name)
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

    /// An app with two distinctly-named nodes loaded, for the filter tests
    /// that need more than one row to narrow between.
    fn app_with_two_nodes() -> App {
        let mut app = app();
        app.apply_nodes(Ok(NodesFetch {
            rows: vec![node_row("worker-1"), node_row("worker-2")],
            usage_note: None,
        }));
        app.toggle_focus();
        app
    }

    /// An app drilled one level further than [`app_with_node`]: one pod
    /// already loaded under `worker-1`, highlighted, ready to drill into its
    /// containers.
    fn app_with_pod() -> App {
        let mut app = app_with_node();
        app.on_key(press(KeyCode::Enter));
        app.apply_pods(Ok(PodsFetch {
            rows: vec![pod_row("api-1")],
            selector_note: None,
        }));
        app
    }

    fn container_row(name: &str) -> crate::k8s::pods::ContainerRow {
        crate::k8s::pods::ContainerRow {
            name: name.to_owned(),
            image: "app:1.0".to_owned(),
            init: false,
            ready: true,
            restarts: 0,
            state: "Running".to_owned(),
            severity: crate::theme::Severity::Ok,
            requests: crate::k8s::pods::Requests::default(),
            cpu_limit: None,
            memory_limit: None,
        }
    }

    /// An app drilled one level further than [`app_with_pod`]: one container
    /// already loaded under `api-1`, highlighted, ready to drill into its
    /// logs.
    fn app_with_container() -> App {
        let mut app = app_with_pod();
        app.on_key(press(KeyCode::Enter));
        app.apply_containers(Ok(ContainersFetch {
            rows: vec![container_row("app")],
            ..ContainersFetch::default()
        }));
        app
    }

    /// [`app_with_container`]'s counterpart for the `p` (previous log) tests:
    /// a container that has restarted, so it has a previous instance to
    /// switch to.
    fn app_with_crashed_container() -> App {
        let mut app = app_with_pod();
        app.on_key(press(KeyCode::Enter));
        app.apply_containers(Ok(ContainersFetch {
            rows: vec![crate::k8s::pods::ContainerRow {
                restarts: 3,
                ..container_row("app")
            }],
            ..ContainersFetch::default()
        }));
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
    fn ctrl_c_quits_immediately() {
        assert_eq!(
            app().on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Flow::Quit
        );
    }

    #[test]
    fn ctrl_c_quits_even_with_a_pending_quit_armed() {
        let mut app = app();
        app.on_key(press(KeyCode::Esc));

        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Flow::Quit
        );
    }

    #[test]
    fn esc_or_q_at_the_top_level_arms_a_pending_quit_without_quitting() {
        assert_eq!(app().on_key(press(KeyCode::Esc)), Flow::Continue);
        assert_eq!(app().on_key(press(KeyCode::Char('q'))), Flow::Continue);
    }

    #[test]
    fn esc_twice_in_rapid_succession_at_the_top_level_quits() {
        let mut app = app();
        assert_eq!(app.on_key(press(KeyCode::Esc)), Flow::Continue);
        assert_eq!(app.on_key(press(KeyCode::Esc)), Flow::Quit);
    }

    #[test]
    fn q_twice_in_rapid_succession_at_the_top_level_quits() {
        let mut app = app();
        assert_eq!(app.on_key(press(KeyCode::Char('q'))), Flow::Continue);
        assert_eq!(app.on_key(press(KeyCode::Char('q'))), Flow::Quit);
    }

    #[test]
    fn esc_then_q_in_rapid_succession_at_the_top_level_quits() {
        let mut app = app();
        assert_eq!(app.on_key(press(KeyCode::Esc)), Flow::Continue);
        assert_eq!(app.on_key(press(KeyCode::Char('q'))), Flow::Quit);
    }

    #[test]
    fn an_unrelated_key_between_two_quit_presses_cancels_the_arm() {
        let mut app = app();
        assert_eq!(app.on_key(press(KeyCode::Esc)), Flow::Continue);
        app.on_key(press(KeyCode::Char('j')));
        assert_eq!(
            app.on_key(press(KeyCode::Esc)),
            Flow::Continue,
            "a navigation key in between must cancel the pending quit"
        );
    }

    #[test]
    fn a_stale_quit_arm_past_the_window_does_not_quit() {
        let mut app = app();
        app.on_key(press(KeyCode::Esc));
        app.quit_armed_at = Instant::now().checked_sub(Duration::from_millis(700));

        assert_eq!(
            app.on_key(press(KeyCode::Esc)),
            Flow::Continue,
            "a press outside the confirm window must re-arm rather than quit"
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

        app.apply_nodes(Err(failed("could not list nodes")));

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

        app.apply_nodes(Err(failed("could not list nodes: nope")));

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
    fn l_offers_a_login_only_when_the_failure_on_screen_is_a_credential_one() {
        let mut app = app();
        app.apply_nodes(Err(refused("prod rejected your credentials")));

        assert!(app.credentials_lost());
        assert_eq!(app.on_key(press(KeyCode::Char('L'))), Flow::Login);
    }

    #[test]
    fn l_does_nothing_when_the_failure_is_one_a_login_could_not_fix() {
        // An unreachable private endpoint, a `403` from the cluster's own
        // access entries: pressing `L` there would open a browser, log the
        // user in perfectly, and change nothing at all.
        let mut app = app();
        app.apply_nodes(Err(failed("could not reach the API server for prod")));

        assert!(!app.credentials_lost());
        assert_eq!(app.on_key(press(KeyCode::Char('L'))), Flow::Continue);
        assert_eq!(app.login_hint(), None);
    }

    #[test]
    fn l_does_nothing_on_a_dashboard_that_is_working_fine() {
        let mut app = app();
        app.apply_nodes(Ok(NodesFetch::default()));

        assert_eq!(app.on_key(press(KeyCode::Char('L'))), Flow::Continue);
    }

    #[test]
    fn a_successful_fetch_withdraws_the_offer_of_a_login() {
        // The banner and the key go together: rows on screen mean the
        // credentials worked, and `L` has nothing left to put right.
        let mut app = app();
        app.apply_nodes(Err(refused("prod rejected your credentials")));

        app.apply_nodes(Ok(NodesFetch::default()));

        assert!(!app.credentials_lost());
        assert_eq!(app.login_hint(), None);
    }

    #[test]
    fn a_credential_refusal_from_any_pane_offers_the_login() {
        // The session belongs to the cluster, not to whichever pane happened
        // to be the one that asked.
        let mut pods = app();
        pods.apply_pods(Err(refused("prod rejected your credentials")));
        assert!(pods.credentials_lost());

        let mut containers = app();
        containers.apply_containers(Err(refused("prod rejected your credentials")));
        assert!(containers.credentials_lost());
    }

    #[test]
    fn a_login_that_failed_is_reported_without_withdrawing_the_offer() {
        // Nothing was fixed, so `L` is still the thing to press — and the
        // reason it did not work has to be readable somewhere.
        let mut app = app();
        app.apply_nodes(Err(refused("prod rejected your credentials")));

        app.apply_login_failure("could not start `aws sso login`".to_owned());

        assert!(app.credentials_lost());
        assert_eq!(
            app.nodes(),
            &NodesState::Error("could not start `aws sso login`".to_owned())
        );
    }

    #[test]
    fn a_login_that_failed_over_good_rows_keeps_them() {
        // The same rule a failed refresh follows: one bad login does not blank
        // a dashboard that is still showing a working listing.
        let mut app = app();
        app.apply_nodes(Ok(NodesFetch::default()));
        app.apply_nodes(Err(refused("prod rejected your credentials")));

        app.apply_login_failure("could not start `aws sso login`".to_owned());

        assert_eq!(
            app.nodes(),
            &NodesState::Loaded {
                rows: Vec::new(),
                usage_note: None,
                refresh_error: Some("could not start `aws sso login`".to_owned()),
            }
        );
    }

    #[test]
    fn switching_clusters_withdraws_a_login_offer_meant_for_the_previous_one() {
        // A sidebar full of clusters in different AWS accounts is the case
        // this protects: `L` does not ask before it runs, so an offer left
        // over from the cluster that failed would open a browser for whatever
        // account the *newly* selected one uses.
        let mut app = app();
        app.apply_nodes(Err(refused("prod rejected your credentials")));

        app.start_loading_nodes();

        assert!(!app.credentials_lost());
        assert_eq!(app.login_hint(), None);
        assert_eq!(app.on_key(press(KeyCode::Char('L'))), Flow::Continue);
    }

    #[test]
    fn refreshing_the_same_cluster_keeps_the_offer() {
        // The contrast case, and the reason the reset lives in
        // `start_loading_nodes` rather than anywhere a refetch passes through:
        // `r` is somebody retrying the cluster that failed, and the offer is
        // still exactly what they need.
        let mut app = app();
        app.apply_nodes(Err(refused("prod rejected your credentials")));

        assert!(is_refresh_key(press(KeyCode::Char('r'))));
        app.on_key(press(KeyCode::Char('r')));

        assert!(app.credentials_lost());
        assert_eq!(app.on_key(press(KeyCode::Char('L'))), Flow::Login);
    }

    #[test]
    fn the_footer_drops_the_login_hint_as_soon_as_the_pane_starts_loading_again() {
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        let mut app = app();
        app.apply_nodes(Err(refused("prod rejected your credentials")));

        app.start_loading_nodes();
        terminal.draw(|frame| draw(frame, &app)).unwrap();

        let rendered = terminal.backend().to_string();
        assert!(!rendered.contains("log in"), "{rendered}");
        // Back to the ordinary hint list rather than the credential one, which
        // does not carry `s/S`.
        assert!(rendered.contains("s/S"), "{rendered}");
    }

    #[test]
    fn l_is_filter_text_while_the_filter_is_capturing() {
        // Every other key is, and a capital `L` in a node name is not a
        // request to open a browser.
        let mut app = app();
        app.apply_nodes(Err(refused("prod rejected your credentials")));
        app.on_key(press(KeyCode::Char('/')));

        assert_eq!(app.on_key(press(KeyCode::Char('L'))), Flow::Continue);
        assert_eq!(app.filter_query(), "L");
    }

    #[test]
    fn a_successful_refresh_clears_an_earlier_refresh_failure() {
        let mut app = app();
        app.apply_nodes(Ok(NodesFetch::default()));
        app.apply_nodes(Err(failed("could not list nodes: nope")));

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
    fn the_footer_offers_l_only_while_a_login_would_help() {
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        let mut app = app();

        app.apply_nodes(Err(failed("could not reach the API server")));
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        assert!(
            !terminal.backend().to_string().contains("log in"),
            "an unreachable cluster is not a login problem"
        );

        app.apply_nodes(Err(refused("prod rejected your credentials")));
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("log in"), "{rendered}");
        // First on the line, because until it is done every other key here
        // leads back to the same error — and `q quit` still fits beside it,
        // which is the whole reason this footer is its own short list.
        assert!(
            rendered.find("log in") < rendered.find("quit"),
            "{rendered}"
        );
        assert!(rendered.contains("quit"), "{rendered}");
    }

    #[test]
    fn footer_shows_a_press_again_hint_once_a_quit_is_armed() {
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        let mut app = app();

        app.on_key(press(KeyCode::Esc));
        terminal.draw(|frame| draw(frame, &app)).unwrap();

        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("press esc/q again to quit"), "{rendered}");
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
    fn a_frame_drilled_into_a_nodes_pods_shows_its_wide_facts() {
        let node = crate::k8s::nodes::NodeRow {
            internal_ip: "10.0.1.9".to_owned(),
            external_ip: "34.201.1.2".to_owned(),
            os_image: "Amazon Linux 2023".to_owned(),
            kernel_version: "6.1.148".to_owned(),
            container_runtime: "containerd://1.7.28".to_owned(),
            ..node_row("worker-1")
        };
        let mut app = app();
        app.apply_nodes(Ok(NodesFetch {
            rows: vec![node],
            usage_note: None,
        }));
        app.toggle_focus();
        app.on_key(press(KeyCode::Enter));
        app.apply_pods(Ok(PodsFetch {
            rows: vec![pod_row("api-1")],
            selector_note: None,
        }));

        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let rendered = terminal.backend().to_string();

        assert!(rendered.contains("INTERNAL-IP: 10.0.1.9"), "{rendered}");
        assert!(rendered.contains("api-1"), "{rendered}");
    }

    #[test]
    fn drilled_node_finds_the_row_behind_the_view() {
        let mut app = app_with_node();
        app.on_key(press(KeyCode::Enter));

        assert_eq!(
            app.drilled_node().map(|row| row.name.as_str()),
            Some("worker-1")
        );
    }

    #[test]
    fn drilled_node_is_none_outside_the_node_pods_view() {
        let app = app_with_node();

        assert_eq!(app.drilled_node(), None);
    }

    #[test]
    fn drilled_node_is_none_once_the_node_has_left_the_listing() {
        // A background refresh that no longer reports this node — scaled
        // down, or removed from the cluster, while its pods were open.
        let mut app = app_with_node();
        app.on_key(press(KeyCode::Enter));

        app.apply_nodes(Ok(NodesFetch {
            rows: vec![node_row("worker-2")],
            usage_note: None,
        }));

        assert_eq!(app.drilled_node(), None);
    }

    #[test]
    fn a_frame_drilled_into_a_pods_containers_carries_the_full_breadcrumb() {
        let mut app = app_with_pod();
        app.on_key(press(KeyCode::Enter));
        app.apply_containers(Ok(ContainersFetch {
            rows: vec![crate::k8s::pods::ContainerRow {
                name: "app".to_owned(),
                image: "app:1.0".to_owned(),
                init: false,
                ready: true,
                restarts: 0,
                state: "Running".to_owned(),
                severity: crate::theme::Severity::Ok,
                requests: crate::k8s::pods::Requests::default(),
                cpu_limit: None,
                memory_limit: None,
            }],
            ..ContainersFetch::default()
        }));

        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();

        let rendered = terminal.backend().to_string();
        assert!(
            rendered.contains("Overview › worker-1 › api-1"),
            "{rendered}"
        );
        assert!(rendered.contains("app:1.0"), "{rendered}");
    }

    #[test]
    fn rendering_a_pods_containers_survives_a_tiny_terminal() {
        for (width, height) in [(1, 1), (8, 3), (20, 2), (200, 60)] {
            let mut app = app_with_pod();
            app.on_key(press(KeyCode::Enter));

            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw(frame, &app)).unwrap();
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
    fn tab_switches_focus_to_detail_then_a_second_tab_finds_nothing_to_drill_into() {
        let mut app = app();
        assert_eq!(app.focus(), Focus::Sidebar);

        app.on_key(press(KeyCode::Tab));
        assert_eq!(app.focus(), Focus::Detail);

        // The node list is still `Loading`, so there's nothing to drill
        // into yet — `Tab` stays on `Detail` rather than toggling back.
        app.on_key(press(KeyCode::Tab));
        assert_eq!(app.focus(), Focus::Detail);
        assert_eq!(app.view(), &View::Overview);
    }

    #[test]
    fn tab_drills_in_once_focus_is_already_on_the_detail_pane() {
        let mut app = app();
        app.apply_nodes(Ok(NodesFetch {
            rows: vec![node_row("worker-1")],
            usage_note: None,
        }));

        app.on_key(press(KeyCode::Tab));
        assert_eq!(app.focus(), Focus::Detail);

        app.on_key(press(KeyCode::Tab));
        assert_eq!(
            app.view(),
            &View::NodePods {
                node: "worker-1".to_owned()
            },
            "a second Tab drills in once focus is already on the detail pane"
        );
    }

    #[test]
    fn right_switches_focus_to_detail_then_drills_in() {
        let mut app = app();
        app.apply_nodes(Ok(NodesFetch {
            rows: vec![node_row("worker-1")],
            usage_note: None,
        }));

        app.on_key(press(KeyCode::Right));
        assert_eq!(app.focus(), Focus::Detail);
        assert_eq!(app.view(), &View::Overview);

        app.on_key(press(KeyCode::Right));
        assert_eq!(
            app.view(),
            &View::NodePods {
                node: "worker-1".to_owned()
            }
        );
    }

    #[test]
    fn right_and_tab_advance_the_same_way() {
        let mut via_right = app();
        let mut via_tab = app();
        for app in [&mut via_right, &mut via_tab] {
            app.apply_nodes(Ok(NodesFetch {
                rows: vec![node_row("worker-1")],
                usage_note: None,
            }));
        }

        via_right.on_key(press(KeyCode::Right));
        via_tab.on_key(press(KeyCode::Tab));
        via_right.on_key(press(KeyCode::Right));
        via_tab.on_key(press(KeyCode::Tab));

        assert_eq!(via_right.focus(), via_tab.focus());
        assert_eq!(via_right.view(), via_tab.view());
    }

    #[test]
    fn left_and_esc_retreat_the_same_way() {
        let mut via_left = app_with_node();
        let mut via_esc = app_with_node();

        via_left.on_key(press(KeyCode::Enter));
        via_esc.on_key(press(KeyCode::Enter));

        via_left.on_key(press(KeyCode::Left));
        via_esc.on_key(press(KeyCode::Esc));

        assert_eq!(via_left.focus(), via_esc.focus());
        assert_eq!(via_left.view(), via_esc.view());
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
    fn esc_backs_out_then_returns_focus_to_the_sidebar_before_arming_quit() {
        // `app_with_node` leaves `Focus::Detail`, so backing all the way out
        // to a confirmed quit takes: back out of the view (1), move focus
        // to the sidebar (1), arm (1), confirm (1).
        let mut app = app_with_node();
        app.on_key(press(KeyCode::Enter));

        assert_eq!(app.on_key(press(KeyCode::Esc)), Flow::Continue);
        assert_eq!(app.view(), &View::Overview);

        assert_eq!(
            app.on_key(press(KeyCode::Esc)),
            Flow::Continue,
            "the first Esc at Overview returns focus to the sidebar rather than quitting"
        );
        assert_eq!(app.focus(), Focus::Sidebar);

        assert_eq!(
            app.on_key(press(KeyCode::Esc)),
            Flow::Continue,
            "the next Esc arms a pending quit"
        );

        assert_eq!(app.on_key(press(KeyCode::Esc)), Flow::Quit);
    }

    #[test]
    fn q_is_a_no_op_while_drilled_into_a_node() {
        let mut app = app_with_node();
        app.on_key(press(KeyCode::Enter));
        let view_before = app.view().clone();

        assert_eq!(app.on_key(press(KeyCode::Char('q'))), Flow::Continue);
        assert_eq!(app.view(), &view_before);
    }

    #[test]
    fn enter_drills_into_the_highlighted_pods_containers() {
        let mut app = app_with_pod();

        app.on_key(press(KeyCode::Enter));

        assert_eq!(
            app.view(),
            &View::PodContainers {
                node: "worker-1".to_owned(),
                namespace: "default".to_owned(),
                pod: "api-1".to_owned(),
            }
        );
        assert_eq!(app.containers(), &ContainersState::Loading);
        assert_eq!(
            app.detail_selected(),
            0,
            "drilling in starts with nothing highlighted in the new list"
        );
    }

    #[test]
    fn enter_is_a_no_op_while_the_pod_list_is_still_loading() {
        let mut app = app_with_node();
        app.on_key(press(KeyCode::Enter));
        assert_eq!(app.pods(), &PodsState::Loading);

        app.on_key(press(KeyCode::Enter));

        assert_eq!(
            app.view(),
            &View::NodePods {
                node: "worker-1".to_owned()
            }
        );
    }

    #[test]
    fn enter_does_nothing_further_once_drilled_into_containers() {
        // There is nowhere left to go; `Enter` on a highlighted container is
        // a no-op rather than a fourth level nothing built.
        let mut app = app_with_pod();
        app.on_key(press(KeyCode::Enter));
        let view_before = app.view().clone();

        app.on_key(press(KeyCode::Enter));

        assert_eq!(app.view(), &view_before);
    }

    #[test]
    fn esc_backs_out_of_a_container_drill_down_to_the_pod_list_not_the_overview() {
        let mut app = app_with_pod();
        app.on_key(press(KeyCode::Enter));
        assert_eq!(app.containers(), &ContainersState::Loading);

        let flow = app.on_key(press(KeyCode::Esc));

        assert_eq!(flow, Flow::Continue);
        assert_eq!(
            app.view(),
            &View::NodePods {
                node: "worker-1".to_owned()
            },
            "esc backs out one level at a time, not straight to the overview"
        );
    }

    #[test]
    fn esc_from_the_pod_list_still_has_the_rows_it_fetched() {
        // Backing out of a pod's containers must not discard the pod
        // listing the reader was just looking at: there was no reason to
        // refetch it, and it did not change underneath them.
        let mut app = app_with_pod();
        app.on_key(press(KeyCode::Enter));

        app.on_key(press(KeyCode::Esc));

        assert_eq!(
            app.pods(),
            &PodsState::Loaded {
                rows: vec![pod_row("api-1")],
                selector_note: None,
            }
        );
    }

    #[test]
    fn esc_from_a_container_drill_down_needs_five_presses_to_reach_quit() {
        // Two levels of view to back out of, then the same
        // focus-then-arm-then-confirm sequence as backing out of a single
        // level (see `esc_backs_out_then_returns_focus_to_the_sidebar_before_arming_quit`).
        let mut app = app_with_pod();
        app.on_key(press(KeyCode::Enter));

        assert_eq!(app.on_key(press(KeyCode::Esc)), Flow::Continue);
        assert_eq!(
            app.view(),
            &View::NodePods {
                node: "worker-1".to_owned()
            }
        );

        assert_eq!(app.on_key(press(KeyCode::Esc)), Flow::Continue);
        assert_eq!(app.view(), &View::Overview);

        assert_eq!(app.on_key(press(KeyCode::Esc)), Flow::Continue);
        assert_eq!(app.focus(), Focus::Sidebar);

        assert_eq!(app.on_key(press(KeyCode::Esc)), Flow::Continue);

        assert_eq!(app.on_key(press(KeyCode::Esc)), Flow::Quit);
    }

    #[test]
    fn q_is_a_no_op_while_drilled_into_a_pods_containers() {
        let mut app = app_with_pod();
        app.on_key(press(KeyCode::Enter));
        let view_before = app.view().clone();

        assert_eq!(app.on_key(press(KeyCode::Char('q'))), Flow::Continue);
        assert_eq!(app.view(), &view_before);
    }

    // --- Drilling into a container's logs -----------------------------------

    #[test]
    fn enter_drills_into_the_highlighted_containers_logs() {
        let mut app = app_with_container();

        app.on_key(press(KeyCode::Enter));

        assert_eq!(
            app.view(),
            &View::ContainerLogs {
                node: "worker-1".to_owned(),
                namespace: "default".to_owned(),
                pod: "api-1".to_owned(),
                container: "app".to_owned(),
                previous: false,
            }
        );
        assert_eq!(app.logs(), &LogsState::Loading);
        assert_eq!(
            app.detail_selected(),
            0,
            "drilling in starts with nothing highlighted in the new list"
        );
    }

    #[test]
    fn enter_does_nothing_further_once_drilled_into_a_containers_logs() {
        // There is nowhere left to go; this is the deepest a reader can get.
        let mut app = app_with_container();
        app.on_key(press(KeyCode::Enter));
        let view_before = app.view().clone();

        app.on_key(press(KeyCode::Enter));

        assert_eq!(app.view(), &view_before);
    }

    #[test]
    fn esc_backs_out_of_a_logs_drill_down_to_the_container_list_not_the_pod_list() {
        let mut app = app_with_container();
        app.on_key(press(KeyCode::Enter));
        assert_eq!(app.logs(), &LogsState::Loading);

        let flow = app.on_key(press(KeyCode::Esc));

        assert_eq!(flow, Flow::Continue);
        assert_eq!(
            app.view(),
            &View::PodContainers {
                node: "worker-1".to_owned(),
                namespace: "default".to_owned(),
                pod: "api-1".to_owned(),
            },
            "esc backs out one level at a time, not straight to the pod list"
        );
    }

    #[test]
    fn q_is_a_no_op_while_drilled_into_a_containers_logs() {
        let mut app = app_with_container();
        app.on_key(press(KeyCode::Enter));
        let view_before = app.view().clone();

        assert_eq!(app.on_key(press(KeyCode::Char('q'))), Flow::Continue);
        assert_eq!(app.view(), &view_before);
    }

    #[test]
    fn apply_log_event_moves_loading_into_streaming_on_the_first_line() {
        let mut app = app();

        app.apply_log_event(LogEvent::Line("starting up".to_owned()));

        assert!(matches!(app.logs(), LogsState::Streaming(_)));
    }

    #[test]
    fn j_and_k_scroll_the_log_rather_than_moving_a_highlight() {
        let mut app = app_with_container();
        app.on_key(press(KeyCode::Enter));
        for line in 1..=5 {
            app.apply_log_event(LogEvent::Line(line.to_string()));
        }

        app.on_key(press(KeyCode::Char('k')));

        let log = streaming(app.logs());
        assert!(!log.follow(), "k must scroll the log, not a row highlight");
        assert_eq!(app.detail_selected(), 0);
    }

    #[test]
    fn f_and_w_toggle_follow_and_wrap_through_on_key() {
        let mut app = app_with_container();
        app.on_key(press(KeyCode::Enter));
        app.apply_log_event(LogEvent::Line("one".to_owned()));

        app.on_key(press(KeyCode::Char('k'))); // stop following first
        app.on_key(press(KeyCode::Char('f')));
        app.on_key(press(KeyCode::Char('w')));

        let log = streaming(app.logs());
        assert!(log.follow(), "f resumes following");
        assert!(log.wrap(), "w turns wrap on");
    }

    #[test]
    fn f_and_w_are_harmless_outside_the_logs_view() {
        let mut app = app_with_node();

        assert_eq!(app.on_key(press(KeyCode::Char('f'))), Flow::Continue);
        assert_eq!(app.on_key(press(KeyCode::Char('w'))), Flow::Continue);
        assert_eq!(app.logs(), &LogsState::Loading);
    }

    #[test]
    fn p_switches_a_restarted_containers_log_to_its_previous_instance() {
        let mut app = app_with_crashed_container();
        app.on_key(press(KeyCode::Enter));
        app.apply_log_event(LogEvent::Line("current instance's output".to_owned()));

        app.on_key(press(KeyCode::Char('p')));

        assert!(matches!(
            app.view(),
            View::ContainerLogs { previous: true, .. }
        ));
        assert_eq!(
            app.logs(),
            &LogsState::Loading,
            "switching modes starts a fresh fetch rather than keeping the old lines"
        );
    }

    #[test]
    fn p_switches_back_to_the_current_log_on_a_second_press() {
        let mut app = app_with_crashed_container();
        app.on_key(press(KeyCode::Enter));
        app.on_key(press(KeyCode::Char('p')));

        app.on_key(press(KeyCode::Char('p')));

        assert!(matches!(
            app.view(),
            View::ContainerLogs {
                previous: false,
                ..
            }
        ));
        assert_eq!(app.logs(), &LogsState::Loading);
    }

    #[test]
    fn p_on_a_never_restarted_container_says_so_rather_than_fetching() {
        let mut app = app_with_container(); // restarts: 0
        app.on_key(press(KeyCode::Enter));

        app.on_key(press(KeyCode::Char('p')));

        assert!(
            matches!(app.logs(), LogsState::Unavailable(message) if message.contains("never restarted")),
            "{:?}",
            app.logs()
        );
    }

    #[test]
    fn p_recovers_from_the_no_previous_log_message_on_a_second_press() {
        // The refusal must not be a dead end: a second `p` has to be able to
        // undo the first, the same as it does when there was a previous log
        // to switch to.
        let mut app = app_with_container();
        app.on_key(press(KeyCode::Enter));
        app.on_key(press(KeyCode::Char('p')));
        assert!(matches!(app.logs(), LogsState::Unavailable(_)));

        app.on_key(press(KeyCode::Char('p')));

        assert!(matches!(
            app.view(),
            View::ContainerLogs {
                previous: false,
                ..
            }
        ));
        assert_eq!(app.logs(), &LogsState::Loading);
    }

    #[test]
    fn p_is_harmless_outside_the_logs_view() {
        let mut app = app_with_node();

        assert_eq!(app.on_key(press(KeyCode::Char('p'))), Flow::Continue);
        assert_eq!(app.view(), &View::Overview);
    }

    #[test]
    fn a_frame_drilled_into_a_containers_logs_carries_the_full_breadcrumb() {
        let mut app = app_with_container();
        app.on_key(press(KeyCode::Enter));
        app.apply_log_event(LogEvent::Line("listening on :8080".to_owned()));

        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();

        let rendered = terminal.backend().to_string();
        assert!(
            rendered.contains("Overview › worker-1 › api-1 › app"),
            "{rendered}"
        );
        assert!(rendered.contains("listening on :8080"), "{rendered}");
    }

    #[test]
    fn rendering_a_containers_logs_survives_a_tiny_terminal() {
        for (width, height) in [(1, 1), (8, 3), (20, 2), (200, 60)] {
            let mut app = app_with_container();
            app.on_key(press(KeyCode::Enter));

            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw(frame, &app)).unwrap();
        }
    }

    #[test]
    fn leave_detail_view_also_discards_a_drill_into_logs() {
        let mut app = app_with_container();
        app.on_key(press(KeyCode::Enter));
        app.apply_log_event(LogEvent::Line("hello".to_owned()));

        app.leave_detail_view();

        assert_eq!(app.view(), &View::Overview);
        assert_eq!(app.logs(), &LogsState::Loading);
    }

    #[test]
    fn apply_containers_moves_a_success_into_the_loaded_state() {
        let mut app = app();

        app.apply_containers(Ok(ContainersFetch::default()));

        assert_eq!(
            app.containers(),
            &ContainersState::Loaded {
                rows: Vec::new(),
                ip: String::new(),
                nominated_node: String::new(),
                readiness_gates: None,
            }
        );
    }

    #[test]
    fn apply_containers_moves_a_failure_into_the_error_state_even_after_a_success() {
        // Unlike the node pane, and like the pod pane: this fetches once per
        // pod rather than refreshing in the background, so there is no
        // earlier good listing for *this* pod worth keeping over a failed
        // one.
        let mut app = app();
        app.apply_containers(Ok(ContainersFetch::default()));

        app.apply_containers(Err(failed("could not get pod")));

        assert_eq!(
            app.containers(),
            &ContainersState::Error("could not get pod".to_owned())
        );
    }

    #[test]
    fn leave_detail_view_resets_the_view_and_the_pods_pane() {
        let mut app = app_with_node();
        app.on_key(press(KeyCode::Enter));

        app.leave_detail_view();

        assert_eq!(app.view(), &View::Overview);
        assert_eq!(app.pods(), &PodsState::Loading);
        assert_eq!(app.detail_selected(), 0);
    }

    #[test]
    fn leave_detail_view_also_discards_a_deeper_drill_into_containers() {
        let mut app = app_with_node();
        app.on_key(press(KeyCode::Enter));
        app.apply_pods(Ok(PodsFetch {
            rows: vec![pod_row("api-1")],
            selector_note: None,
        }));
        app.on_key(press(KeyCode::Enter));
        assert_eq!(app.containers(), &ContainersState::Loading);

        app.leave_detail_view();

        assert_eq!(app.view(), &View::Overview);
        assert_eq!(app.containers(), &ContainersState::Loading);
        assert_eq!(app.detail_selected(), 0);
    }

    #[test]
    fn apply_pods_moves_a_success_into_the_loaded_state() {
        let mut app = app();

        app.apply_pods(Ok(PodsFetch::default()));

        assert_eq!(
            app.pods(),
            &PodsState::Loaded {
                rows: Vec::new(),
                selector_note: None,
            }
        );
    }

    #[test]
    fn apply_pods_carries_the_selector_note_into_the_loaded_state() {
        let mut app = app();

        app.apply_pods(Ok(PodsFetch {
            rows: Vec::new(),
            selector_note: Some("label selector `app=api`".to_owned()),
        }));

        assert_eq!(
            app.pods(),
            &PodsState::Loaded {
                rows: Vec::new(),
                selector_note: Some("label selector `app=api`".to_owned()),
            }
        );
    }

    #[test]
    fn apply_pods_moves_a_failure_into_the_error_state_even_after_a_success() {
        // Unlike the node pane, the pod pane fetches once per node rather
        // than refreshing in the background, so there is no earlier good
        // listing for *this* node worth keeping over a failed one.
        let mut app = app();
        app.apply_pods(Ok(PodsFetch::default()));

        app.apply_pods(Err(failed("could not list pods")));

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

    #[test]
    fn a_new_app_opens_on_the_default_ordering_for_both_panes() {
        let app = app();
        assert_eq!(app.node_order(), k8s_nodes::Order::default());
        assert_eq!(app.node_direction(), SortDirection::default());
        assert_eq!(app.pod_order(), k8s_pods::Order::default());
        assert_eq!(app.pod_direction(), SortDirection::default());
    }

    #[test]
    fn s_cycles_the_node_panes_ordering() {
        let mut app = app();

        app.on_key(press(KeyCode::Char('s')));
        assert_eq!(app.node_order(), k8s_nodes::Order::Status);

        app.on_key(press(KeyCode::Char('s')));
        assert_eq!(app.node_order(), k8s_nodes::Order::Cpu);
    }

    #[test]
    fn cycling_sort_all_the_way_round_returns_to_the_default() {
        let mut app = app();

        for _ in 0..k8s_nodes::Order::value_variants().len() {
            app.on_key(press(KeyCode::Char('s')));
        }

        assert_eq!(app.node_order(), k8s_nodes::Order::default());
    }

    #[test]
    fn shift_s_reverses_the_active_direction() {
        let mut app = app();

        app.on_key(press(KeyCode::Char('S')));
        assert_eq!(app.node_direction(), SortDirection::Reversed);

        app.on_key(press(KeyCode::Char('S')));
        assert_eq!(app.node_direction(), SortDirection::Natural);
    }

    #[test]
    fn sorting_re_orders_already_loaded_rows_without_a_new_fetch() {
        let mut app = app();
        app.apply_nodes(Ok(NodesFetch {
            rows: vec![
                node_row_with_cpu("idle", "100m", "4"),
                node_row_with_cpu("busy", "3800m", "4"),
            ],
            usage_note: None,
        }));

        app.on_key(press(KeyCode::Char('s'))); // Status
        app.on_key(press(KeyCode::Char('s'))); // Cpu

        let names: Vec<&str> = app
            .nodes()
            .rows()
            .iter()
            .map(|row| row.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["busy", "idle"],
            "no fetch happened; the rows already on screen were re-sorted in place"
        );
    }

    #[test]
    fn a_freshly_loaded_pane_opens_already_sorted_by_the_active_ordering() {
        let mut app = app();
        app.on_key(press(KeyCode::Char('s'))); // Status
        app.on_key(press(KeyCode::Char('s'))); // Cpu

        app.apply_nodes(Ok(NodesFetch {
            rows: vec![
                node_row_with_cpu("idle", "100m", "4"),
                node_row_with_cpu("busy", "3800m", "4"),
            ],
            usage_note: None,
        }));

        let names: Vec<&str> = app
            .nodes()
            .rows()
            .iter()
            .map(|row| row.name.as_str())
            .collect();
        assert_eq!(names, ["busy", "idle"]);
    }

    #[test]
    fn sort_and_reverse_act_on_whichever_pane_the_view_is_currently_showing() {
        let mut app = app_with_node();
        app.on_key(press(KeyCode::Enter));
        assert!(matches!(app.view(), View::NodePods { .. }));

        app.on_key(press(KeyCode::Char('s')));

        assert_eq!(app.pod_order(), k8s_pods::Order::Restarts);
        assert_eq!(
            app.node_order(),
            k8s_nodes::Order::default(),
            "the node pane's ordering must not move while a different pane is showing"
        );
    }

    #[test]
    fn sort_and_reverse_are_harmless_while_a_pods_containers_are_showing() {
        // No ordering exists for this pane yet; `s`/`S` must not panic, and
        // must not leak into the other two panes' orderings either.
        let mut app = app_with_pod();
        app.on_key(press(KeyCode::Enter));
        assert!(matches!(app.view(), View::PodContainers { .. }));

        app.on_key(press(KeyCode::Char('s')));
        app.on_key(press(KeyCode::Char('S')));

        assert_eq!(app.node_order(), k8s_nodes::Order::default());
        assert_eq!(app.pod_order(), k8s_pods::Order::default());
    }

    #[test]
    fn changing_the_node_panes_order_is_visible_in_the_rendered_frame() {
        let mut app = app();
        app.apply_nodes(Ok(NodesFetch {
            rows: vec![node_row("worker-1")],
            usage_note: None,
        }));

        app.on_key(press(KeyCode::Char('s'))); // Status
        app.on_key(press(KeyCode::Char('s'))); // Cpu

        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();

        assert!(
            terminal.backend().to_string().contains("Sorted by cpu."),
            "{}",
            terminal.backend().to_string()
        );
    }

    #[test]
    fn slash_opens_the_filter_and_switches_focus_to_the_detail_pane() {
        let mut app = app();
        assert_eq!(app.focus(), Focus::Sidebar);

        app.on_key(press(KeyCode::Char('/')));

        assert_eq!(app.focus(), Focus::Detail);
        assert!(app.is_filtering());
    }

    #[test]
    fn typing_while_editing_captures_navigation_letters_as_query_text() {
        let mut app = app_with_two_nodes();

        app.on_key(press(KeyCode::Char('/')));
        app.on_key(press(KeyCode::Char('j'))); // would otherwise move the highlight

        assert_eq!(app.filter_query(), "j");
        assert_eq!(
            app.detail_selected(),
            0,
            "a keystroke while editing resets the highlight rather than moving it"
        );
    }

    #[test]
    fn backspace_removes_the_last_character_of_the_query() {
        let mut app = app_with_two_nodes();

        app.on_key(press(KeyCode::Char('/')));
        app.on_key(press(KeyCode::Char('w')));
        app.on_key(press(KeyCode::Char('x')));
        app.on_key(press(KeyCode::Backspace));

        assert_eq!(app.filter_query(), "w");
    }

    #[test]
    fn esc_while_editing_cancels_the_filter_entirely() {
        let mut app = app_with_two_nodes();

        app.on_key(press(KeyCode::Char('/')));
        app.on_key(press(KeyCode::Char('w')));
        app.on_key(press(KeyCode::Esc));

        assert!(!app.is_filtering());
        assert_eq!(app.filter_query(), "");
    }

    #[test]
    fn committing_an_empty_query_leaves_the_filter_inactive() {
        let mut app = app_with_two_nodes();

        app.on_key(press(KeyCode::Char('/')));
        app.on_key(press(KeyCode::Enter));

        assert!(!app.is_filtering());
        assert_eq!(app.filter_query(), "");
    }

    #[test]
    fn after_committing_a_filter_normal_keys_resume_their_usual_meaning() {
        let mut app = app_with_two_nodes();

        app.on_key(press(KeyCode::Char('/')));
        app.on_key(press(KeyCode::Char('w'))); // matches both rows
        app.on_key(press(KeyCode::Enter));
        assert!(!app.is_filtering());

        app.on_key(press(KeyCode::Char('j')));

        assert_eq!(
            app.detail_selected(),
            1,
            "j moves the highlight again rather than editing the query"
        );
    }

    #[test]
    fn pressing_slash_again_reopens_editing_with_the_existing_query() {
        let mut app = app_with_two_nodes();
        app.on_key(press(KeyCode::Char('/')));
        app.on_key(press(KeyCode::Char('w')));
        app.on_key(press(KeyCode::Enter));

        app.on_key(press(KeyCode::Char('/')));

        assert!(app.is_filtering());
        assert_eq!(app.filter_query(), "w");
    }

    #[test]
    fn the_filter_narrows_which_row_enter_drills_into() {
        let mut app = app_with_two_nodes();

        app.on_key(press(KeyCode::Char('/')));
        for c in "worker-2".chars() {
            app.on_key(press(KeyCode::Char(c)));
        }
        app.on_key(press(KeyCode::Enter)); // commits the filter
        app.on_key(press(KeyCode::Enter)); // drills into the sole match

        assert_eq!(
            app.view(),
            &View::NodePods {
                node: "worker-2".to_owned()
            }
        );
    }

    #[test]
    fn drilling_in_resets_the_filter() {
        let mut app = app_with_two_nodes();
        app.on_key(press(KeyCode::Char('/')));
        app.on_key(press(KeyCode::Char('w')));
        app.on_key(press(KeyCode::Enter));
        assert_eq!(app.filter_query(), "w");

        app.on_key(press(KeyCode::Enter)); // drills in

        assert_eq!(
            app.filter_query(),
            "",
            "a freshly drilled-into view starts with no filter"
        );
    }

    #[test]
    fn leaving_the_detail_view_clears_the_filter() {
        let mut app = app_with_two_nodes();
        app.on_key(press(KeyCode::Char('/')));
        app.on_key(press(KeyCode::Char('w')));
        app.on_key(press(KeyCode::Enter));
        assert_eq!(app.filter_query(), "w");

        app.leave_detail_view();

        assert_eq!(app.filter_query(), "");
    }

    #[test]
    fn slash_is_a_no_op_on_the_container_logs_pane() {
        let mut app = app_with_container();
        app.on_key(press(KeyCode::Enter)); // drills into the container's log
        assert!(matches!(app.view(), View::ContainerLogs { .. }));

        app.on_key(press(KeyCode::Char('/')));

        assert!(!app.is_filtering());
        assert_eq!(app.filter_query(), "");
    }

    #[test]
    fn esc_clears_an_applied_filter_before_backing_out_of_a_drill_down() {
        let mut app = app_with_two_nodes();
        app.on_key(press(KeyCode::Enter)); // drills into a node's pods
        assert!(matches!(app.view(), View::NodePods { .. }));

        app.on_key(press(KeyCode::Char('/')));
        app.on_key(press(KeyCode::Char('a')));
        app.on_key(press(KeyCode::Enter));
        assert_eq!(app.filter_query(), "a");

        app.on_key(press(KeyCode::Esc));

        assert_eq!(app.filter_query(), "", "the first Esc clears the filter");
        assert!(
            matches!(app.view(), View::NodePods { .. }),
            "the drill-down must still be showing after only clearing the filter"
        );

        app.on_key(press(KeyCode::Esc));

        assert_eq!(
            app.view(),
            &View::Overview,
            "the second Esc backs out as usual"
        );
    }

    #[test]
    fn a_quit_key_while_editing_the_filter_is_added_to_the_query_instead_of_arming_a_quit() {
        let mut app = app_with_two_nodes();

        app.on_key(press(KeyCode::Char('/')));
        app.on_key(press(KeyCode::Char('q')));

        assert_eq!(app.filter_query(), "q");
        assert!(!app.quit_pending());
    }

    #[test]
    fn ctrl_c_still_quits_immediately_while_editing_the_filter() {
        let mut app = app_with_two_nodes();
        app.on_key(press(KeyCode::Char('/')));

        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Flow::Quit
        );
    }
}
