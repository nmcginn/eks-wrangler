//! The interactive dashboard.
//!
//! State and input handling live in [`App`], deliberately free of any terminal
//! I/O, so navigation can be tested by feeding it key events. Only [`run`]
//! touches the real terminal.

use std::collections::VecDeque;
use std::io::Write;
use std::str::FromStr;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::ValueEnum;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::{execute, terminal};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Padding, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::cluster::ClusterView;
use crate::commands::exec::{Plan, Target as ExecTarget};
use crate::commands::nodes::NodesFetch;
use crate::commands::pods::{ContainersFetch, PodsFetch, selectors_for};
use crate::commands::{FetchError, StreamHandle};
use crate::k8s::forward::{Event as ForwardEvent, PodPort};
use crate::k8s::nodes as k8s_nodes;
use crate::k8s::order::Direction as SortDirection;
use crate::k8s::page::{Budget, ParseError};
use crate::k8s::pods as k8s_pods;
use crate::k8s::pods::{LogEvent, Selectors};
use crate::theme::{Background, Theme};

mod background;
mod containers;
mod forwards;
mod logs;
mod nodes;
mod pods;

use background::ReplyReader;
use containers::{ContainersState, Entry};
pub use forwards::{Forward, ForwardRequest, Forwards};
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

/// Starts a fetch of the pods on one node of one cluster, filtered by
/// whichever `-l`/`--field-selector` `App::pod_selectors` currently holds —
/// the flags the process started with until `l`/`F` retypes one of them, so a
/// fetch started after a commit asks the cluster the new question rather than
/// the one the session began with.
pub type PodsFetcher =
    Box<dyn Fn(&str, &str, &Selectors) -> mpsc::Receiver<Result<PodsFetch, FetchError>>>;

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

/// Checks, on a background thread, that a shell can be opened in a container
/// of the named context, and plans the session if so — see
/// [`crate::commands::exec::spawn_prepare`]. A refusal is a sentence for the
/// status line.
pub type ExecPreparer = Box<dyn Fn(&str, &ExecTarget) -> mpsc::Receiver<Result<Plan, FetchError>>>;

/// Runs a planned shell session to its end, on this thread and the real
/// terminal, which [`run`] has handed back before calling it — the same
/// arrangement as [`LoginRunner`]. The `Err` is a sentence for the status
/// line: the session could not start, or broke partway.
pub type SessionRunner = Box<dyn Fn(&str, &Plan) -> Result<(), FetchError>>;

/// Starts forwarding one port of one pod of the named context, on a thread of
/// its own, until the returned [`StreamHandle`] is dropped — see
/// [`crate::commands::forward::spawn_dashboard`]. Like [`LogsFetcher`]'s, the
/// handle is the forward: dropping it closes the port.
pub type ForwardStarter =
    Box<dyn Fn(&str, &PodPort) -> (mpsc::Receiver<ForwardEvent>, StreamHandle)>;

/// [`SessionRunner`] with the terminal handed back around it, as
/// `event_loop` sees it. Borrowed for the reason [`Suspended`] is.
type SuspendedSession<'a> = &'a dyn Fn(&str, &Plan) -> Result<(), FetchError>;

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Flow {
    /// Stay in the loop.
    Continue,
    /// Give the terminal back, log in to AWS, take it again, and refetch.
    ///
    /// Its own variant rather than something [`App`] could do itself, for the
    /// reason [`Quit`](Self::Quit) is one: the state machine decides, and the
    /// event loop — the only thing here that owns a terminal — acts.
    Login,
    /// Check, off the render thread, that a shell can be opened in this
    /// container. If one can, give the terminal to it until it exits, then
    /// take the terminal back and redraw. If not, say why in the status line.
    ///
    /// A variant for the reason [`Login`](Self::Login) is one: running the
    /// session means owning the terminal, and only the event loop does.
    Exec(ExecTarget),
    /// Start this forward.
    ///
    /// It runs on a thread the event loop holds the handle of, and is
    /// stopped by [`App`] no longer wanting it (see [`Forwards::wants`]),
    /// which the event loop checks before every frame, so stopping needs no
    /// variant of its own.
    Forward(ForwardRequest),
    /// Tear down and exit.
    Quit,
}

/// What the status line above the footer says: what `x` is doing, or a note
/// about the key just pressed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum StatusLine {
    /// Nothing; the status line is not drawn.
    #[default]
    Idle,
    /// The checks behind `x` are running for this container. `Esc` cancels.
    Preparing(ExecTarget),
    /// Why a shell could not be opened, or why one ended badly. Shown until
    /// the next key.
    Refused(String),
    /// Advice about a key that had nothing to act on as pressed — `f` on a
    /// container rather than a port, or on a port already forwarded. Not a
    /// failure, so not drawn as one; shown until the next key, like a
    /// refusal.
    Note(String),
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

/// A `--sort-resource` prompt: the node pane's own, in [`App::node_resource_sort`],
/// and the pod-drilldown pane's own copy in [`App::pod_resource_sort`] — the
/// two panes never share a screen, so each holds its own rather than one
/// shared between rows of two different shapes.
///
/// A free-form counterpart to `s`/`S`'s fixed cycle over an `Order`, mirroring
/// [`Filter`]'s life cycle for the same reason: a device's name is not known
/// until the rows have been fetched, so it has to be typed rather than picked
/// from a `clap::ValueEnum` the way `--sort` is (see `k8s::nodes::order`'s
/// module docs). `Editing` captures every keystroke as
/// text; `Enter` applies it, collapsing an empty query back to `Inactive`
/// rather than leaving an `Applied("")` with nothing to rank by; `Esc`
/// cancels outright. Applying or clearing it, unlike a filter, changes what
/// the pane's rows are actually sorted by, so both are followed by a re-sort
/// rather than only a redraw.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum ResourceSort {
    /// No `--sort-resource` prompt has ever been applied; the fixed `s`/`S`
    /// cycle is in charge. The common case, costing nothing extra to draw.
    #[default]
    Inactive,
    /// The resource name is being typed.
    Editing(String),
    /// The name was committed with `Enter` and governs the pane's ordering
    /// until `s`, an empty commit, or an `Esc` clears it.
    Applied(String),
}

impl ResourceSort {
    /// The text to seed a fresh edit with — whatever was last applied, or
    /// empty when nothing was.
    fn query(&self) -> &str {
        match self {
            Self::Inactive => "",
            Self::Editing(text) | Self::Applied(text) => text,
        }
    }

    fn is_editing(&self) -> bool {
        matches!(self, Self::Editing(_))
    }

    /// The applied resource name, or `None` while inactive or still being
    /// typed — a prompt is not a `--sort-resource` until `Enter` commits it.
    fn applied(&self) -> Option<&str> {
        match self {
            Self::Applied(text) => Some(text),
            Self::Inactive | Self::Editing(_) => None,
        }
    }
}

/// The `l`/`F` prompt that retypes one of the pod-drilldown pane's two
/// selectors — the dashboard's own `-l`/`--field-selector` — without
/// restarting it. [`ResourceSort`]'s counterpart for a value that can be
/// rejected: `Editing` captures every keystroke as selector text; `Enter`
/// revalidates it through [`selectors_for`], the same function and rejection
/// wording `eks pods` and dashboard startup already share, and either commits
/// the canonical result to [`App::pod_selectors`] and returns to `Inactive`,
/// or stays `Editing` with `error` set to the rejection's own sentence, so
/// the offending text is not lost. `Esc` cancels outright, leaving
/// `pod_selectors` exactly as it was. Only one of the two selectors can be
/// mid-edit at a time — `l`/`F` each open their own text seeded from what is
/// currently applied, the same seeding `ResourceSort::query` gives its own
/// prompt.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum SelectorEdit {
    #[default]
    Inactive,
    Editing {
        field: pods::SelectorField,
        text: String,
        /// The last rejection [`selectors_for`] gave this text, if any —
        /// cleared on the next keystroke so a fixed selector does not still
        /// show the old complaint.
        error: Option<String>,
    },
}

impl SelectorEdit {
    fn is_editing(&self) -> bool {
        matches!(self, Self::Editing { .. })
    }
}

/// Dashboard state.
#[derive(Debug, Clone)]
pub struct App {
    clusters: Vec<ClusterView>,
    selected: usize,
    theme: Theme,
    /// See [`App::set_asks_terminal_background`].
    asks_terminal_background: bool,
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
    /// The node pane's `--sort-resource` prompt — `R` in place of `s`'s fixed
    /// cycle. `node_direction` still governs its reversal: the two flags
    /// share one direction on the command line, and the pane follows suit
    /// rather than tracking a second one nothing composes with.
    node_resource_sort: ResourceSort,
    /// The pod-drilldown pane's ordering, independent of the node pane's:
    /// the two panes hold different rows and `s`/`S` act on whichever one
    /// [`View`] is currently showing.
    pod_order: k8s_pods::Order,
    pod_direction: SortDirection,
    /// The pod-drilldown pane's `--sort-resource` prompt — [`Self::node_resource_sort`]'s
    /// own copy for this pane, independent for the same reason `pod_order`
    /// is: the two panes never share a screen, and a device ordering typed
    /// against one node's pods has nothing to say about the next one drilled
    /// into.
    pod_resource_sort: ResourceSort,
    /// The dashboard's own `-l`/`--field-selector`, seeded from the flags the
    /// process started with (`Self::set_pod_selectors`) and retypeable at
    /// runtime through `l`/`F` (`Self::pod_selector_edit`). Read by every
    /// pod-drilldown fetch — [`PodsFetcher`]'s third argument — so a fetch
    /// started after a commit asks the new question rather than the one the
    /// session began with.
    pod_selectors: Selectors,
    /// The `l`/`F` prompt retyping one of the selectors above, if either is
    /// being retyped right now.
    pod_selector_edit: SelectorEdit,
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
    /// What the status line says. The check behind `x` lives on the event
    /// loop, which drops it as soon as this stops being
    /// [`StatusLine::Preparing`].
    status: StatusLine,
    /// The port forwards started with `f`. Not reset by anything that resets
    /// a pane: a forward runs for as long as the dashboard is open.
    forwards: Forwards,
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
            asks_terminal_background: false,
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
            node_resource_sort: ResourceSort::default(),
            pod_order: k8s_pods::Order::default(),
            pod_direction: SortDirection::default(),
            pod_resource_sort: ResourceSort::default(),
            pod_selectors: Selectors::default(),
            pod_selector_edit: SelectorEdit::default(),
            quit_armed_at: None,
            credentials_lost: false,
            status: StatusLine::Idle,
            forwards: Forwards::default(),
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

    /// The port forwards this dashboard has started and not dismissed.
    #[must_use]
    pub fn forwards(&self) -> &Forwards {
        &self.forwards
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

    /// [`Self::is_filtering`]'s counterpart for the container-logs pane's
    /// own `/` search.
    #[must_use]
    pub fn is_searching_log(&self) -> bool {
        self.logs.is_search_editing()
    }

    /// Whether the container-logs pane has a committed search for `n`/`N` to
    /// step through — offered in the footer only then, the same "only offer
    /// a key where it does something" rule `R`'s hint already follows.
    #[must_use]
    pub fn log_search_active(&self) -> bool {
        self.logs.is_search_applied()
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

    /// The node pane's active ordering: the applied `--sort-resource`
    /// prompt, if there is one, else the fixed `s`/`S` cycle — mirrors
    /// `commands::nodes::SortBy`, which resolves the same two flags the same
    /// way on the command line.
    #[must_use]
    fn node_sort(&self) -> nodes::Sort<'_> {
        match self.node_resource_sort.applied() {
            Some(resource) => nodes::Sort::Resource(resource),
            None => nodes::Sort::Order(self.node_order),
        }
    }

    /// The text currently being typed into the node pane's `--sort-resource`
    /// prompt, or `None` when nothing is being edited right now — distinct
    /// from [`Self::node_sort`]'s `Resource` case, which only appears once
    /// `Enter` commits this.
    #[must_use]
    fn node_resource_prompt(&self) -> Option<&str> {
        match &self.node_resource_sort {
            ResourceSort::Editing(text) => Some(text.as_str()),
            ResourceSort::Inactive | ResourceSort::Applied(_) => None,
        }
    }

    /// Whether either pane's `R` prompt is currently capturing keystrokes,
    /// for the footer's hints — the resource-sort counterpart to
    /// [`Self::is_filtering`]. Only one can ever be editing at once: `R`
    /// opens exactly one of the two for a given [`View`].
    #[must_use]
    pub fn is_typing_resource_sort(&self) -> bool {
        self.node_resource_sort.is_editing() || self.pod_resource_sort.is_editing()
    }

    /// The pod-drilldown pane's current ordering.
    #[must_use]
    pub fn pod_order(&self) -> k8s_pods::Order {
        self.pod_order
    }

    /// The pod-drilldown pane's active ordering: the applied
    /// `--sort-resource` prompt, if there is one, else the fixed `s`/`S`
    /// cycle — [`Self::node_sort`]'s counterpart for this pane.
    #[must_use]
    fn pod_sort(&self) -> pods::Sort<'_> {
        match self.pod_resource_sort.applied() {
            Some(resource) => pods::Sort::Resource(resource),
            None => pods::Sort::Order(self.pod_order),
        }
    }

    /// The text currently being typed into the pod-drilldown pane's
    /// `--sort-resource` prompt — [`Self::node_resource_prompt`]'s
    /// counterpart for this pane.
    #[must_use]
    fn pod_resource_prompt(&self) -> Option<&str> {
        match &self.pod_resource_sort {
            ResourceSort::Editing(text) => Some(text.as_str()),
            ResourceSort::Inactive | ResourceSort::Applied(_) => None,
        }
    }

    /// The pod-drilldown pane's current direction.
    #[must_use]
    pub fn pod_direction(&self) -> SortDirection {
        self.pod_direction
    }

    /// The dashboard's own `-l`/`--field-selector`, as every pod-drilldown
    /// fetch reads it right now.
    #[must_use]
    fn pod_selectors(&self) -> &Selectors {
        &self.pod_selectors
    }

    /// The pod-drilldown pane's `l`/`F` prompt, mid-edit — `None` once
    /// nothing is being retyped right now, whether or not a selector is
    /// applied.
    #[must_use]
    fn pod_selector_prompt(&self) -> Option<pods::SelectorPrompt<'_>> {
        match &self.pod_selector_edit {
            SelectorEdit::Editing { field, text, error } => Some(pods::SelectorPrompt {
                field: *field,
                text: text.as_str(),
                error: error.as_deref(),
            }),
            SelectorEdit::Inactive => None,
        }
    }

    /// Whether the `l`/`F` prompt is currently capturing keystrokes, for the
    /// footer's hints — the selector counterpart to [`Self::is_filtering`].
    #[must_use]
    pub fn is_typing_selector(&self) -> bool {
        self.pod_selector_edit.is_editing()
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
        self.credentials_lost = result.as_ref().is_err_and(|error| error.credentials);
        self.nodes = match (result, std::mem::take(&mut self.nodes)) {
            (Ok(fetch), _) => NodesState::Loaded {
                rows: fetch.rows,
                usage_note: fetch.usage_note,
                requests_note: fetch.requests_note,
                refresh_error: None,
            },
            (
                Err(error),
                NodesState::Loaded {
                    rows,
                    usage_note,
                    requests_note,
                    ..
                },
            ) => NodesState::Loaded {
                rows,
                usage_note,
                requests_note,
                refresh_error: Some(error.message),
            },
            (Err(error), _) => NodesState::Error(error.message),
        };
        self.sort_nodes();
    }

    /// Re-sort the node pane's already-fetched rows under whichever ordering
    /// is current: the applied `--sort-resource` prompt, if there is one,
    /// else the fixed `s`/`S` cycle. Called after anything changes what
    /// "current" means — a fetch landing, `s`, `S`, or the prompt being
    /// applied or cleared — never as a fetch of its own, the same rule
    /// [`Self::cycle_sort`] already followed.
    fn sort_nodes(&mut self) {
        let NodesState::Loaded { rows, .. } = &mut self.nodes else {
            return;
        };
        match self.node_resource_sort.applied() {
            Some(resource) => k8s_nodes::sort_by_device(rows, resource, self.node_direction),
            None => k8s_nodes::sort(rows, self.node_order, self.node_direction),
        }
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
            .then_some("Press L to sign in again and retry.")
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
                rows,
                usage_note,
                requests_note,
                ..
            } => NodesState::Loaded {
                rows,
                usage_note,
                requests_note,
                refresh_error: Some(message),
            },
            _ => NodesState::Error(message),
        };
    }

    /// Whether the checks behind `x` are running. The event loop keeps the
    /// pending answer only while this holds, so anything that clears it —
    /// `Esc`, leaving the pane, another cluster — is also what cancels it.
    #[must_use]
    pub fn exec_preparing(&self) -> bool {
        matches!(self.status, StatusLine::Preparing(_))
    }

    /// The checks behind `x` came back with a reason no shell can be opened,
    /// or a session ended without its shell exiting.
    ///
    /// It goes on the status line rather than into a pane: the pane is still
    /// right about everything it shows. A credential refusal also offers `L`,
    /// as from any pane. Nothing else changes the offer, which belongs to
    /// whichever pane made it.
    pub fn apply_exec_refusal(&mut self, error: FetchError) {
        if error.credentials {
            self.credentials_lost = true;
        }
        self.status = StatusLine::Refused(error.message);
    }

    /// A session the dashboard handed the terminal to has ended — the shell
    /// exited, however it exited — or could not run. The screen is left as it
    /// was before `x`.
    pub fn apply_exec_ending(&mut self, outcome: Result<(), FetchError>) {
        match outcome {
            Ok(()) => self.status = StatusLine::Idle,
            Err(error) => self.apply_exec_refusal(error),
        }
    }

    /// The container `x` opens a shell in, from wherever the detail pane is:
    /// the highlighted container, the highlighted pod's default container,
    /// or the container whose log is open.
    ///
    /// `None` where `x` means nothing — the node pane, or a list with nothing
    /// highlighted yet. `Err` where it would mean something once the
    /// highlight is visible, which is only while the detail pane has focus.
    fn exec_target(&self) -> Option<Result<ExecTarget, String>> {
        let focused = self.focus == Focus::Detail;
        match &self.view {
            View::Overview => None,
            View::NodePods { .. } if !focused => Some(Err(
                "Press tab to move to the pod list, then x on a pod to open a shell in it."
                    .to_owned(),
            )),
            View::PodContainers { .. } if !focused => Some(Err(
                "Press tab to move to the container list, then x on a container to open a shell in it."
                    .to_owned(),
            )),
            View::NodePods { .. } => {
                let pod = self.visible_pods().get(self.detail_selected).copied()?;
                Some(Ok(ExecTarget {
                    namespace: pod.namespace.clone(),
                    pod: pod.name.clone(),
                    container: None,
                }))
            }
            View::PodContainers { namespace, pod, .. } => {
                let container = self.highlighted_entry()?.container();
                Some(Ok(ExecTarget {
                    namespace: namespace.clone(),
                    pod: pod.clone(),
                    container: Some(container.name.clone()),
                }))
            }
            View::ContainerLogs {
                namespace,
                pod,
                container,
                ..
            } => Some(Ok(ExecTarget {
                namespace: namespace.clone(),
                pod: pod.clone(),
                container: Some(container.clone()),
            })),
        }
    }

    /// `x`: start the checks for a shell, or say why there is nothing to
    /// check yet.
    fn start_exec(&mut self) -> Flow {
        match self.exec_target() {
            Some(Ok(target)) => {
                self.status = StatusLine::Preparing(target.clone());
                Flow::Exec(target)
            }
            Some(Err(advice)) => {
                self.status = StatusLine::Refused(advice);
                Flow::Continue
            }
            None => Flow::Continue,
        }
    }

    /// `f`/`F`, which mean something different in each pane that has them:
    /// forward and stop a port among a pod's containers, follow a log, and
    /// (`F` alone) retype the pod list's field selector.
    fn f_key(&mut self, key: char) -> Flow {
        match (key, &self.view) {
            ('f', View::PodContainers { .. }) => return self.forward_highlighted(),
            (_, View::PodContainers { .. }) => return self.stop_highlighted(),
            ('f', _) => self.toggle_log_follow(),
            _ => self.start_selector_edit(pods::SelectorField::Field),
        }
        Flow::Continue
    }

    /// `f` in the pod-containers pane: forward the highlighted port, or say
    /// what to do instead.
    ///
    /// Only a port can be forwarded, so every other place the highlight can
    /// be gets a sentence on the status line naming what would work, rather
    /// than a key that silently does nothing.
    fn forward_highlighted(&mut self) -> Flow {
        let View::PodContainers { namespace, pod, .. } = &self.view else {
            return Flow::Continue;
        };
        if self.focus != Focus::Detail {
            return self
                .note("Press tab to move to the container list, then f on a port to forward it.");
        }
        let (namespace, pod) = (namespace.clone(), pod.clone());
        let advice = match self.highlighted_entry() {
            None => return Flow::Continue,
            Some(Entry::Container(row)) if row.ports.is_empty() => format!(
                "Container {} declares no ports. If it listens on one anyway, \
                 `eks port-forward {pod} -n {namespace} PORT` forwards it.",
                row.name
            ),
            Some(Entry::Container(row)) => format!(
                "Move down to one of {}'s ports, then press f to forward it.",
                row.name
            ),
            Some(Entry::Port(_, port)) if !port.is_tcp() => format!(
                "Port {} is {}, and a port forward carries TCP only.",
                port.number, port.protocol
            ),
            Some(Entry::Port(_, port)) => {
                let number = port.number;
                let Some(cluster) = self.selected_cluster() else {
                    return Flow::Continue;
                };
                let (context, label) = (cluster.context_name.clone(), cluster.display_name.clone());
                let target = PodPort {
                    namespace,
                    pod: pod.clone(),
                    port: number,
                };
                match self.forwards.start(&context, &label, target.clone()) {
                    forwards::Started::New(id) => {
                        return Flow::Forward(ForwardRequest {
                            id,
                            context,
                            target,
                        });
                    }
                    forwards::Started::Already(Some(url)) => format!(
                        "Port {number} of pod {pod} is already forwarded to {url}. Press F to stop it."
                    ),
                    forwards::Started::Already(None) => format!(
                        "Port {number} of pod {pod} is already being forwarded. Press F to stop it."
                    ),
                }
            }
        };
        self.note(&advice)
    }

    /// `F` in the pod-containers pane: stop the highlighted port's forward,
    /// or dismiss it if it has already stopped by itself.
    fn stop_highlighted(&mut self) -> Flow {
        let View::PodContainers { namespace, pod, .. } = &self.view else {
            return Flow::Continue;
        };
        if self.focus != Focus::Detail {
            return self.note(
                "Press tab to move to the container list, then F on a forwarded port to stop it.",
            );
        }
        let (namespace, pod) = (namespace.clone(), pod.clone());
        let number = match self.highlighted_entry() {
            None => return Flow::Continue,
            Some(Entry::Container(_)) => {
                return self
                    .note("Move down to a forwarded port, then press F to stop its forward.");
            }
            Some(Entry::Port(_, port)) => port.number,
        };
        let Some(context) = self.selected_cluster().map(|c| c.context_name.clone()) else {
            return Flow::Continue;
        };
        let target = PodPort {
            namespace,
            pod,
            port: number,
        };
        if self.forwards.stop(&context, &target) {
            Flow::Continue
        } else {
            self.note(&format!(
                "Port {number} is not being forwarded. Press f to forward it."
            ))
        }
    }

    /// Put a note on the status line until the next key.
    fn note(&mut self, text: &str) -> Flow {
        self.status = StatusLine::Note(text.to_owned());
        Flow::Continue
    }

    /// Apply what forward `id` reported about itself.
    ///
    /// A forward on the selected cluster that ended for want of credentials
    /// arms `L`, as a refused fetch does in any pane; a successful login then
    /// starts it again (see [`Self::retry_forwards_after_login`]). One on
    /// another cluster does not: `L` logs in to the selected cluster's
    /// profile without asking, and arming it for another account's failure
    /// is what decision 76 rules out. Nothing here disarms it: a forward that
    /// is working says nothing about whichever pane is failing.
    pub fn apply_forward_event(&mut self, id: forwards::Id, event: ForwardEvent) {
        let selected = self.selected_cluster().map(|c| c.context_name.as_str());
        let here = self
            .forwards
            .all()
            .iter()
            .any(|f| f.id == id && Some(f.context.as_str()) == selected);
        if here
            && matches!(
                event,
                ForwardEvent::Ended {
                    credentials: true,
                    ..
                }
            )
        {
            self.credentials_lost = true;
        }
        self.forwards.apply(id, event);
    }

    /// A forward's thread is gone without saying why — it could not start a
    /// runtime. Said on the strip like any other way of stopping, so the port
    /// does not read as starting for ever.
    pub fn apply_forward_lost(&mut self, id: forwards::Id) {
        if self.forwards.wants(id) {
            self.forwards.apply(
                id,
                ForwardEvent::Ended {
                    message: "eks could not start this forward. Press f on its port to try again."
                        .to_owned(),
                    credentials: false,
                },
            );
        }
    }

    /// Whether forward `id` still wants the thread running it.
    #[must_use]
    pub fn wants_forward(&self, id: forwards::Id) -> bool {
        self.forwards.wants(id)
    }

    /// After a successful `L`: the selected cluster's forwards that stopped
    /// for want of credentials, put back to starting, for the event loop to
    /// start again. Another cluster's were not what was logged in to.
    pub fn retry_forwards_after_login(&mut self) -> Vec<ForwardRequest> {
        match self.selected_cluster().map(|c| c.context_name.clone()) {
            Some(context) => self.forwards.retry_after_login(&context),
            None => Vec::new(),
        }
    }

    /// What each forwarded port of the pod on screen shows on its own row,
    /// by port number. Empty outside [`View::PodContainers`].
    fn port_marks(&self) -> containers::Marks {
        let (View::PodContainers { namespace, pod, .. }, Some(cluster)) =
            (&self.view, self.selected_cluster())
        else {
            return containers::Marks::new();
        };
        self.forwards
            .all()
            .iter()
            .filter(|f| {
                f.context == cluster.context_name
                    && f.target.namespace == *namespace
                    && f.target.pod == *pod
            })
            .map(|f| (f.target.port, f.mark()))
            .collect()
    }

    /// What the status line says, if anything: the checks in progress, or a
    /// refusal. Read by the renderer, which only lays it out.
    fn status_lines(&self) -> Vec<Line<'static>> {
        let theme = self.theme;
        match &self.status {
            StatusLine::Idle => Vec::new(),
            StatusLine::Preparing(target) => {
                let what = match &target.container {
                    Some(container) => format!("container {container} of pod {}", target.pod),
                    None => format!("pod {}", target.pod),
                };
                vec![Line::from(vec![
                    Span::styled(format!("Opening a shell in {what}…   "), theme.dim()),
                    Span::styled("esc", theme.heading()),
                    Span::styled(" cancel", theme.dim()),
                ])]
            }
            StatusLine::Note(message) => message
                .lines()
                .map(|line| Line::styled(line.to_owned(), theme.body()))
                .collect(),
            // One line per line of the message, for the reason the node
            // pane splits its errors: the second line is the advice.
            StatusLine::Refused(message) => message
                .lines()
                .map(|line| {
                    Line::styled(
                        line.to_owned(),
                        theme.severity(crate::theme::Severity::Critical),
                    )
                })
                .collect(),
        }
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
    /// The pod-drilldown pane now refreshes in the background too (see
    /// `pods_refresh_target`), so this follows [`Self::apply_nodes`]'s own
    /// rule rather than the one-shot behaviour it used to: a failure after an
    /// earlier fetch had already loaded keeps the last good rows on screen,
    /// as `refresh_error`, rather than blanking them — a pane a reader left
    /// open must not read as "this node lost every pod" over one missed
    /// poll. A failure with nothing loaded yet is still the full-pane error
    /// it always was.
    pub fn apply_pods(&mut self, result: Result<PodsFetch, FetchError>) {
        self.credentials_lost = result.as_ref().is_err_and(|error| error.credentials);
        self.pods = match (result, std::mem::take(&mut self.pods)) {
            (Ok(fetch), _) => PodsState::Loaded {
                rows: fetch.rows,
                selector_note: fetch.selector_note,
                usage_note: fetch.usage_note,
                refresh_error: None,
            },
            (
                Err(error),
                PodsState::Loaded {
                    rows,
                    selector_note,
                    usage_note,
                    ..
                },
            ) => PodsState::Loaded {
                rows,
                selector_note,
                usage_note,
                refresh_error: Some(error.message),
            },
            (Err(error), _) => PodsState::Error(error.message),
        };
        self.sort_pods();
    }

    /// Re-sort the pod-drilldown pane's already-fetched rows under whichever
    /// ordering is current — [`Self::sort_nodes`]'s counterpart for this
    /// pane, called for the same reasons: a fetch landing, `s`, `S`, or the
    /// pane's own `--sort-resource` prompt being applied or cleared.
    fn sort_pods(&mut self) {
        let PodsState::Loaded { rows, .. } = &mut self.pods else {
            return;
        };
        match self.pod_resource_sort.applied() {
            Some(resource) => k8s_pods::sort_by_device(rows, resource, self.pod_direction),
            None => k8s_pods::sort(rows, self.pod_order, self.pod_direction),
        }
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
                events: fetch.events,
                events_error: fetch.events_error,
                events_empty_note: fetch.events_empty_note,
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
    ///
    /// A [`LogEvent::Refused`] also arms `L`, as a refused fetch does in every
    /// other pane. Nothing here disarms it: a line arriving says this stream's
    /// credential is fine, not that some other pane's is.
    pub fn apply_log_event(&mut self, event: LogEvent) {
        if matches!(event, LogEvent::Refused(_)) {
            self.credentials_lost = true;
        }
        self.logs.apply(event);
    }

    /// After a successful `L`: put a container or log pane that failed back
    /// to `Loading`, so the event loop refetches it. Returns whether it did.
    ///
    /// The node and pod panes refetch on `L` already, through the same calls
    /// `r` makes. These two do not refresh on `r` — they fetch once per pod or
    /// container they are asked to show — so without this the pane whose
    /// refusal put `L` on screen would go on showing that refusal after the
    /// login that fixed it. Only a failed pane is retried: one that loaded is
    /// not what `L` was pressed for.
    pub fn retry_failed_detail(&mut self) -> bool {
        match self.view {
            View::PodContainers { .. } if matches!(self.containers, ContainersState::Error(_)) => {
                self.containers = ContainersState::Loading;
                true
            }
            View::ContainerLogs { .. } if matches!(self.logs, LogsState::Error(_)) => {
                self.logs = LogsState::Loading;
                true
            }
            _ => false,
        }
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
                // `s` reclaims the fixed cycle from any `--sort-resource`
                // prompt in effect. Leaving the prompt applied and doing
                // nothing would make the key read as broken rather than as
                // superseded — the same reason a key that cannot help is
                // dropped from the footer rather than kept and silently
                // ignored.
                self.node_resource_sort = ResourceSort::Inactive;
                self.sort_nodes();
            }
            View::NodePods { .. } => {
                self.pod_order = next_variant(self.pod_order);
                // The same reclaiming `View::Overview` does above, now that
                // this pane has a `--sort-resource` prompt of its own.
                self.pod_resource_sort = ResourceSort::Inactive;
                self.sort_pods();
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
                // Unlike `s`, this leaves an applied `--sort-resource`
                // prompt in charge — `direction` is the one thing the fixed
                // cycle and the prompt already share, so reversing it should
                // not also cancel whichever of the two is governing the
                // rows right now.
                self.node_direction = reverse(self.node_direction);
                self.sort_nodes();
            }
            View::NodePods { .. } => {
                self.pod_direction = reverse(self.pod_direction);
                self.sort_pods();
            }
            View::PodContainers { .. } | View::ContainerLogs { .. } => {}
        }
    }

    /// `/`: open the fuzzy filter over whichever pane [`View`] is currently
    /// showing a row list, seeded with whatever query was already applied so
    /// a second press refines it rather than starting over.
    ///
    /// [`View::ContainerLogs`] has no rows to filter, so its own `/` means
    /// something else — "jump to the next matching line," through
    /// [`logs::Log::start_search`] — rather than being a no-op the way
    /// `cycle_sort`/`reverse_sort` still are there. Switches focus to the
    /// detail pane regardless of which pane held it: once this returns, every
    /// following keystroke is query text belonging to that pane, so it
    /// should be the one drawing the focus border.
    fn start_filter(&mut self) {
        if matches!(self.view, View::ContainerLogs { .. }) {
            self.focus = Focus::Detail;
            if let LogsState::Streaming(log) = &mut self.logs {
                log.start_search();
            }
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

    /// [`Self::clear_filter`]'s own counterpart for the container-logs
    /// pane's `/` search.
    fn clear_log_search(&mut self) {
        if let LogsState::Streaming(log) = &mut self.logs {
            log.clear_search();
        }
    }

    /// Handle a key press while the container-logs pane's own `/` search is
    /// capturing text — [`Self::edit_filter`]'s counterpart for
    /// [`logs::Log::handle_search_key`], which holds the actual state
    /// transitions; this only routes the key to whichever log is currently
    /// streaming, the same indirection [`Self::search_log_next`] and
    /// [`Self::search_log_previous`] need for `n`/`N`.
    fn edit_log_search(&mut self, key: KeyEvent) -> Flow {
        if let LogsState::Streaming(log) = &mut self.logs {
            log.handle_search_key(key);
        }
        Flow::Continue
    }

    /// `n`/`N`: jump the container-logs pane to the next (`forward`) or
    /// previous match of its committed search, wrapping past either end of
    /// the buffer. A no-op outside [`View::ContainerLogs`] or before `Enter`
    /// has committed a query — the same shape [`Self::toggle_log_follow`]
    /// and its siblings share.
    fn search_log(&mut self, forward: bool) {
        if let LogsState::Streaming(log) = &mut self.logs {
            let direction = if forward {
                logs::SearchDirection::Forward
            } else {
                logs::SearchDirection::Backward
            };
            log.jump_to_match(direction, false);
        }
    }

    /// Route a key press to whichever `/` is currently capturing text — the
    /// row-list [`Filter`] or the container-logs pane's own
    /// [`logs::LogSearch`], through [`Self::edit_filter`]/[`Self::edit_log_search`].
    /// Only called from [`Self::on_key`], which has already checked that one
    /// of the two is editing.
    fn edit_search(&mut self, key: KeyEvent) -> Flow {
        if self.filter.is_editing() {
            self.edit_filter(key)
        } else {
            self.edit_log_search(key)
        }
    }

    /// Handle a key press while [`Filter::Editing`] is capturing text —
    /// split out of [`Self::on_key`] so every other key's handling does not
    /// have to share a function with this one. `Enter` commits the query
    /// (collapsing an empty one back to [`Filter::Inactive`] rather than
    /// leaving an `Applied("")` with nothing to show for it); `Esc` cancels
    /// outright; `Backspace` and any other character edit the text. Every
    /// other key — including the ones that would otherwise navigate or
    /// quit — is simply not one of those and does nothing.
    ///
    /// Routed to from [`Self::edit_search`] whenever it is `Filter` doing the
    /// editing rather than the logs pane's own search.
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

    /// `R`: begin typing a `--sort-resource` name for whichever pane [`View`]
    /// is currently showing a device-sortable listing — the node pane under
    /// [`View::Overview`], the pod-drilldown pane under [`View::NodePods`] —
    /// seeded with whatever was already applied so a second press refines
    /// it, the same seeding [`Self::start_filter`] does for `/`.
    ///
    /// A no-op from [`View::PodContainers`] or [`View::ContainerLogs`]:
    /// neither pane has a `sort_by_device` of its own to rank by.
    fn start_resource_sort(&mut self) {
        match &self.view {
            View::Overview => {
                self.focus = Focus::Detail;
                self.node_resource_sort =
                    ResourceSort::Editing(self.node_resource_sort.query().to_owned());
            }
            View::NodePods { .. } => {
                self.focus = Focus::Detail;
                self.pod_resource_sort =
                    ResourceSort::Editing(self.pod_resource_sort.query().to_owned());
            }
            View::PodContainers { .. } | View::ContainerLogs { .. } => {}
        }
    }

    /// Handle a key press while either pane's `--sort-resource` prompt is
    /// capturing text — the resource-sort counterpart to
    /// [`Self::edit_filter`], and just as exclusive: [`Self::on_key`] only
    /// reaches this while one of the two is [`ResourceSort::Editing`], and
    /// [`Self::start_resource_sort`] never opens more than one for a given
    /// [`View`]. `Enter` commits the name (collapsing an empty one back to
    /// [`ResourceSort::Inactive`], the same rule `edit_filter` follows);
    /// `Esc` cancels outright. Unlike a filter, committing or cancelling
    /// changes what the rows are actually sorted by, so both re-sort rather
    /// than only redraw — [`advance_resource_sort`] is the pure step shared
    /// between the two arms below, which differ only in which prompt and
    /// which pane's rows that re-sort touches.
    fn edit_resource_sort(&mut self, key: KeyEvent) -> Flow {
        let resort = matches!(key.code, KeyCode::Enter | KeyCode::Esc);
        match &self.view {
            View::Overview => {
                let ResourceSort::Editing(text) = &self.node_resource_sort else {
                    return Flow::Continue;
                };
                self.node_resource_sort = advance_resource_sort(text, key);
                if resort {
                    self.sort_nodes();
                }
            }
            View::NodePods { .. } => {
                let ResourceSort::Editing(text) = &self.pod_resource_sort else {
                    return Flow::Continue;
                };
                self.pod_resource_sort = advance_resource_sort(text, key);
                if resort {
                    self.sort_pods();
                }
            }
            View::PodContainers { .. } | View::ContainerLogs { .. } => {}
        }
        Flow::Continue
    }

    /// `l`/`F`: begin retyping the pod-drilldown pane's label or field
    /// selector, seeded with whatever is already applied so a second press
    /// refines it — the same seeding [`Self::start_resource_sort`] gives its
    /// own prompt. A no-op off [`View::NodePods`]: no other pane's fetch
    /// reads `pod_selectors` at all, so there is nothing here for either key
    /// to retype.
    fn start_selector_edit(&mut self, field: pods::SelectorField) {
        if !matches!(self.view, View::NodePods { .. }) {
            return;
        }
        self.focus = Focus::Detail;
        let text = match field {
            pods::SelectorField::Label => self.pod_selectors.label.clone(),
            pods::SelectorField::Field => self.pod_selectors.field.clone(),
        }
        .unwrap_or_default();
        self.pod_selector_edit = SelectorEdit::Editing {
            field,
            text,
            error: None,
        };
    }

    /// Handle a key press while [`SelectorEdit::Editing`] is capturing text —
    /// the selector counterpart to [`Self::edit_filter`]. `Enter` revalidates
    /// the retyped selector through [`selectors_for`], alongside whichever of
    /// the two is *not* being retyped right now taken from its own already-
    /// canonical `pod_selectors`, so a rejected label does not also
    /// re-reject an already-applied field selector. A good pair commits to
    /// `pod_selectors` and returns to `Inactive`; a bad one stays `Editing`
    /// with the rejection's own sentence attached, so the offending text is
    /// not lost. `Esc` cancels outright, leaving `pod_selectors` exactly as
    /// it was.
    fn edit_selector(&mut self, key: KeyEvent) -> Flow {
        let SelectorEdit::Editing { field, text, .. } = &self.pod_selector_edit else {
            return Flow::Continue;
        };
        let field = *field;
        let mut text = text.clone();
        match key.code {
            KeyCode::Enter => {
                let (label, field_selector): (&str, &str) = match field {
                    pods::SelectorField::Label => {
                        (&text, self.pod_selectors.field.as_deref().unwrap_or(""))
                    }
                    pods::SelectorField::Field => {
                        (self.pod_selectors.label.as_deref().unwrap_or(""), &text)
                    }
                };
                match selectors_for(Some(label), Some(field_selector)) {
                    Ok(selectors) => {
                        self.pod_selectors = selectors;
                        self.pod_selector_edit = SelectorEdit::Inactive;
                    }
                    Err(error) => {
                        self.pod_selector_edit = SelectorEdit::Editing {
                            field,
                            text,
                            error: Some(error.to_string()),
                        };
                    }
                }
            }
            KeyCode::Esc => self.pod_selector_edit = SelectorEdit::Inactive,
            KeyCode::Backspace => {
                text.pop();
                self.pod_selector_edit = SelectorEdit::Editing {
                    field,
                    text,
                    error: None,
                };
            }
            KeyCode::Char(c) => {
                text.push(c);
                self.pod_selector_edit = SelectorEdit::Editing {
                    field,
                    text,
                    error: None,
                };
            }
            _ => {}
        }
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
                let container = self.highlighted_entry()?.container();
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

    /// The highlighted row of the pod-containers pane — a container, or one
    /// of its ports — from the same [`containers::entries`] the pane draws,
    /// so the row `enter`, `x`, and `f` act on is the one on screen.
    fn highlighted_entry(&self) -> Option<Entry<'_>> {
        containers::entries(&self.visible_containers())
            .get(self.detail_selected)
            .copied()
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
            View::PodContainers { .. } => containers::entries(&self.visible_containers()).len(),
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

    /// Seed the dashboard's `-l`/`--field-selector` from the flags the
    /// process started with, before the terminal takes over. Only
    /// `main::dashboard` calls this — every test after `App::new` wants the
    /// empty default, the same reason `select_context` is a separate call
    /// rather than a second `App::new` parameter.
    pub fn set_pod_selectors(&mut self, selectors: Selectors) {
        self.pod_selectors = selectors;
    }

    /// Set the theme every pane draws in, resolved from `--theme`/the config
    /// file's own `theme` and the terminal's own background before the
    /// terminal takes over — the same seed-after-`new` shape
    /// [`Self::set_pod_selectors`] already uses, and for the same reason:
    /// every test after `App::new` wants the dark default rather than a
    /// second constructor parameter every call site would have to pass.
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    /// The terminal answered the OSC 11 background query: draw in whichever
    /// theme its background calls for from the next frame on.
    ///
    /// Only reachable when `--theme`/the config file said `auto` and
    /// `COLORFGBG` could not tell — `main::dashboard` asks nothing otherwise
    /// (see `theme::should_query_background`) — so an answer here always
    /// outranks the dark fallback the first frame was drawn in. A dark
    /// answer is that same fallback confirmed, and changes nothing on screen.
    pub fn apply_terminal_background(&mut self, background: Background) {
        self.theme = Theme::for_background(background);
    }

    /// Whether the theme set so far is only a fallback the terminal should
    /// be asked to confirm: `main::dashboard` sets this from
    /// `theme::should_query_background`, and [`run`] reads it to decide
    /// whether to send the query at all. Seeded after `new` for the reason
    /// [`Self::set_theme`] is.
    pub fn set_asks_terminal_background(&mut self, ask: bool) {
        self.asks_terminal_background = ask;
    }

    /// See [`Self::set_asks_terminal_background`].
    #[must_use]
    pub fn asks_terminal_background(&self) -> bool {
        self.asks_terminal_background
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

        // A refusal on the status line is read once: the next key dismisses
        // it, and still does whatever it does.
        if matches!(self.status, StatusLine::Refused(_) | StatusLine::Note(_)) {
            self.status = StatusLine::Idle;
        }

        // A shell checked for in a pane the user has since left, or on a
        // cluster no longer selected, is not one they want any more.
        let (selected, view) = (self.selected, std::mem::discriminant(&self.view));
        let flow = self.handle_key(key);
        if self.exec_preparing()
            && (self.selected != selected || std::mem::discriminant(&self.view) != view)
        {
            self.status = StatusLine::Idle;
        }
        flow
    }

    /// [`Self::on_key`], for a key press, after the status line's own
    /// bookkeeping.
    fn handle_key(&mut self, key: KeyEvent) -> Flow {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            return Flow::Quit;
        }

        // While a `/` is capturing text — the row-list `Filter` or the
        // container-logs pane's own `logs::LogSearch` — every key below is a
        // character in that query rather than its usual meaning, so this
        // returns before any of it is reached. At most one of the two is
        // ever editing at once: `/` only ever opens one for a given `View`.
        if self.filter.is_editing() || self.logs.is_search_editing() {
            return self.edit_search(key);
        }

        if self.node_resource_sort.is_editing() || self.pod_resource_sort.is_editing() {
            return self.edit_resource_sort(key);
        }

        if self.pod_selector_edit.is_editing() {
            return self.edit_selector(key);
        }

        // Ahead of every other meaning `Esc` has: what it cancels is the
        // newest thing on screen.
        if key.code == KeyCode::Esc && self.exec_preparing() {
            self.status = StatusLine::Idle;
            return Flow::Continue;
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
            // not zero. The line below follows suit for the logs pane.
            KeyCode::Esc | KeyCode::Left if self.filter.is_applied() => self.clear_filter(),
            KeyCode::Esc | KeyCode::Left if self.log_search_active() => self.clear_log_search(),
            KeyCode::Esc | KeyCode::Left => return self.retreat(),
            KeyCode::Tab | KeyCode::Right => self.advance(),
            KeyCode::Enter => self.drill_in(),
            KeyCode::Char('s') => self.cycle_sort(),
            KeyCode::Char('S') => self.reverse_sort(),
            KeyCode::Char('R') => self.start_resource_sort(),
            KeyCode::Char('l') => self.start_selector_edit(pods::SelectorField::Label),
            KeyCode::Char('c') => self.forwards.clear_stopped(),
            KeyCode::Char(c @ ('f' | 'F')) => return self.f_key(c),
            // Only when there is something for it to fix. A key that silently
            // does nothing is worse than one that is not offered, so the
            // footer hint appears under exactly this condition too.
            KeyCode::Char('L') if self.credentials_lost => return Flow::Login,
            KeyCode::Char('x') => return self.start_exec(),
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
            KeyCode::Char('w') => self.toggle_log_wrap(),
            KeyCode::Char('p') => self.toggle_log_previous(),
            KeyCode::Char(c @ ('n' | 'N')) => self.search_log(c == 'n'),
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
/// cluster's context, the drilled-into node's name, and the dashboard's
/// current `-l`/`--field-selector` (`App::pod_selectors`) whenever the detail
/// pane's view changes to [`View::NodePods`], and again on the same three
/// triggers `spawn_nodes` answers to — `r`, the refresh interval, a
/// successful login, or `l`/`F` committing a new selector — for as long as
/// that view is the one on screen (see `pods_refresh_target`,
/// `refetch_pods`). `spawn_containers` is one level
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
/// `foreground` holds the two things that need the terminal itself: `L`'s
/// login and `x`'s shell. This function leaves the alternate screen and raw
/// mode around each, and takes the terminal back after. `x`'s checks run
/// before that, on a background thread, through `drill.prepare_exec`.
///
/// When [`App::asks_terminal_background`] says so, this also sends
/// [`crate::theme::BACKGROUND_QUERY`] once, right after the first frame is on
/// screen, and re-themes if the terminal answers light (decision 111).
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
    foreground: &Foreground,
) -> Result<()> {
    let Foreground { login, session } = foreground;
    let query_background = app.asks_terminal_background();
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

    // The same for `x`, around a shell rather than a login. The line the
    // session prints first, and the shell itself, land on the screen the
    // user had before the dashboard opened, as `kubectl exec` would.
    let in_session = |context: &str, plan: &Plan| -> Result<(), FetchError> {
        let left = leave_terminal();
        let outcome = session(context, plan);
        let entered = enter_terminal();
        let terminal_error = |message| FetchError {
            message,
            credentials: false,
        };
        outcome
            .and(left.map_err(terminal_error))
            .and(entered.map_err(terminal_error))
    };

    let mut next_event = |timeout: Duration| -> std::io::Result<Option<Event>> {
        if event::poll(timeout)? {
            event::read().map(Some)
        } else {
            Ok(None)
        }
    };
    // Written straight to stdout, the terminal `ratatui::init` drew on, and
    // flushed so it leaves now rather than with the next frame. The answer
    // comes back on stdin and is read by `next_event` like any keypress.
    let mut query = || -> std::io::Result<()> {
        let mut stdout = std::io::stdout();
        stdout.write_all(crate::theme::BACKGROUND_QUERY.as_bytes())?;
        stdout.flush()
    };

    let result = event_loop(
        &mut terminal,
        app,
        nodes_rx,
        spawn_nodes,
        drill,
        refresh,
        TerminalIo {
            suspend: &suspended,
            session: &in_session,
            next_event: &mut next_event,
            query_background: if query_background {
                Some(&mut query)
            } else {
                None
            },
        },
    );
    ratatui::restore();
    result
}

/// What runs on this thread with the terminal handed back to it: `L`'s login,
/// and `x`'s shell. Bundled for the reason [`DrillFetchers`] is.
pub struct Foreground {
    pub login: LoginRunner,
    pub session: SessionRunner,
}

impl std::fmt::Debug for Foreground {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Foreground").finish_non_exhaustive()
    }
}

/// What `event_loop` needs from a real terminal beyond drawing on it, taken
/// as closures so a test can drive the whole loop — keys in, frames out —
/// against `TestBackend`, the same reason [`Suspended`] is one.
struct TerminalIo<'a> {
    /// Hand the terminal back around a login; see [`Suspended`].
    suspend: Suspended<'a>,
    /// Hand the terminal back around a shell; see [`SuspendedSession`].
    session: SuspendedSession<'a>,
    /// Wait up to the given time for the next terminal event; `None` if
    /// nothing arrived.
    next_event: &'a mut dyn FnMut(Duration) -> std::io::Result<Option<Event>>,
    /// Send the OSC 11 background query. Called at most once, after the
    /// first frame; `None` when this run asks nothing.
    query_background: Option<&'a mut dyn FnMut() -> std::io::Result<()>>,
}

/// Hand the terminal back to the shell, keeping the `Terminal` handle valid.
///
/// Deliberately not `ratatui::restore()` followed by `ratatui::init()`: that
/// pair hands back a *new* `Terminal`, and the one `event_loop` is holding —
/// along with everything it has cached about the screen — would have to be
/// replaced mid-loop. These are the two things `init` does that matter to a
/// suspended session, undone and redone around the handle we already have.
///
/// The cursor is shown again too: `ratatui` hides it while it draws, and a
/// shell or a helper's prompt without one is hard to type into.
fn leave_terminal() -> Result<(), String> {
    terminal::disable_raw_mode().map_err(|error| format!("could not leave raw mode: {error}"))?;
    execute!(
        std::io::stdout(),
        terminal::LeaveAlternateScreen,
        ratatui::crossterm::cursor::Show
    )
    .map_err(|error| format!("could not leave the alternate screen: {error}"))
}

/// The other half of [`leave_terminal`].
fn enter_terminal() -> Result<(), String> {
    terminal::enable_raw_mode().map_err(|error| format!("could not re-enter raw mode: {error}"))?;
    execute!(std::io::stdout(), terminal::EnterAlternateScreen)
        .map_err(|error| format!("could not re-open the alternate screen: {error}"))
}

/// The fetchers for the detail pane's three drill-down levels, bundled into
/// one parameter for the reason [`run`]'s doc comment gives — and the check
/// `x` starts from those same levels before a shell is opened.
#[derive(Clone, Copy)]
pub struct DrillFetchers<'a> {
    pub spawn_pods: &'a PodsFetcher,
    pub spawn_containers: &'a ContainersFetcher,
    pub spawn_logs: &'a LogsFetcher,
    pub prepare_exec: &'a ExecPreparer,
    /// `f`'s forwards, which outlive the pane they were started from.
    pub start_forward: &'a ForwardStarter,
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

/// The threads running the dashboard's port forwards, by the id [`App`]
/// knows each one as.
///
/// [`App`] decides which forwards should exist; this only holds the handles.
/// After every key the event loop drops the handle of any forward `App` no
/// longer wants — stopped with `F`, or ended by itself — and dropping it is
/// what closes the port (see [`crate::commands::spawn_stream`]).
#[derive(Default)]
struct Forwarders(Vec<Forwarder>);

struct Forwarder {
    id: forwards::Id,
    events: mpsc::Receiver<ForwardEvent>,
    /// Held, never read: dropping it ends the forward.
    _handle: StreamHandle,
}

impl Forwarders {
    fn start(&mut self, start: &ForwardStarter, request: &ForwardRequest) {
        let (events, handle) = start(&request.context, &request.target);
        self.0.push(Forwarder {
            id: request.id,
            events,
            _handle: handle,
        });
    }

    /// Hand `app` everything each forward has said since the last frame,
    /// then drop the ones it no longer wants. Never waits.
    fn take_arrivals(&mut self, app: &mut App) {
        for forwarder in &self.0 {
            loop {
                match forwarder.events.try_recv() {
                    Ok(event) => app.apply_forward_event(forwarder.id, event),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        app.apply_forward_lost(forwarder.id);
                        break;
                    }
                }
            }
        }
        self.reconcile(app);
    }

    /// Drop every forward `app` no longer wants. Called with the arrivals at
    /// the top of each pass, so a forward `F` stopped has closed its port
    /// before the frame that no longer lists it is drawn.
    fn reconcile(&mut self, app: &App) {
        self.0.retain(|forwarder| app.wants_forward(forwarder.id));
    }

    /// After a successful `L`: start again every forward that stopped for
    /// want of credentials (see [`App::retry_forwards_after_login`]).
    fn retry_after_login(&mut self, app: &mut App, start: &ForwardStarter) {
        for request in app.retry_forwards_after_login() {
            self.start(start, &request);
        }
    }
}

/// The keys `event_loop` handles, and the background query whose answer
/// arrives among them.
struct Input<'a> {
    next_event: &'a mut dyn FnMut(Duration) -> std::io::Result<Option<Event>>,
    /// Taken the first time a frame is drawn, so it is sent at most once.
    query: Option<&'a mut dyn FnMut() -> std::io::Result<()>>,
    /// Present only once the query has gone out: until then there is no
    /// reply to pick out of the keys, and after a failed send there never
    /// will be.
    reader: Option<ReplyReader>,
    /// Keys a [`ReplyReader`] gave back several at once — an Alt+`]` that
    /// turned out to be the user's, and whatever they typed after it —
    /// handled one per iteration ahead of anything new from the terminal.
    replayed: VecDeque<KeyEvent>,
}

impl<'a> Input<'a> {
    fn new(io: TerminalIo<'a>) -> Self {
        Self {
            next_event: io.next_event,
            query: io.query_background,
            reader: None,
            replayed: VecDeque::new(),
        }
    }

    /// Called after every draw. The first time, sends the background query.
    ///
    /// After the draw, never before it: the first frame is on screen before
    /// the question is even asked, so the query spends none of the
    /// first-paint budget however slow the answer is (decision 111). A failed
    /// write is not an error the user needs to see — it leaves the theme the
    /// frame was drawn in, the same as a terminal that never answers.
    fn frame_drawn(&mut self) {
        if let Some(query) = self.query.take()
            && query().is_ok()
        {
            self.reader = Some(ReplyReader::default());
        }
    }

    /// The next key for the dashboard to handle, waiting up to [`TICK`] for
    /// one; `None` when nothing arrived, or when what arrived was part of the
    /// terminal's reply rather than a keypress.
    ///
    /// The reply is picked out here, ahead of `App::on_key` *and* the
    /// refresh check after it: a reply's `rgb` is otherwise an `r`, and a
    /// refetch nobody asked for. A reply that names a background re-themes
    /// `app` from the next frame on.
    fn next_key(&mut self, app: &mut App) -> std::io::Result<Option<KeyEvent>> {
        if let Some(key) = self.replayed.pop_front() {
            return Ok(Some(key));
        }
        let Some(Event::Key(key)) = (self.next_event)(TICK)? else {
            return Ok(None);
        };
        let Some(reader) = self.reader.as_mut() else {
            return Ok(Some(key));
        };
        let fed = reader.feed(key);
        if let Some(background) = fed.background {
            app.apply_terminal_background(background);
        }
        self.replayed.extend(fed.keys);
        Ok(self.replayed.pop_front())
    }
}

fn event_loop<B>(
    terminal: &mut Terminal<B>,
    mut app: App,
    mut nodes_rx: Option<mpsc::Receiver<Result<NodesFetch, FetchError>>>,
    spawn_nodes: &NodesFetcher,
    drill: &DrillFetchers<'_>,
    refresh: RefreshInterval,
    io: TerminalIo<'_>,
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
    let login = io.suspend;
    let session = io.session;
    let mut input = Input::new(io);
    // The checks behind `x`, while they run. Kept only while `App` says it
    // is still waiting for them, so dropping it is how `Esc` cancels.
    let mut exec_rx: Option<mpsc::Receiver<Result<Plan, FetchError>>> = None;
    let mut forwarders = Forwarders::default();

    loop {
        take_arrivals(&mut app, nodes_rx.as_ref(), &inflight);
        forwarders.take_arrivals(&mut app);
        if let Some(checked) = exec_rx.as_ref().and_then(checked_exec) {
            exec_rx = None;
            finish_exec(
                terminal,
                &mut app,
                checked,
                session,
                selected_context.as_deref(),
            )?;
        }

        terminal.draw(|frame| draw(frame, &app))?;

        input.frame_drawn();

        if next_refresh.is_some_and(|at| Instant::now() >= at) {
            refetch(spawn_nodes, &mut nodes_rx, selected_context.as_deref());
            refetch_pods(
                drill,
                app.view(),
                app.pod_selectors(),
                selected_context.as_deref(),
                &mut inflight,
            );
            next_refresh = schedule(refresh);
        }

        let Some(key) = input.next_key(&mut app)? else {
            continue;
        };

        let view_before = app.view().clone();
        // Captured the same way as `view_before`, and read the same way
        // below: a commit through `l`/`F` is a fetch trigger in its own
        // right, and the only way to notice one is to compare before and
        // after, the same as a cluster or view change already does.
        let pod_selectors_before = app.pod_selectors().clone();

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
                        let context = selected_context.as_deref();
                        refetch(spawn_nodes, &mut nodes_rx, context);
                        refetch_after_login(&mut app, drill, context, &mut inflight);
                        forwarders.retry_after_login(&mut app, drill.start_forward);
                        next_refresh = schedule(refresh);
                    }
                    Err(message) => app.apply_login_failure(message),
                }
            }
            Flow::Exec(target) => {
                exec_rx = selected_context
                    .as_deref()
                    .map(|context| (drill.prepare_exec)(context, &target));
            }
            Flow::Forward(request) => forwarders.start(drill.start_forward, &request),
            Flow::Continue => {}
        }
        if !app.exec_preparing() {
            exec_rx = None;
        }

        if is_refresh_key(key) {
            refetch(spawn_nodes, &mut nodes_rx, selected_context.as_deref());
            refetch_pods(
                drill,
                app.view(),
                app.pod_selectors(),
                selected_context.as_deref(),
                &mut inflight,
            );
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

        // Its own `if`, not another arm of the chain above: committing a new
        // `l`/`F` selector never changes the cluster or the view, so it would
        // never be reached as an `else`. Immediate for the same reason a
        // selection change is above — waiting for the interval would leave
        // the pane showing the *previous* selector's rows under the newly
        // typed one for however long that takes.
        if *app.pod_selectors() != pod_selectors_before {
            refetch_pods(
                drill,
                app.view(),
                app.pod_selectors(),
                selected_context.as_deref(),
                &mut inflight,
            );
        }
    }
}

/// Apply every background fetch that has finished since the last frame.
///
/// Never waits for one that has not.
fn take_arrivals(
    app: &mut App,
    nodes_rx: Option<&mpsc::Receiver<Result<NodesFetch, FetchError>>>,
    inflight: &Inflight,
) {
    // Non-blocking: a fetch that has not finished yet leaves the pane
    // exactly as it was, and one that finished while the user was
    // pressing keys is picked up on the very next frame rather than
    // waiting for a quiet moment.
    if let Some(rx) = nodes_rx
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
}

/// Act on the answer to the checks behind `x`: run the shell they planned,
/// or put their refusal on the status line.
fn finish_exec<B>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    checked: Result<Plan, FetchError>,
    session: SuspendedSession<'_>,
    context: Option<&str>,
) -> Result<()>
where
    B: ratatui::backend::Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    match checked {
        Ok(plan) => {
            // Blocks until the shell exits, on purpose: the terminal is the
            // shell's until then. The background fetches go on meanwhile,
            // and are picked up on the next pass.
            let outcome = session(context.unwrap_or_default(), &plan);
            // The shell had the screen, so `ratatui`'s idea of what is on it
            // is stale, as after `L`; and `leave_terminal` showed the cursor
            // `ratatui` keeps hidden.
            terminal.clear()?;
            terminal.hide_cursor()?;
            app.apply_exec_ending(outcome);
        }
        Err(error) => app.apply_exec_refusal(error),
    }
    Ok(())
}

/// The answer to the checks behind `x`, if it has arrived.
///
/// A check whose thread ended without answering — it could not start a
/// runtime — is reported as a refusal rather than left as an "Opening a
/// shell…" that never finishes.
fn checked_exec(rx: &mpsc::Receiver<Result<Plan, FetchError>>) -> Option<Result<Plan, FetchError>> {
    match rx.try_recv() {
        Ok(checked) => Some(checked),
        Err(mpsc::TryRecvError::Empty) => None,
        Err(mpsc::TryRecvError::Disconnected) => Some(Err(FetchError {
            message: "eks could not start the check for a shell. Press x to try again.".to_owned(),
            credentials: false,
        })),
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
                inflight.pods = Some((drill.spawn_pods)(context, node, app.pod_selectors()));
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

/// After a successful `L`, refetch whichever detail pane is on screen: the
/// pod listing, as `r` would, or a container or log pane whose own failure is
/// what `L` was pressed for — see [`App::retry_failed_detail`].
fn refetch_after_login(
    app: &mut App,
    drill: &DrillFetchers<'_>,
    context: Option<&str>,
    inflight: &mut Inflight,
) {
    refetch_pods(drill, app.view(), app.pod_selectors(), context, inflight);
    if app.retry_failed_detail() {
        start_drill_fetch(app, drill, context, inflight);
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

/// The node whose pods should be refetched right now, or `None` when the
/// detail pane is not on [`View::NodePods`] — the pane's own answer to
/// "am I the one currently on screen", read by every trigger that also
/// refetches the node listing (the interval tick, `r`, and a successful
/// login) so a reader who has drilled further in, into a pod's containers or
/// its logs, does not pay for a fetch nothing is showing. Pulled out of
/// [`refetch_pods`] as a pure function so the "which view counts" rule is a
/// fixture rather than something only a live event loop exercises.
fn pods_refresh_target(view: &View) -> Option<&str> {
    match view {
        View::NodePods { node } => Some(node),
        View::Overview | View::PodContainers { .. } | View::ContainerLogs { .. } => None,
    }
}

/// Refresh the pod-drilldown pane's rows in place, the same trigger points
/// [`refetch`] uses for the node listing beside it — this pane fetches once
/// per node no longer, matching `commands::pods::spawn_gather_for_node`'s own
/// updated doc comment. Replaces whatever pod fetch was already in flight
/// rather than adding a second one; a no-op when nothing is selected or the
/// pane showing right now is not [`View::NodePods`], so a reader who has
/// drilled deeper, or backed out to the node listing, is not charged for a
/// fetch nobody would see land.
fn refetch_pods(
    drill: &DrillFetchers<'_>,
    view: &View,
    selectors: &Selectors,
    selected_context: Option<&str>,
    inflight: &mut Inflight,
) {
    if let (Some(context), Some(node)) = (selected_context, pods_refresh_target(view)) {
        inflight.pods = Some((drill.spawn_pods)(context, node, selectors));
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

/// One key's effect on a `--sort-resource` prompt already [`ResourceSort::Editing`]
/// `text` — the pure step [`App::edit_resource_sort`] shares between the node
/// pane's prompt and the pod-drilldown pane's own copy, which differ only in
/// which field this is assigned back into and whose rows get re-sorted
/// afterward. `Enter` commits (collapsing an empty query back to `Inactive`
/// rather than leaving an `Applied("")` with nothing to rank by); `Esc`
/// cancels outright; `Backspace` and any other character edit the text; any
/// other key leaves it as it was.
fn advance_resource_sort(text: &str, key: KeyEvent) -> ResourceSort {
    let mut text = text.to_owned();
    match key.code {
        KeyCode::Enter if text.is_empty() => ResourceSort::Inactive,
        KeyCode::Enter => ResourceSort::Applied(text),
        KeyCode::Esc => ResourceSort::Inactive,
        KeyCode::Backspace => {
            text.pop();
            ResourceSort::Editing(text)
        }
        KeyCode::Char(c) => {
            text.push(c);
            ResourceSort::Editing(text)
        }
        _ => ResourceSort::Editing(text),
    }
}

/// Draw one frame.
pub fn draw(frame: &mut Frame, app: &App) {
    let status = app.status_lines();
    let selected_context = app.selected_cluster().map(|c| c.context_name.as_str());
    let strip = app.forwards.strip(selected_context, app.theme);
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Min(0),    // body
            Constraint::Length(strip_height(&strip, area.width, area.height)),
            Constraint::Length(status_height(&status, area.width, area.height)),
            Constraint::Length(1), // footer
        ])
        .split(area);

    draw_header(frame, chunks[0], app);
    draw_body(frame, chunks[1], app);
    draw_forwards(
        frame,
        chunks[2],
        strip,
        app.forwards.stopped() > 0,
        app.theme,
    );
    // Indented a column, as the footer's hints are, wrapped lines included.
    frame.render_widget(
        Paragraph::new(status)
            .wrap(Wrap { trim: false })
            .block(Block::new().padding(Padding::left(STATUS_INDENT))),
        chunks[3],
    );
    draw_footer(frame, chunks[4], app);
}

/// How many rows the forwards strip takes: its title rule, and its lines
/// wrapped as the status line's are, within the same third of the screen.
/// None at all while nothing has been forwarded.
fn strip_height(lines: &[Line<'_>], width: u16, height: u16) -> u16 {
    if lines.is_empty() {
        return 0;
    }
    let cap = (height / 3).max(1);
    status_height(lines, width, height)
        .saturating_add(1)
        .min(cap)
}

/// The forwards strip: every forward this dashboard has started, under a
/// rule titled the way the panes are, on every view — a forward outlives the
/// pane it was started from, so where to click for it must too.
fn draw_forwards(
    frame: &mut Frame,
    area: Rect,
    lines: Vec<Line<'static>>,
    any_stopped: bool,
    theme: Theme,
) {
    if area.height == 0 {
        return;
    }
    let block = Block::new()
        .borders(ratatui::widgets::Borders::TOP)
        .border_style(theme.pane_border(false))
        .title(strip_title(any_stopped))
        .title_style(theme.heading())
        .padding(Padding::left(STATUS_INDENT));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block),
        area,
    );
}

/// The strip's title. It says that forwards end with the dashboard here,
/// where it is on screen whenever there is one, rather than by lengthening
/// `q quit` in a footer that already clips at a hundred columns; the armed
/// quit says it again. While a stopped forward is listed it also offers `c`,
/// the one key that clears it from any pane.
fn strip_title(any_stopped: bool) -> &'static str {
    if any_stopped {
        " Forwards · they end when eks quits · c clears stopped "
    } else {
        " Forwards · they end when eks quits "
    }
}

/// How far the status line is indented from the screen's left edge.
const STATUS_INDENT: u16 = 1;

/// How many rows the status line takes: each of its lines, wrapped at
/// `width` less [`STATUS_INDENT`], up to a third of the screen so the panes
/// stay in sight.
///
/// Wrapping is estimated by characters, where `ratatui` wraps by words, so a
/// long message can run one row short and lose the end of its last line;
/// the cap would cut it sooner anyway.
fn status_height(lines: &[Line<'_>], width: u16, height: u16) -> u16 {
    let width = usize::from(width.saturating_sub(STATUS_INDENT));
    if lines.is_empty() || width == 0 {
        return 0;
    }
    let rows: usize = lines
        .iter()
        .map(|line| line.width().div_ceil(width).max(1))
        .sum();
    let cap = usize::from((height / 3).max(1));
    u16::try_from(rows.min(cap)).unwrap_or(u16::MAX)
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
            app.node_sort(),
            app.node_direction(),
            app.node_resource_prompt(),
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
            app.pod_sort(),
            app.pod_direction(),
            app.pod_resource_prompt(),
            app.pod_selector_prompt(),
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
                &app.port_marks(),
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

/// What the footer says while a quit waits for its second press: that it
/// will end the forwards too, while any are running.
fn quit_warning(running: usize) -> String {
    match running {
        0 => "press esc/q again to quit".to_owned(),
        1 => "press esc/q again to quit and end the port forward".to_owned(),
        n => format!("press esc/q again to quit and end {n} port forwards"),
    }
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme;

    if app.quit_pending() {
        let warning = Line::from(vec![
            Span::raw(" "),
            Span::styled(
                quit_warning(app.forwards.running()),
                theme.severity(crate::theme::Severity::Warn),
            ),
        ]);
        frame.render_widget(Paragraph::new(warning), area);
        return;
    }

    let hints: Vec<(&str, &str)> = if app.is_filtering() {
        // Every other key below is filter text while this is showing — see
        // `App::on_key` — so the hints say that instead of listing keys that
        // do not mean what they usually do right now.
        vec![("type", "filter"), ("enter", "apply"), ("esc", "cancel")]
    } else if app.is_typing_resource_sort() {
        // The `R` counterpart to the branch above: every key is resource-name
        // text while this is showing, through `App::edit_resource_sort`.
        vec![("type", "resource"), ("enter", "apply"), ("esc", "cancel")]
    } else if app.is_typing_selector() {
        // The `l`/`F` counterpart to the two branches above: every key is
        // selector text while this is showing, through `App::edit_selector`.
        vec![("type", "selector"), ("enter", "apply"), ("esc", "cancel")]
    } else if app.is_searching_log() {
        // The container-logs pane's own `/`, which jumps rather than
        // narrows — "apply" would read like the row-list filter above, so
        // this says what `Enter` actually does here.
        vec![("type", "search"), ("enter", "jump"), ("esc", "cancel")]
    } else if app.credentials_lost() {
        // Its own list rather than one more hint on the end of the others,
        // for the reason the two branches around it are: this is a state that
        // changes what the keys are worth. Until the session is back, `j/k`
        // and `s/S` are moving around a listing nothing can refill, and the
        // hints that survive are the ones that lead somewhere. Keeping the
        // list short is also what keeps `q quit` on screen at eighty columns,
        // which the default list below is deliberately ordered to protect.
        vec![
            ("L", "log in"),
            ("tab/→", "switch"),
            ("←/esc", "back"),
            ("r", "refresh"),
            ("q", "quit"),
        ]
    } else if matches!(app.view(), View::ContainerLogs { .. }) {
        // A log has nothing to `enter` further into and no ordering `s`/`S`
        // could apply to — `f`/`w`/`p` take their place, the three things
        // this pane's own keys change. `/` and the conditional `n`/`N` below
        // it are placed after `q`, the same "narrow terminal clips the
        // newest hint first" ordering the default list below already
        // follows, so a narrow pane drops them before it drops `q quit`.
        let mut hints = vec![
            ("tab/→", "switch"),
            ("j/k", "scroll"),
            ("f", "follow"),
            ("w", "wrap"),
            ("p", "previous"),
            ("←/esc", "back"),
            ("q", "quit"),
            ("/", "search"),
            ("x", "shell"),
        ];
        if app.log_search_active() {
            hints.push(("n/N", "next/prev match"));
        }
        hints
    } else {
        let mut hints = vec![
            ("tab/→", "switch/drill"),
            ("j/k", "move"),
            ("enter", "open"),
            ("←/esc", "back"),
            ("r", "refresh"),
            // The container list has no ordering for `s`/`S` to change (see
            // `App::cycle_sort`), so its slot goes to `x`, which is the
            // thing a container list is most often opened for.
            if matches!(app.view(), View::PodContainers { .. }) {
                ("x", "shell")
            } else {
                ("s/S", "sort")
            },
            ("q", "quit"),
            // Last, not because it matters least, but so a narrow terminal
            // clips the newest hint before it clips `q quit` — the one this
            // tool can least afford to hide.
            ("/", "filter"),
        ];
        // `f`/`F` only where there is a port to press them on. After `/`
        // for the reason `x` is on the pod list; the highlighted port names
        // the key on its own row too, so it is on screen however narrow the
        // terminal.
        if matches!(app.view(), View::PodContainers { .. })
            && app
                .containers()
                .rows()
                .iter()
                .any(|row| !row.ports.is_empty())
        {
            hints.push(("f/F", "forward/stop"));
        }
        // On a pod, `x` opens a shell in its default container. After `/`,
        // so it is clipped before `q quit`, and ahead of the two narrower
        // hints below.
        if matches!(app.view(), View::NodePods { .. }) {
            hints.push(("x", "shell"));
        }
        // `R` only does anything against a pane with a `sort_by_device` of
        // its own — the node pane and, now, the pod-drilldown pane (see
        // `App::start_resource_sort`) — so it is offered only there rather
        // than advertised as a key that silently does nothing on the other
        // two. Dropped first of all under a narrow terminal: it is the
        // newest and the narrowest-scoped of the hints here.
        if matches!(app.view(), View::Overview | View::NodePods { .. }) {
            hints.push(("R", "sort resource"));
        }
        // `l`/`F` only do anything against the pod-drilldown pane's own
        // fetch (`App::start_selector_edit`) — the node pane has no
        // selector of its own to retype. Pushed last of all: the newest and
        // narrowest-scoped hint here, so a narrow terminal drops it before
        // `R` above.
        if matches!(app.view(), View::NodePods { .. }) {
            hints.push(("l/F", "selector"));
        }
        hints
    };

    let mut spans = vec![Span::raw(" ")];
    for &(key, action) in &hints {
        spans.push(Span::styled(key, theme.heading()));
        spans.push(Span::styled(format!(" {action}   "), theme.dim()));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::k8s::forward::ports::Declared;
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

    #[test]
    fn a_fresh_app_draws_in_the_dark_theme_until_told_otherwise() {
        // `main::dashboard` sets the resolved theme right after `App::new`,
        // but every test after it — this whole module — wants the same
        // default `set_pod_selectors` already gives `pod_selectors`.
        assert_eq!(app().theme, Theme::dark());
    }

    #[test]
    fn set_theme_replaces_the_theme_every_pane_draws_in() {
        let mut app = app();
        app.set_theme(Theme::light());
        assert_eq!(app.theme, Theme::light());
    }

    #[test]
    fn a_terminal_background_answer_sets_the_theme_it_calls_for() {
        let mut app = app();
        app.apply_terminal_background(Background::Light);
        assert_eq!(app.theme, Theme::light());
        app.apply_terminal_background(Background::Dark);
        assert_eq!(app.theme, Theme::dark());
    }

    #[test]
    fn a_fresh_app_asks_the_terminal_nothing_until_told_to() {
        let mut app = app();
        assert!(!app.asks_terminal_background());
        app.set_asks_terminal_background(true);
        assert!(app.asks_terminal_background());
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

        use crate::k8s::nodes::{Capacity, Pressure, Share};

        crate::k8s::nodes::NodeRow {
            name: name.to_owned(),
            status: "Ready".to_owned(),
            severity: crate::theme::Severity::Ok,
            version: "v1.31".to_owned(),
            pressure: Pressure::default(),
            cpu: Capacity::default(),
            memory: Capacity::default(),
            cpu_requested: Share::default(),
            memory_requested: Share::default(),
            cpu_used: Share::default(),
            memory_used: Share::default(),
            usage_stale: false,
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
            usage_stale: false,
            cpu_requested: Quantity::default(),
            memory_requested: Quantity::default(),
            extended_requested: std::collections::BTreeMap::new(),
            cpu_limit: None,
            memory_limit: None,
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
            requests_note: None,
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
            requests_note: None,
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
            usage_note: None,
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
            ports: Vec::new(),
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
    fn declared(container: &str, number: u16, name: Option<&str>, protocol: &str) -> Declared {
        Declared {
            container: container.to_owned(),
            name: name.map(str::to_owned),
            number,
            protocol: protocol.to_owned(),
        }
    }

    /// An app drilled into `api-1`'s containers, where `web` declares an
    /// HTTP port and a UDP one and `worker` declares none — so the rows the
    /// highlight moves through are `web`, `8080`, `53`, `worker`, focused on
    /// `web`.
    fn app_with_ports() -> App {
        let mut app = app_with_pod();
        app.on_key(press(KeyCode::Enter));
        app.apply_containers(Ok(ContainersFetch {
            rows: vec![
                crate::k8s::pods::ContainerRow {
                    ports: vec![
                        declared("web", 8080, Some("http"), "TCP"),
                        declared("web", 53, Some("dns"), "UDP"),
                    ],
                    ..container_row("web")
                },
                container_row("worker"),
            ],
            ..ContainersFetch::default()
        }));
        app
    }

    /// The pod port `app_with_ports`' `8080` row forwards.
    fn web_port() -> PodPort {
        PodPort {
            namespace: "default".to_owned(),
            pod: "api-1".to_owned(),
            port: 8080,
        }
    }

    /// The selected cluster's context, as a forward records it.
    const BETA: &str = "arn:aws:eks:us-east-1:1234:cluster/beta";

    /// [`app_with_ports`] with `f` pressed on `8080`: the forward's id, and
    /// the app with it starting.
    fn app_forwarding() -> (forwards::Id, App) {
        let mut app = app_with_ports();
        app.on_key(press(KeyCode::Char('j')));
        let Flow::Forward(request) = app.on_key(press(KeyCode::Char('f'))) else {
            return (u64::MAX, app);
        };
        (request.id, app)
    }

    fn status_text(app: &App) -> String {
        app.status_lines()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

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
                requests_note: None,
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
                requests_note: None,
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
    fn a_log_that_could_not_open_for_want_of_a_sign_in_offers_l() {
        let mut app = app_with_container();
        app.on_key(press(KeyCode::Enter));

        app.apply_log_event(LogEvent::Refused(
            "L runs `aws` in the foreground".to_owned(),
        ));

        assert!(app.credentials_lost());
        assert_eq!(
            app.logs(),
            &LogsState::Error("L runs `aws` in the foreground".to_owned())
        );
        assert_eq!(app.on_key(press(KeyCode::Char('L'))), Flow::Login);
    }

    #[test]
    fn a_log_that_failed_for_any_other_reason_does_not_offer_l() {
        let mut app = app_with_container();
        app.on_key(press(KeyCode::Enter));

        app.apply_log_event(LogEvent::Ended(Some("container not found".to_owned())));

        assert!(!app.credentials_lost());
    }

    #[test]
    fn a_line_from_one_log_does_not_withdraw_an_offer_another_pane_made() {
        // The node pane's refusal stands until the node pane is refetched; a
        // stream opened earlier on the old token says nothing about it.
        let mut app = app();
        app.apply_nodes(Err(refused("prod rejected your credentials")));

        app.apply_log_event(LogEvent::Line("still streaming".to_owned()));

        assert!(app.credentials_lost());
    }

    #[test]
    fn after_l_a_container_pane_that_failed_goes_back_to_loading() {
        let mut app = app_with_pod();
        app.on_key(press(KeyCode::Enter));
        app.apply_containers(Err(refused("prod rejected your credentials")));

        assert!(app.retry_failed_detail());
        assert_eq!(app.containers(), &ContainersState::Loading);
    }

    #[test]
    fn after_l_a_log_pane_that_failed_goes_back_to_loading() {
        let mut app = app_with_container();
        app.on_key(press(KeyCode::Enter));
        app.apply_log_event(LogEvent::Refused("refused".to_owned()));

        assert!(app.retry_failed_detail());
        assert_eq!(app.logs(), &LogsState::Loading);
    }

    #[test]
    fn after_l_a_pane_that_loaded_is_left_alone() {
        // The container list on screen is not what `L` was pressed for, and
        // neither is a log already streaming.
        let mut containers = app_with_container();
        assert!(!containers.retry_failed_detail());
        assert!(matches!(
            containers.containers(),
            ContainersState::Loaded { .. }
        ));

        let mut logs = app_with_container();
        logs.on_key(press(KeyCode::Enter));
        logs.apply_log_event(LogEvent::Line("hello".to_owned()));
        assert!(!logs.retry_failed_detail());

        // And the overview's panes refetch on `L` by themselves.
        assert!(!app().retry_failed_detail());
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
                requests_note: None,
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
                requests_note: None,
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
    fn the_footer_offers_r_over_both_panes_with_a_device_ordering() {
        // Wide enough that the footer's last hint — `R`, deliberately the
        // first to clip on a narrow terminal (see `draw_footer`) — is not
        // truncated away before this test gets to look for it.
        let mut terminal = Terminal::new(TestBackend::new(200, 20)).unwrap();
        let app_initial = app();

        terminal.draw(|frame| draw(frame, &app_initial)).unwrap();
        let over_node_pane = terminal.backend().to_string();
        assert!(
            over_node_pane.contains("sort resource"),
            "over the node pane: {over_node_pane}"
        );

        let mut app = app_with_node();
        app.on_key(press(KeyCode::Enter));
        assert!(matches!(app.view(), View::NodePods { .. }));

        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let over_pod_drilldown_pane = terminal.backend().to_string();
        assert!(
            over_pod_drilldown_pane.contains("sort resource"),
            "over the pod-drilldown pane: {over_pod_drilldown_pane}"
        );

        app.apply_pods(Ok(PodsFetch {
            rows: vec![pod_row("api-1")],
            selector_note: None,
            usage_note: None,
        }));
        app.on_key(press(KeyCode::Enter));
        assert!(matches!(app.view(), View::PodContainers { .. }));

        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let over_pod_containers_pane = terminal.backend().to_string();
        assert!(
            !over_pod_containers_pane.contains("sort resource"),
            "over the pod-containers pane, which has no device ordering: \
             {over_pod_containers_pane}"
        );
    }

    #[test]
    fn the_footer_switches_to_resource_hints_while_the_prompt_is_editing() {
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        let mut app = app();

        app.on_key(press(KeyCode::Char('R')));
        terminal.draw(|frame| draw(frame, &app)).unwrap();

        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("resource"), "{rendered}");
        assert!(rendered.contains("apply"), "{rendered}");
        assert!(rendered.contains("cancel"), "{rendered}");
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
            requests_note: None,
        }));
        app.toggle_focus();
        app.on_key(press(KeyCode::Enter));
        app.apply_pods(Ok(PodsFetch {
            rows: vec![pod_row("api-1")],
            selector_note: None,
            usage_note: None,
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
            requests_note: None,
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
                ports: Vec::new(),
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
    fn pods_refresh_target_names_the_node_under_the_node_pods_view() {
        let view = View::NodePods {
            node: "worker-1".to_owned(),
        };
        assert_eq!(pods_refresh_target(&view), Some("worker-1"));
    }

    #[test]
    fn pods_refresh_target_is_none_off_the_node_pods_view() {
        assert_eq!(pods_refresh_target(&View::Overview), None);
        assert_eq!(
            pods_refresh_target(&View::PodContainers {
                node: "worker-1".to_owned(),
                namespace: "default".to_owned(),
                pod: "api-1".to_owned(),
            }),
            None
        );
        assert_eq!(
            pods_refresh_target(&View::ContainerLogs {
                node: "worker-1".to_owned(),
                namespace: "default".to_owned(),
                pod: "api-1".to_owned(),
                container: "app".to_owned(),
                previous: false,
            }),
            None
        );
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
            requests_note: None,
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
            requests_note: None,
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
                requests_note: None,
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
            requests_note: None,
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
            requests_note: None,
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
                usage_note: None,
                refresh_error: None,
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

    /// Drives `/`, then a query, then `Enter` through `App::on_key` — the
    /// end-to-end path [`logs::tests`] covers piece by piece on `Log` itself.
    fn commit_log_search(app: &mut App, query: &str) {
        app.on_key(press(KeyCode::Char('/')));
        for c in query.chars() {
            app.on_key(press(KeyCode::Char(c)));
        }
        app.on_key(press(KeyCode::Enter));
    }

    #[test]
    fn slash_then_a_query_and_enter_commits_a_search_and_jumps_to_the_match() {
        let mut app = app_with_container();
        app.on_key(press(KeyCode::Enter));
        for line in ["alpha", "beta error", "gamma"] {
            app.apply_log_event(LogEvent::Line(line.to_owned()));
        }

        commit_log_search(&mut app, "error");

        assert!(!app.is_searching_log());
        assert!(app.log_search_active());
        assert_eq!(
            streaming(app.logs()).visible(1).collect::<Vec<_>>(),
            vec!["beta error"]
        );
    }

    #[test]
    fn n_and_shift_n_step_through_a_committed_search() {
        let mut app = app_with_container();
        app.on_key(press(KeyCode::Enter));
        // The newest line deliberately does not match: committing a search
        // while following lands on the newest *matching* line, which needs
        // wrapping past the oldest match to reach here, the same case
        // `logs::tests::committing_a_query_jumps_to_the_nearest_match_and_stops_following`
        // covers directly on `Log`.
        for line in ["alpha", "beta error", "gamma", "delta error", "epsilon"] {
            app.apply_log_event(LogEvent::Line(line.to_owned()));
        }
        commit_log_search(&mut app, "error");
        assert_eq!(
            streaming(app.logs()).visible(1).collect::<Vec<_>>(),
            vec!["beta error"]
        );

        app.on_key(press(KeyCode::Char('n')));
        assert_eq!(
            streaming(app.logs()).visible(1).collect::<Vec<_>>(),
            vec!["delta error"]
        );

        app.on_key(press(KeyCode::Char('N')));
        assert_eq!(
            streaming(app.logs()).visible(1).collect::<Vec<_>>(),
            vec!["beta error"],
            "shift-n steps back to where n started"
        );
    }

    #[test]
    fn n_and_shift_n_are_harmless_outside_the_logs_view() {
        let mut app = app_with_node();

        assert_eq!(app.on_key(press(KeyCode::Char('n'))), Flow::Continue);
        assert_eq!(app.on_key(press(KeyCode::Char('N'))), Flow::Continue);
        assert_eq!(app.view(), &View::Overview);
    }

    #[test]
    fn esc_clears_an_applied_log_search_before_backing_out_of_the_drill_down() {
        let mut app = app_with_container();
        app.on_key(press(KeyCode::Enter));
        app.apply_log_event(LogEvent::Line("beta error".to_owned()));
        commit_log_search(&mut app, "error");
        assert!(app.log_search_active());

        app.on_key(press(KeyCode::Esc));
        assert!(!app.log_search_active(), "the first esc clears the search");
        assert!(
            matches!(app.view(), View::ContainerLogs { .. }),
            "not backed out yet"
        );

        app.on_key(press(KeyCode::Esc));
        assert!(
            matches!(app.view(), View::PodContainers { .. }),
            "the second esc backs out"
        );
    }

    #[test]
    fn esc_while_editing_a_log_search_cancels_it_outright() {
        let mut app = app_with_container();
        app.on_key(press(KeyCode::Enter));
        app.apply_log_event(LogEvent::Line("hello".to_owned()));
        app.on_key(press(KeyCode::Char('/')));
        app.on_key(press(KeyCode::Char('x')));

        app.on_key(press(KeyCode::Esc));

        assert!(!app.is_searching_log());
        assert!(!app.log_search_active());
    }

    #[test]
    fn the_footer_switches_to_search_hints_while_the_logs_prompt_is_editing() {
        let mut app = app_with_container();
        app.on_key(press(KeyCode::Enter));
        app.apply_log_event(LogEvent::Line("hello".to_owned()));
        app.on_key(press(KeyCode::Char('/')));

        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();

        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("search"), "{rendered}");
        assert!(rendered.contains("jump"), "{rendered}");
        assert!(rendered.contains("cancel"), "{rendered}");
    }

    #[test]
    fn the_footer_offers_n_and_shift_n_only_once_a_log_search_is_committed() {
        // `n/N` is the newest, narrowest-scoped hint in this pane's list — see
        // `draw_footer` — so a wide terminal keeps it from being clipped
        // before this test gets to look for it.
        let mut terminal = Terminal::new(TestBackend::new(200, 20)).unwrap();
        let mut app = app_with_container();
        app.on_key(press(KeyCode::Enter));
        app.apply_log_event(LogEvent::Line("beta error".to_owned()));

        terminal.draw(|frame| draw(frame, &app)).unwrap();
        assert!(
            !terminal.backend().to_string().contains("n/N"),
            "no search committed yet"
        );

        commit_log_search(&mut app, "error");

        terminal.draw(|frame| draw(frame, &app)).unwrap();
        assert!(terminal.backend().to_string().contains("n/N"));
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
                events: Vec::new(),
                events_error: None,
                events_empty_note: String::new(),
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
            usage_note: None,
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
                usage_note: None,
                refresh_error: None,
            }
        );
    }

    #[test]
    fn apply_pods_carries_the_selector_note_into_the_loaded_state() {
        let mut app = app();

        app.apply_pods(Ok(PodsFetch {
            rows: Vec::new(),
            selector_note: Some("label selector `app=api`".to_owned()),
            usage_note: None,
        }));

        assert_eq!(
            app.pods(),
            &PodsState::Loaded {
                rows: Vec::new(),
                selector_note: Some("label selector `app=api`".to_owned()),
                usage_note: None,
                refresh_error: None,
            }
        );
    }

    #[test]
    fn apply_pods_moves_a_failure_into_the_error_state() {
        let mut app = app();

        app.apply_pods(Err(failed("could not list pods")));

        assert_eq!(
            app.pods(),
            &PodsState::Error("could not list pods".to_owned())
        );
    }

    #[test]
    fn apply_pods_keeps_the_last_good_rows_when_a_background_refresh_fails() {
        // The pod-drilldown pane now refreshes on the same interval and `r`
        // press the node pane does (see `pods_refresh_target`), so a failure
        // is no longer necessarily the *first* answer this pane gets: one
        // bad poll after a good one must not blank it back to an error
        // screen, mirroring `a_failed_refresh_after_a_loaded_pane_keeps_its_rows`.
        let mut app = app();
        app.apply_pods(Ok(PodsFetch::default()));

        app.apply_pods(Err(failed("could not list pods: nope")));

        assert_eq!(
            app.pods(),
            &PodsState::Loaded {
                rows: Vec::new(),
                selector_note: None,
                usage_note: None,
                refresh_error: Some("could not list pods: nope".to_owned()),
            }
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
            requests_note: None,
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
            requests_note: None,
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
            requests_note: None,
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
            requests_note: None,
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

    /// A node reporting one booked device, for the `--sort-resource` tests.
    fn node_row_with_device(
        name: &str,
        resource: &str,
        booked: &str,
        allocatable: &str,
    ) -> crate::k8s::nodes::NodeRow {
        use crate::k8s::nodes::{Capacity, Device};
        use crate::k8s::quantity::Quantity;

        let mut devices = std::collections::BTreeMap::new();
        devices.insert(
            resource.to_owned(),
            Device {
                capacity: Capacity {
                    allocatable: Some(Quantity::parse(allocatable).unwrap()),
                    capacity: Some(Quantity::parse(allocatable).unwrap()),
                },
                booked: Some(Quantity::parse(booked).unwrap()),
            },
        );
        crate::k8s::nodes::NodeRow {
            devices,
            ..node_row(name)
        }
    }

    #[test]
    fn r_opens_a_resource_sort_prompt_and_switches_focus_to_the_detail_pane() {
        let mut app = app();
        assert_eq!(app.focus(), Focus::Sidebar);

        app.on_key(press(KeyCode::Char('R')));

        assert_eq!(app.focus(), Focus::Detail);
        assert!(app.is_typing_resource_sort());
        assert_eq!(app.node_resource_prompt(), Some(""));
    }

    #[test]
    fn typing_after_r_builds_up_the_resource_prompts_text() {
        let mut app = app();
        app.on_key(press(KeyCode::Char('R')));

        for c in "nvidia.com/gpu".chars() {
            app.on_key(press(KeyCode::Char(c)));
        }

        assert_eq!(app.node_resource_prompt(), Some("nvidia.com/gpu"));
    }

    #[test]
    fn enter_applies_the_resource_prompt_and_re_sorts_the_already_loaded_rows() {
        let mut app = app();
        app.apply_nodes(Ok(NodesFetch {
            rows: vec![
                node_row_with_device("roomy", "nvidia.com/gpu", "2", "16"),
                node_row_with_device("tight", "nvidia.com/gpu", "2", "2"),
            ],
            usage_note: None,
            requests_note: None,
        }));

        app.on_key(press(KeyCode::Char('R')));
        for c in "nvidia.com/gpu".chars() {
            app.on_key(press(KeyCode::Char(c)));
        }
        app.on_key(press(KeyCode::Enter));

        assert!(!app.is_typing_resource_sort());
        assert_eq!(app.node_resource_prompt(), None);
        let names: Vec<&str> = app
            .nodes()
            .rows()
            .iter()
            .map(|row| row.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["tight", "roomy"],
            "no fetch happened; the rows already on screen were re-sorted by device share"
        );
    }

    #[test]
    fn committing_an_empty_resource_prompt_leaves_it_inactive() {
        let mut app = app();
        app.on_key(press(KeyCode::Char('R')));
        app.on_key(press(KeyCode::Enter));

        assert_eq!(app.node_resource_prompt(), None);
        assert!(!app.is_typing_resource_sort());
    }

    #[test]
    fn esc_while_editing_the_resource_prompt_cancels_it_and_restores_the_fixed_order() {
        let mut app = app();
        app.apply_nodes(Ok(NodesFetch {
            rows: vec![
                node_row_with_device("roomy", "nvidia.com/gpu", "2", "16"),
                node_row_with_device("tight", "nvidia.com/gpu", "2", "2"),
            ],
            usage_note: None,
            requests_note: None,
        }));
        app.on_key(press(KeyCode::Char('R')));
        for c in "nvidia.com/gpu".chars() {
            app.on_key(press(KeyCode::Char(c)));
        }
        app.on_key(press(KeyCode::Enter));

        // Re-open the prompt, seeded with the applied name, and cancel it.
        app.on_key(press(KeyCode::Char('R')));
        assert_eq!(app.node_resource_prompt(), Some("nvidia.com/gpu"));
        app.on_key(press(KeyCode::Esc));

        assert_eq!(app.node_resource_prompt(), None);
        let names: Vec<&str> = app
            .nodes()
            .rows()
            .iter()
            .map(|row| row.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["roomy", "tight"],
            "cancelling the prompt must fall back to the fixed order (by name)"
        );
    }

    #[test]
    fn backspace_edits_the_resource_prompts_text() {
        let mut app = app();
        app.on_key(press(KeyCode::Char('R')));
        app.on_key(press(KeyCode::Char('x')));
        app.on_key(press(KeyCode::Char('y')));
        app.on_key(press(KeyCode::Backspace));

        assert_eq!(app.node_resource_prompt(), Some("x"));
    }

    #[test]
    fn s_reclaims_the_fixed_cycle_from_an_applied_resource_sort() {
        let mut app = app();
        app.apply_nodes(Ok(NodesFetch {
            rows: vec![node_row_with_device("worker-1", "nvidia.com/gpu", "2", "4")],
            usage_note: None,
            requests_note: None,
        }));
        app.on_key(press(KeyCode::Char('R')));
        for c in "nvidia.com/gpu".chars() {
            app.on_key(press(KeyCode::Char(c)));
        }
        app.on_key(press(KeyCode::Enter));

        app.on_key(press(KeyCode::Char('s')));

        assert_eq!(app.node_resource_prompt(), None);
        assert_eq!(app.node_order(), k8s_nodes::Order::Status);
    }

    #[test]
    fn shift_s_reverses_an_applied_resource_sort_without_clearing_it() {
        let mut app = app();
        app.apply_nodes(Ok(NodesFetch {
            rows: vec![
                node_row_with_device("roomy", "nvidia.com/gpu", "2", "16"),
                node_row_with_device("tight", "nvidia.com/gpu", "2", "2"),
            ],
            usage_note: None,
            requests_note: None,
        }));
        app.on_key(press(KeyCode::Char('R')));
        for c in "nvidia.com/gpu".chars() {
            app.on_key(press(KeyCode::Char(c)));
        }
        app.on_key(press(KeyCode::Enter));

        app.on_key(press(KeyCode::Char('S')));

        assert_eq!(app.node_direction(), SortDirection::Reversed);
        let names: Vec<&str> = app
            .nodes()
            .rows()
            .iter()
            .map(|row| row.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["roomy", "tight"],
            "reversing must keep the device ordering in charge rather than falling back to the \
             fixed cycle"
        );
    }

    /// A pod reporting one requested device, for the pod-drilldown pane's
    /// `--sort-resource` tests — the pod-panel counterpart to
    /// `node_row_with_device`.
    fn pod_row_with_device(name: &str, resource: &str, requested: &str) -> k8s_pods::PodRow {
        use crate::k8s::quantity::Quantity;

        let mut extended_requested = std::collections::BTreeMap::new();
        extended_requested.insert(resource.to_owned(), Quantity::parse(requested).unwrap());
        k8s_pods::PodRow {
            extended_requested,
            ..pod_row(name)
        }
    }

    #[test]
    fn r_opens_a_resource_sort_prompt_for_the_pod_drilldown_pane_too() {
        let mut app = app_with_node();
        app.on_key(press(KeyCode::Enter));
        assert!(matches!(app.view(), View::NodePods { .. }));
        app.apply_pods(Ok(PodsFetch {
            rows: vec![pod_row("api-1")],
            selector_note: None,
            usage_note: None,
        }));

        app.on_key(press(KeyCode::Char('R')));

        assert!(app.is_typing_resource_sort());
        assert_eq!(app.pod_resource_prompt(), Some(""));
        assert_eq!(
            app.node_resource_prompt(),
            None,
            "the node pane's own prompt must not also open"
        );
    }

    #[test]
    fn enter_applies_the_pod_panes_resource_prompt_and_re_sorts_the_already_loaded_rows() {
        let mut app = app_with_node();
        app.on_key(press(KeyCode::Enter));
        app.apply_pods(Ok(PodsFetch {
            rows: vec![
                pod_row_with_device("alpha", "nvidia.com/gpu", "1"),
                pod_row_with_device("bravo", "nvidia.com/gpu", "2"),
            ],
            selector_note: None,
            usage_note: None,
        }));

        app.on_key(press(KeyCode::Char('R')));
        for c in "nvidia.com/gpu".chars() {
            app.on_key(press(KeyCode::Char(c)));
        }
        app.on_key(press(KeyCode::Enter));

        assert!(!app.is_typing_resource_sort());
        assert_eq!(app.pod_resource_prompt(), None);
        let names: Vec<&str> = app
            .pods()
            .rows()
            .iter()
            .map(|row| row.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["bravo", "alpha"],
            "no fetch happened; the rows already on screen were re-sorted by device request"
        );
    }

    #[test]
    fn esc_while_editing_the_pod_panes_resource_prompt_cancels_it_and_restores_the_fixed_order() {
        let mut app = app_with_node();
        app.on_key(press(KeyCode::Enter));
        app.apply_pods(Ok(PodsFetch {
            rows: vec![
                pod_row_with_device("alpha", "nvidia.com/gpu", "1"),
                pod_row_with_device("bravo", "nvidia.com/gpu", "2"),
            ],
            selector_note: None,
            usage_note: None,
        }));
        app.on_key(press(KeyCode::Char('R')));
        for c in "nvidia.com/gpu".chars() {
            app.on_key(press(KeyCode::Char(c)));
        }
        app.on_key(press(KeyCode::Enter));

        // Re-open the prompt, seeded with the applied name, and cancel it.
        app.on_key(press(KeyCode::Char('R')));
        assert_eq!(app.pod_resource_prompt(), Some("nvidia.com/gpu"));
        app.on_key(press(KeyCode::Esc));

        assert_eq!(app.pod_resource_prompt(), None);
        let names: Vec<&str> = app
            .pods()
            .rows()
            .iter()
            .map(|row| row.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["alpha", "bravo"],
            "cancelling the prompt must fall back to the fixed order (by name)"
        );
    }

    #[test]
    fn s_reclaims_the_pod_panes_fixed_cycle_from_an_applied_resource_sort() {
        let mut app = app_with_node();
        app.on_key(press(KeyCode::Enter));
        app.apply_pods(Ok(PodsFetch {
            rows: vec![pod_row_with_device("alpha", "nvidia.com/gpu", "1")],
            selector_note: None,
            usage_note: None,
        }));
        app.on_key(press(KeyCode::Char('R')));
        for c in "nvidia.com/gpu".chars() {
            app.on_key(press(KeyCode::Char(c)));
        }
        app.on_key(press(KeyCode::Enter));

        app.on_key(press(KeyCode::Char('s')));

        assert_eq!(app.pod_resource_prompt(), None);
        assert_eq!(app.pod_order(), k8s_pods::Order::Restarts);
    }

    #[test]
    fn shift_s_reverses_the_pod_panes_applied_resource_sort_without_clearing_it() {
        let mut app = app_with_node();
        app.on_key(press(KeyCode::Enter));
        app.apply_pods(Ok(PodsFetch {
            rows: vec![
                pod_row_with_device("alpha", "nvidia.com/gpu", "1"),
                pod_row_with_device("bravo", "nvidia.com/gpu", "2"),
            ],
            selector_note: None,
            usage_note: None,
        }));
        app.on_key(press(KeyCode::Char('R')));
        for c in "nvidia.com/gpu".chars() {
            app.on_key(press(KeyCode::Char(c)));
        }
        app.on_key(press(KeyCode::Enter));

        app.on_key(press(KeyCode::Char('S')));

        assert_eq!(app.pod_direction(), SortDirection::Reversed);
        let names: Vec<&str> = app
            .pods()
            .rows()
            .iter()
            .map(|row| row.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["alpha", "bravo"],
            "reversing must keep the device ordering in charge rather than falling back to the \
             fixed cycle"
        );
    }

    #[test]
    fn r_is_harmless_in_the_pod_containers_pane() {
        // Neither pane with a device ordering — the node pane and the
        // pod-drilldown pane — is what is showing here.
        let mut app = app_with_pod();
        app.on_key(press(KeyCode::Enter));
        assert!(matches!(app.view(), View::PodContainers { .. }));

        app.on_key(press(KeyCode::Char('R')));

        assert!(!app.is_typing_resource_sort());
        assert_eq!(app.node_resource_prompt(), None);
        assert_eq!(app.pod_resource_prompt(), None);
    }

    #[test]
    fn a_quit_key_while_editing_the_resource_prompt_is_added_to_it_instead_of_arming_a_quit() {
        let mut app = app();
        app.on_key(press(KeyCode::Char('R')));

        assert_eq!(app.on_key(press(KeyCode::Char('q'))), Flow::Continue);

        assert_eq!(app.node_resource_prompt(), Some("q"));
        assert!(!app.quit_pending());
    }

    #[test]
    fn ctrl_c_still_quits_immediately_while_editing_the_resource_prompt() {
        let mut app = app();
        app.on_key(press(KeyCode::Char('R')));

        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Flow::Quit
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
    fn slash_opens_the_logs_panes_own_search_rather_than_the_row_filter() {
        let mut app = app_with_container();
        app.on_key(press(KeyCode::Enter)); // drills into the container's log
        assert!(matches!(app.view(), View::ContainerLogs { .. }));
        app.apply_log_event(LogEvent::Line("hello".to_owned()));

        app.on_key(press(KeyCode::Char('/')));

        // The row-list `Filter` is untouched — this pane has no rows — but
        // the logs pane's own search is now capturing keystrokes.
        assert!(!app.is_filtering());
        assert_eq!(app.filter_query(), "");
        assert!(app.is_searching_log());
    }

    #[test]
    fn slash_before_the_log_has_streamed_anything_does_not_open_a_search() {
        // There is no `Log` to hold the query yet — `LogsState` is still
        // `Loading` — so this is the one point in the drill-down where `/`
        // really is a no-op, same as the roadmap's original exception.
        let mut app = app_with_container();
        app.on_key(press(KeyCode::Enter));
        assert!(matches!(app.view(), View::ContainerLogs { .. }));

        app.on_key(press(KeyCode::Char('/')));

        assert!(!app.is_searching_log());
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

    // --- `l`/`F`: retyping the pod-drilldown pane's selectors ---

    #[test]
    fn l_opens_a_label_selector_prompt() {
        let mut app = app_with_pod();

        app.on_key(press(KeyCode::Char('l')));

        assert!(app.is_typing_selector());
        let prompt = app.pod_selector_prompt().unwrap();
        assert_eq!(prompt.field, pods::SelectorField::Label);
        assert_eq!(prompt.text, "");
        assert_eq!(prompt.error, None);
    }

    #[test]
    fn shift_f_opens_a_field_selector_prompt_distinct_from_l() {
        let mut app = app_with_pod();

        app.on_key(press(KeyCode::Char('F')));

        assert!(app.is_typing_selector());
        let prompt = app.pod_selector_prompt().unwrap();
        assert_eq!(prompt.field, pods::SelectorField::Field);
    }

    #[test]
    fn l_and_shift_f_do_nothing_off_the_node_pods_view() {
        let mut app = app_with_node();
        assert_eq!(*app.view(), View::Overview);

        app.on_key(press(KeyCode::Char('l')));
        assert!(!app.is_typing_selector());

        app.on_key(press(KeyCode::Char('F')));
        assert!(!app.is_typing_selector());
    }

    #[test]
    fn typing_after_l_builds_up_the_selector_prompts_text() {
        let mut app = app_with_pod();
        app.on_key(press(KeyCode::Char('l')));

        for c in "app=api".chars() {
            app.on_key(press(KeyCode::Char(c)));
        }

        assert_eq!(app.pod_selector_prompt().unwrap().text, "app=api");
    }

    #[test]
    fn backspace_erases_the_selector_prompts_last_character() {
        let mut app = app_with_pod();
        app.on_key(press(KeyCode::Char('l')));
        for c in "app=api".chars() {
            app.on_key(press(KeyCode::Char(c)));
        }

        app.on_key(press(KeyCode::Backspace));

        assert_eq!(app.pod_selector_prompt().unwrap().text, "app=ap");
    }

    #[test]
    fn enter_commits_a_valid_label_selector() {
        let mut app = app_with_pod();
        app.on_key(press(KeyCode::Char('l')));
        for c in "app=api".chars() {
            app.on_key(press(KeyCode::Char(c)));
        }

        app.on_key(press(KeyCode::Enter));

        assert!(!app.is_typing_selector());
        assert_eq!(app.pod_selector_prompt(), None);
        assert_eq!(app.pod_selectors().label.as_deref(), Some("app=api"));
    }

    #[test]
    fn enter_on_a_bad_selector_keeps_editing_with_its_own_rejection_shown() {
        let mut app = app_with_pod();
        app.on_key(press(KeyCode::Char('l')));
        for c in "app in".chars() {
            app.on_key(press(KeyCode::Char(c)));
        }

        app.on_key(press(KeyCode::Enter));

        assert!(
            app.is_typing_selector(),
            "a rejected selector must not be lost"
        );
        let prompt = app.pod_selector_prompt().unwrap();
        assert_eq!(prompt.text, "app in", "the offending text stays as typed");
        assert!(prompt.error.is_some());
        assert_eq!(
            app.pod_selectors().label,
            None,
            "nothing was ever committed"
        );
    }

    #[test]
    fn a_keystroke_after_a_rejection_clears_the_old_error() {
        let mut app = app_with_pod();
        app.on_key(press(KeyCode::Char('l')));
        for c in "app in".chars() {
            app.on_key(press(KeyCode::Char(c)));
        }
        app.on_key(press(KeyCode::Enter));
        assert!(app.pod_selector_prompt().unwrap().error.is_some());

        app.on_key(press(KeyCode::Backspace));

        assert_eq!(app.pod_selector_prompt().unwrap().error, None);
    }

    #[test]
    fn esc_while_editing_a_selector_cancels_it_without_changing_the_applied_one() {
        let mut app = app_with_pod();
        app.on_key(press(KeyCode::Char('l')));
        for c in "app=api".chars() {
            app.on_key(press(KeyCode::Char(c)));
        }
        app.on_key(press(KeyCode::Enter));

        // Re-open the prompt, seeded with the applied text, retype it, and
        // cancel — the applied selector must survive untouched.
        app.on_key(press(KeyCode::Char('l')));
        assert_eq!(app.pod_selector_prompt().unwrap().text, "app=api");
        app.on_key(press(KeyCode::Char('x')));
        app.on_key(press(KeyCode::Esc));

        assert!(!app.is_typing_selector());
        assert_eq!(app.pod_selectors().label.as_deref(), Some("app=api"));
    }

    #[test]
    fn committing_a_field_selector_does_not_disturb_an_already_applied_label_selector() {
        let mut app = app_with_pod();
        app.on_key(press(KeyCode::Char('l')));
        for c in "app=api".chars() {
            app.on_key(press(KeyCode::Char(c)));
        }
        app.on_key(press(KeyCode::Enter));

        app.on_key(press(KeyCode::Char('F')));
        for c in "status.phase=Running".chars() {
            app.on_key(press(KeyCode::Char(c)));
        }
        app.on_key(press(KeyCode::Enter));

        assert_eq!(app.pod_selectors().label.as_deref(), Some("app=api"));
        assert_eq!(
            app.pod_selectors().field.as_deref(),
            Some("status.phase=Running")
        );
    }

    #[test]
    fn committing_an_empty_selector_prompt_clears_it_rather_than_rejecting_it() {
        let mut app = app_with_pod();
        app.on_key(press(KeyCode::Char('l')));
        for c in "app=api".chars() {
            app.on_key(press(KeyCode::Char(c)));
        }
        app.on_key(press(KeyCode::Enter));

        // Retype it away to nothing and commit — a blank selector filters
        // nothing, the same rule `selectors_for` gives `-l ''` on the CLI.
        app.on_key(press(KeyCode::Char('l')));
        for _ in 0.."app=api".len() {
            app.on_key(press(KeyCode::Backspace));
        }
        app.on_key(press(KeyCode::Enter));

        assert!(!app.is_typing_selector());
        assert_eq!(app.pod_selectors().label, None);
    }

    #[test]
    fn the_footer_switches_to_selector_hints_while_the_prompt_is_editing() {
        let mut app = app_with_pod();
        app.on_key(press(KeyCode::Char('l')));

        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let rendered = terminal.backend().to_string();

        assert!(rendered.contains("type"), "{rendered}");
        assert!(rendered.contains("selector"), "{rendered}");
        assert!(!rendered.contains("sort"), "{rendered}");
    }

    #[test]
    fn a_quit_key_while_editing_a_selector_is_added_to_it_instead_of_arming_a_quit() {
        let mut app = app_with_pod();
        app.on_key(press(KeyCode::Char('l')));

        assert_eq!(app.on_key(press(KeyCode::Char('q'))), Flow::Continue);

        assert_eq!(app.pod_selector_prompt().unwrap().text, "q");
        assert!(!app.quit_pending());
    }

    #[test]
    fn ctrl_c_still_quits_immediately_while_editing_a_selector() {
        let mut app = app_with_pod();
        app.on_key(press(KeyCode::Char('l')));

        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Flow::Quit
        );
    }

    // --- `x`: a shell from the dashboard ---

    fn refused_exec(message: &str) -> FetchError {
        FetchError {
            message: message.to_owned(),
            credentials: false,
        }
    }

    fn footer_of(app: &App, width: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let screen = terminal.backend().to_string();
        screen.lines().last().unwrap_or_default().to_owned()
    }

    #[test]
    fn x_in_the_containers_pane_asks_for_the_highlighted_container() {
        let mut app = app_with_container();

        assert_eq!(
            app.on_key(press(KeyCode::Char('x'))),
            Flow::Exec(ExecTarget {
                namespace: "default".to_owned(),
                pod: "api-1".to_owned(),
                container: Some("app".to_owned()),
            })
        );
        assert!(app.exec_preparing());
    }

    #[test]
    fn x_on_a_pod_leaves_the_container_to_the_pods_default() {
        let mut app = app_with_pod();

        assert_eq!(
            app.on_key(press(KeyCode::Char('x'))),
            Flow::Exec(ExecTarget {
                namespace: "default".to_owned(),
                pod: "api-1".to_owned(),
                container: None,
            })
        );
    }

    #[test]
    fn x_in_a_log_asks_for_the_container_whose_log_it_is() {
        let mut app = app_with_container();
        app.on_key(press(KeyCode::Enter));
        assert!(matches!(app.view(), View::ContainerLogs { .. }));

        let flow = app.on_key(press(KeyCode::Char('x')));

        assert!(
            matches!(&flow, Flow::Exec(target) if target.container.as_deref() == Some("app")),
            "{flow:?}"
        );
    }

    #[test]
    fn x_follows_the_filter_to_the_row_actually_highlighted() {
        let mut app = app_with_pod();
        app.apply_pods(Ok(PodsFetch {
            rows: vec![pod_row("api-1"), pod_row("worker-1")],
            selector_note: None,
            usage_note: None,
        }));
        app.on_key(press(KeyCode::Char('/')));
        for c in "work".chars() {
            app.on_key(press(KeyCode::Char(c)));
        }
        app.on_key(press(KeyCode::Enter));

        let flow = app.on_key(press(KeyCode::Char('x')));

        assert!(
            matches!(&flow, Flow::Exec(target) if target.pod == "worker-1"),
            "{flow:?}"
        );
    }

    #[test]
    fn x_with_the_sidebar_focused_says_how_to_reach_the_list() {
        let mut app = app_with_container();
        app.toggle_focus();
        assert_eq!(app.focus(), Focus::Sidebar);

        assert_eq!(app.on_key(press(KeyCode::Char('x'))), Flow::Continue);

        let lines = app.status_lines();
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0]
                .to_string()
                .contains("Press tab to move to the container list"),
            "{}",
            lines[0]
        );
    }

    #[test]
    fn x_does_nothing_on_the_node_pane() {
        let mut app = app_with_node();
        app.toggle_focus();

        assert_eq!(app.on_key(press(KeyCode::Char('x'))), Flow::Continue);
        assert_eq!(app.status_lines(), Vec::<Line>::new());
    }

    #[test]
    fn x_does_nothing_while_the_pod_list_is_still_loading() {
        let mut app = app_with_node();
        app.on_key(press(KeyCode::Enter));
        assert!(matches!(app.view(), View::NodePods { .. }));

        assert_eq!(app.on_key(press(KeyCode::Char('x'))), Flow::Continue);
        assert!(!app.exec_preparing());
    }

    #[test]
    fn x_is_filter_text_while_the_filter_is_capturing() {
        let mut app = app_with_container();
        app.on_key(press(KeyCode::Char('/')));

        assert_eq!(app.on_key(press(KeyCode::Char('x'))), Flow::Continue);
        assert_eq!(app.filter_query(), "x");
        assert!(!app.exec_preparing());
    }

    #[test]
    fn esc_cancels_a_pending_shell_before_it_backs_out_of_anything() {
        let mut app = app_with_container();
        app.on_key(press(KeyCode::Char('x')));

        assert_eq!(app.on_key(press(KeyCode::Esc)), Flow::Continue);

        assert!(!app.exec_preparing());
        assert!(matches!(app.view(), View::PodContainers { .. }));
    }

    #[test]
    fn moving_the_highlight_keeps_the_pending_shell() {
        let mut app = app_with_container();
        app.on_key(press(KeyCode::Char('x')));

        app.on_key(press(KeyCode::Char('j')));

        assert!(app.exec_preparing());
    }

    #[test]
    fn switching_clusters_cancels_a_pending_shell() {
        let mut app = App::new(vec![cluster("prod", true), cluster("staging", false)]);
        app.apply_nodes(Ok(NodesFetch {
            rows: vec![node_row("ip-10-0-1-12")],
            ..NodesFetch::default()
        }));
        app.on_key(press(KeyCode::Tab));
        app.on_key(press(KeyCode::Enter));
        app.apply_pods(Ok(PodsFetch {
            rows: vec![pod_row("api-1")],
            selector_note: None,
            usage_note: None,
        }));
        app.on_key(press(KeyCode::Char('x')));
        assert!(app.exec_preparing());

        app.toggle_focus();
        app.on_key(press(KeyCode::Char('j')));

        assert!(!app.exec_preparing());
    }

    #[test]
    fn a_refusal_stays_on_the_status_line_until_the_next_key() {
        let mut app = app_with_container();
        app.on_key(press(KeyCode::Char('x')));
        app.apply_exec_refusal(refused_exec(
            "container app in pod api-1 is not running (CrashLoopBackOff).\n\
             Open its log and press p to see how its last run ended.",
        ));

        let lines: Vec<String> = app.status_lines().iter().map(ToString::to_string).collect();
        assert_eq!(
            lines,
            [
                "container app in pod api-1 is not running (CrashLoopBackOff).",
                "Open its log and press p to see how its last run ended.",
            ]
        );
        assert!(!app.exec_preparing());

        app.on_key(press(KeyCode::Char('k')));
        assert_eq!(app.status_lines(), Vec::<Line>::new());
    }

    #[test]
    fn a_credential_refusal_offers_l_without_withdrawing_anything() {
        let mut app = app_with_container();
        app.apply_exec_refusal(FetchError {
            message: "prod rejected your credentials.".to_owned(),
            credentials: true,
        });
        assert!(app.credentials_lost());

        // And a refusal for any other reason leaves an offer another pane
        // made where it was.
        app.apply_exec_refusal(refused_exec("pod api-1 has no shell."));
        assert!(app.credentials_lost());
    }

    #[test]
    fn a_shell_that_exited_leaves_nothing_on_the_status_line() {
        let mut app = app_with_container();
        app.on_key(press(KeyCode::Char('x')));

        app.apply_exec_ending(Ok(()));

        assert_eq!(app.status_lines(), Vec::<Line>::new());
        assert!(!app.exec_preparing());
    }

    #[test]
    fn the_footer_offers_x_where_there_is_a_pod_or_container_to_open() {
        let mut app = app_with_node();
        assert!(!footer_of(&app, 200).contains("x shell"));

        app = app_with_pod();
        assert!(footer_of(&app, 200).contains("x shell"));

        app = app_with_container();
        assert!(footer_of(&app, 200).contains("x shell"));

        app.on_key(press(KeyCode::Enter));
        assert!(footer_of(&app, 200).contains("x shell"));
    }

    #[test]
    fn the_containers_pane_offers_x_in_place_of_a_sort_it_does_not_have() {
        let footer = footer_of(&app_with_container(), 100);
        assert!(footer.contains("x shell"), "{footer}");
        assert!(!footer.contains("s/S"), "{footer}");
    }

    #[test]
    fn on_a_pod_x_comes_after_quit_so_a_narrow_footer_clips_it_first() {
        let footer = footer_of(&app_with_pod(), 200);
        let quit = footer.find("q quit").unwrap();
        let shell = footer.find("x shell").unwrap();
        assert!(quit < shell, "{footer}");
    }

    #[test]
    fn the_status_line_takes_no_room_when_there_is_nothing_to_say() {
        assert_eq!(status_height(&[], 100, 24), 0);
    }

    #[test]
    fn the_status_line_grows_with_each_wrapped_line() {
        // 150 characters in the 99 columns left of a 100-column terminal.
        let lines = [Line::raw("x".repeat(150)), Line::raw("short")];
        assert_eq!(status_height(&lines, 100, 24), 3);
    }

    #[test]
    fn the_status_line_never_takes_more_than_a_third_of_the_screen() {
        let lines: Vec<Line> = (0..20).map(|n| Line::raw(format!("line {n}"))).collect();
        assert_eq!(status_height(&lines, 100, 24), 8);
        // And always a row, however small the terminal, so a refusal is
        // never silently invisible.
        assert_eq!(status_height(&lines, 100, 2), 1);
    }

    #[test]
    fn the_status_line_takes_nothing_from_a_terminal_with_no_width() {
        assert_eq!(status_height(&[Line::raw("x")], 0, 24), 0);
        assert_eq!(status_height(&[Line::raw("x")], 1, 24), 0);
    }

    #[test]
    fn a_refusal_draws_on_a_one_by_one_terminal_without_panicking() {
        let mut app = app_with_container();
        app.apply_exec_refusal(refused_exec(
            "pod api-1 has no shell.\nRun a command instead.",
        ));
        let mut terminal = Terminal::new(TestBackend::new(1, 1)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
    }

    #[test]
    fn a_container_s_ports_are_rows_the_highlight_moves_through() {
        let mut app = app_with_ports();
        assert_eq!(app.detail_row_count(), 4);
        app.on_key(press(KeyCode::End));
        assert_eq!(app.detail_selected(), 3);
        app.on_key(press(KeyCode::Char('j')));
        assert_eq!(app.detail_selected(), 0, "the highlight still wraps");
    }

    #[test]
    fn f_on_a_port_starts_a_forward_to_that_port_of_that_pod() {
        let mut app = app_with_ports();
        app.on_key(press(KeyCode::Char('j')));
        let flow = app.on_key(press(KeyCode::Char('f')));
        assert_eq!(
            flow,
            Flow::Forward(ForwardRequest {
                id: 0,
                context: BETA.to_owned(),
                target: web_port(),
            })
        );
        assert_eq!(app.forwards().running(), 1);
        assert_eq!(app.forwards().all()[0].cluster, "beta");
    }

    #[test]
    fn f_on_a_port_already_forwarded_starts_nothing_and_says_where_it_is() {
        let (id, mut app) = app_forwarding();
        app.apply_forward_event(
            id,
            ForwardEvent::Listening {
                url: "http://127.0.0.1:8080".to_owned(),
                notes: Vec::new(),
            },
        );
        assert_eq!(app.on_key(press(KeyCode::Char('f'))), Flow::Continue);
        assert_eq!(app.forwards().all().len(), 1);
        assert_eq!(
            status_text(&app),
            "Port 8080 of pod api-1 is already forwarded to http://127.0.0.1:8080. Press F to stop it."
        );
    }

    #[test]
    fn f_on_a_container_with_ports_says_to_move_to_one() {
        let mut app = app_with_ports();
        assert_eq!(app.on_key(press(KeyCode::Char('f'))), Flow::Continue);
        assert_eq!(
            status_text(&app),
            "Move down to one of web's ports, then press f to forward it."
        );
        assert!(app.forwards().all().is_empty());
    }

    #[test]
    fn f_on_a_container_without_ports_says_how_to_forward_one_anyway() {
        let mut app = app_with_ports();
        app.on_key(press(KeyCode::End));
        app.on_key(press(KeyCode::Char('f')));
        let text = status_text(&app);
        assert!(
            text.starts_with("Container worker declares no ports."),
            "{text}"
        );
        assert!(
            text.contains("`eks port-forward api-1 -n default PORT`"),
            "{text}"
        );
    }

    #[test]
    fn f_on_a_udp_port_says_a_forward_carries_tcp_only() {
        let mut app = app_with_ports();
        app.on_key(press(KeyCode::Char('j')));
        app.on_key(press(KeyCode::Char('j')));
        assert_eq!(app.on_key(press(KeyCode::Char('f'))), Flow::Continue);
        assert_eq!(
            status_text(&app),
            "Port 53 is UDP, and a port forward carries TCP only."
        );
        assert!(app.forwards().all().is_empty());
    }

    #[test]
    fn f_with_the_sidebar_focused_says_to_move_to_the_list_first() {
        let mut app = app_with_ports();
        app.toggle_focus();
        assert_eq!(app.on_key(press(KeyCode::Char('f'))), Flow::Continue);
        assert!(status_text(&app).starts_with("Press tab to move to the container list"));
    }

    #[test]
    fn f_and_capital_f_mean_nothing_on_the_node_pane() {
        let mut app = app_with_node();
        assert_eq!(app.on_key(press(KeyCode::Char('f'))), Flow::Continue);
        assert_eq!(app.on_key(press(KeyCode::Char('F'))), Flow::Continue);
        assert!(status_text(&app).is_empty());
        assert!(app.forwards().all().is_empty());
    }

    #[test]
    fn capital_f_on_the_pod_list_still_retypes_the_field_selector() {
        let mut app = app_with_pod();
        app.on_key(press(KeyCode::Char('F')));
        assert!(app.is_typing_selector());
    }

    #[test]
    fn f_in_the_log_pane_still_toggles_following() {
        let mut app = app_with_container();
        app.on_key(press(KeyCode::Enter));
        app.apply_log_event(LogEvent::Line("one".to_owned()));
        let before = streaming(app.logs()).follow();
        app.on_key(press(KeyCode::Char('f')));
        assert_ne!(streaming(app.logs()).follow(), before);
        assert!(app.forwards().all().is_empty());
    }

    #[test]
    fn capital_f_on_a_forwarded_port_stops_it() {
        let (id, mut app) = app_forwarding();
        assert!(app.wants_forward(id));
        assert_eq!(app.on_key(press(KeyCode::Char('F'))), Flow::Continue);
        assert!(!app.wants_forward(id));
        assert!(app.forwards().all().is_empty());
        assert!(status_text(&app).is_empty());
    }

    #[test]
    fn capital_f_dismisses_a_forward_that_stopped_by_itself() {
        let (id, mut app) = app_forwarding();
        app.apply_forward_event(
            id,
            ForwardEvent::Ended {
                message: "pod api-1 was deleted.".to_owned(),
                credentials: false,
            },
        );
        app.on_key(press(KeyCode::Char('F')));
        assert!(app.forwards().all().is_empty());
    }

    #[test]
    fn capital_f_on_a_port_with_no_forward_says_how_to_start_one() {
        let mut app = app_with_ports();
        app.on_key(press(KeyCode::Char('j')));
        app.on_key(press(KeyCode::Char('F')));
        assert_eq!(
            status_text(&app),
            "Port 8080 is not being forwarded. Press f to forward it."
        );
    }

    #[test]
    fn capital_f_on_a_container_says_to_move_to_a_forwarded_port() {
        let mut app = app_with_ports();
        app.on_key(press(KeyCode::Char('F')));
        assert_eq!(
            status_text(&app),
            "Move down to a forwarded port, then press F to stop its forward."
        );
    }

    #[test]
    fn the_next_key_clears_a_forwarding_note() {
        let mut app = app_with_ports();
        app.on_key(press(KeyCode::Char('f')));
        assert!(!status_text(&app).is_empty());
        app.on_key(press(KeyCode::Char('j')));
        assert!(status_text(&app).is_empty());
    }

    #[test]
    fn enter_on_a_port_opens_the_log_of_the_container_it_belongs_to() {
        let mut app = app_with_ports();
        app.on_key(press(KeyCode::Char('j')));
        app.on_key(press(KeyCode::Enter));
        assert!(matches!(
            app.view(),
            View::ContainerLogs { container, .. } if container == "web"
        ));
    }

    #[test]
    fn x_on_a_port_opens_a_shell_in_the_container_it_belongs_to() {
        let mut app = app_with_ports();
        app.on_key(press(KeyCode::Char('j')));
        assert_eq!(
            app.on_key(press(KeyCode::Char('x'))),
            Flow::Exec(ExecTarget {
                namespace: "default".to_owned(),
                pod: "api-1".to_owned(),
                container: Some("web".to_owned()),
            })
        );
    }

    #[test]
    fn a_forward_outlives_the_pane_and_the_cluster_it_was_started_from() {
        let (id, mut app) = app_forwarding();
        app.on_key(press(KeyCode::Esc));
        app.on_key(press(KeyCode::Esc));
        app.leave_detail_view();
        app.select_next();
        assert!(app.wants_forward(id));
        assert_eq!(app.forwards().running(), 1);
    }

    #[test]
    fn the_port_row_marks_only_the_pod_on_screen() {
        let (id, mut app) = app_forwarding();
        app.apply_forward_event(
            id,
            ForwardEvent::Listening {
                url: "http://127.0.0.1:8080".to_owned(),
                notes: Vec::new(),
            },
        );
        assert_eq!(
            app.port_marks().get(&8080).map(|(text, _)| text.as_str()),
            Some("→ http://127.0.0.1:8080")
        );
        app.on_key(press(KeyCode::Esc));
        assert!(app.port_marks().is_empty(), "the pod list has no port rows");
    }

    #[test]
    fn a_forward_refused_for_credentials_offers_l_and_a_login_retries_it() {
        let (id, mut app) = app_forwarding();
        app.apply_forward_event(
            id,
            ForwardEvent::Ended {
                message: "beta rejected your credentials.".to_owned(),
                credentials: true,
            },
        );
        assert!(app.credentials_lost());

        let retried = app.retry_forwards_after_login();
        assert_eq!(retried.len(), 1);
        assert_eq!(retried[0].context, BETA);
        assert_eq!(retried[0].target, web_port());
        assert!(app.wants_forward(retried[0].id));
    }

    #[test]
    fn a_credential_failure_on_another_cluster_s_forward_neither_offers_l_nor_is_retried() {
        let (id, mut app) = app_forwarding();
        app.select_next();
        app.apply_forward_event(
            id,
            ForwardEvent::Ended {
                message: "beta rejected your credentials.".to_owned(),
                credentials: true,
            },
        );
        assert!(
            !app.credentials_lost(),
            "L would log in to gamma's profile for beta's failure"
        );
        assert!(app.retry_forwards_after_login().is_empty());
    }

    #[test]
    fn c_clears_a_stopped_forward_from_any_pane() {
        let (lost, mut app) = app_forwarding();
        app.apply_forward_event(
            lost,
            ForwardEvent::Ended {
                message: "pod api-1 was deleted.".to_owned(),
                credentials: false,
            },
        );
        // Backed all the way out, where no port row is on screen to press
        // `F` on.
        app.on_key(press(KeyCode::Esc));
        app.on_key(press(KeyCode::Esc));
        assert!(render_app(&app, 120, 30).contains("c clears stopped"));

        app.on_key(press(KeyCode::Char('c')));
        assert!(app.forwards().all().is_empty());
        assert!(!render_app(&app, 120, 30).contains("Forwards"));
    }

    #[test]
    fn c_leaves_a_running_forward_alone() {
        let (id, mut app) = app_forwarding();
        app.on_key(press(KeyCode::Char('c')));
        assert!(app.wants_forward(id));
        assert!(!render_app(&app, 120, 30).contains("c clears stopped"));
    }

    #[test]
    fn a_forward_whose_thread_is_lost_says_so_and_how_to_retry() {
        let (id, mut app) = app_forwarding();
        app.apply_forward_lost(id);
        assert!(!app.wants_forward(id));
        assert!(matches!(
            &app.forwards().all()[0].state,
            forwards::State::Ended { message, .. } if message.contains("Press f on its port")
        ));
    }

    fn render_app(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        terminal.backend().to_string()
    }

    #[test]
    fn the_forwards_strip_appears_once_something_is_forwarded_and_stays_on_every_view() {
        assert!(!render_app(&app_with_ports(), 100, 24).contains("Forwards"));

        let (id, mut app) = app_forwarding();
        app.apply_forward_event(
            id,
            ForwardEvent::Listening {
                url: "http://127.0.0.1:8080".to_owned(),
                notes: Vec::new(),
            },
        );
        let screen = render_app(&app, 100, 24);
        assert!(screen.contains("Forwards"), "{screen}");
        assert!(
            screen.contains("http://127.0.0.1:8080 → pod api-1 port 8080  no connections"),
            "{screen}"
        );

        app.on_key(press(KeyCode::Esc));
        app.on_key(press(KeyCode::Esc));
        assert_eq!(app.view(), &View::Overview);
        assert!(render_app(&app, 100, 24).contains("http://127.0.0.1:8080 → pod api-1"));
    }

    #[test]
    fn the_strip_says_its_forwards_end_when_the_dashboard_does() {
        let (_, app) = app_forwarding();
        assert!(render_app(&app, 80, 24).contains("Forwards · they end when eks quits"));
    }

    #[test]
    fn an_armed_quit_with_forwards_running_says_it_will_end_them() {
        let (_, mut app) = app_forwarding();
        app.on_key(press(KeyCode::Esc));
        app.on_key(press(KeyCode::Esc));
        app.on_key(press(KeyCode::Esc));
        app.on_key(press(KeyCode::Char('q')));
        assert!(
            render_app(&app, 80, 24).contains("press esc/q again to quit and end the port forward")
        );
    }

    #[test]
    fn an_armed_quit_does_not_count_a_forward_that_already_stopped() {
        let (id, mut app) = app_forwarding();
        app.apply_forward_event(
            id,
            ForwardEvent::Ended {
                message: "gone".to_owned(),
                credentials: false,
            },
        );
        app.on_key(press(KeyCode::Esc));
        app.on_key(press(KeyCode::Esc));
        app.on_key(press(KeyCode::Esc));
        app.on_key(press(KeyCode::Char('q')));
        let screen = render_app(&app, 80, 24);
        assert!(screen.contains("press esc/q again to quit"), "{screen}");
        assert!(!screen.contains("port forward"), "{screen}");
    }

    #[test]
    fn the_armed_quit_counts_the_forwards_it_will_end() {
        assert_eq!(quit_warning(0), "press esc/q again to quit");
        assert_eq!(
            quit_warning(1),
            "press esc/q again to quit and end the port forward"
        );
        assert_eq!(
            quit_warning(3),
            "press esc/q again to quit and end 3 port forwards"
        );
    }

    #[test]
    fn the_footer_offers_f_only_where_there_is_a_port() {
        assert!(render_app(&app_with_ports(), 160, 24).contains("f/F forward/stop"));
        assert!(!render_app(&app_with_container(), 160, 24).contains("f/F"));
        assert!(!render_app(&app_with_pod(), 160, 24).contains("f/F"));
    }

    #[test]
    fn the_strip_never_takes_more_than_a_third_of_the_screen() {
        let lines: Vec<Line<'_>> = (0..20).map(|n| Line::raw(n.to_string())).collect();
        assert_eq!(strip_height(&lines, 80, 24), 8);
        assert_eq!(strip_height(&[], 80, 24), 0);
        assert_eq!(strip_height(&lines, 80, 1), 1);
    }

    #[test]
    fn a_one_by_one_terminal_with_forwards_draws_without_panicking() {
        let (_, app) = app_forwarding();
        render_app(&app, 1, 1);
        // Too short for the strip to have a row of its own beside the body.
        render_app(&app, 30, 3);
    }

    mod event_loop;
    mod golden;
}
