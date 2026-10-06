//! The dashboard's port forwards: which are running, what each has said, and
//! the strip that lists them.
//!
//! A forward is started from a port in the pod-containers pane and outlives
//! that pane: it runs for as long as the dashboard is open, whichever cluster
//! or pane is on screen, so its state lives here rather than in a pane's.
//! The forward itself runs on its own thread
//! ([`crate::commands::forward::spawn_dashboard`]) and reports through
//! [`Event`]s; [`Forwards::apply`] is the state change each one makes, and
//! [`Forwards::strip`] is the text the strip draws, both pure so neither
//! needs a socket to test.

use ratatui::text::{Line, Span};

use crate::k8s::forward::{Event, PodPort};
use crate::theme::{Severity, Theme};

/// Identifies one forward between `App` and the event loop holding its
/// thread. Never reused within a session, so an event from a forward that
/// was stopped and started again cannot land on its successor.
pub(super) type Id = u64;

/// Where one forward has got to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Connecting, checking the pod, and binding: nothing to click yet.
    Starting,
    /// Listening on `url`. `notes` say why the local port is not the pod's
    /// own number, when it is not.
    Listening { url: String, notes: Vec<String> },
    /// Stopped by itself — it could not start, or its pod went away — with
    /// the reason. Kept on the strip until it is started again or dismissed,
    /// so a forward that died while nobody was looking still says why.
    Ended { message: String, credentials: bool },
}

/// One forward, as the strip and the port it belongs to show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forward {
    pub id: Id,
    /// The kubeconfig context it reaches the cluster through.
    pub context: String,
    /// The cluster's short name, for the strip when another one is selected.
    pub cluster: String,
    pub target: PodPort,
    pub state: State,
    /// Local connections open right now.
    pub connections: usize,
    /// The last thing that went wrong without ending it. Cleared by nothing
    /// but a newer one: "last error" is what the strip promises.
    pub problem: Option<String>,
}

impl Forward {
    /// Whether it is still running, or still trying to.
    #[must_use]
    pub fn is_running(&self) -> bool {
        !matches!(self.state, State::Ended { .. })
    }

    /// The few words a port's own row shows about its forward.
    #[must_use]
    pub fn mark(&self) -> (String, Severity) {
        match &self.state {
            State::Starting => ("starting forward…".to_owned(), Severity::Unknown),
            State::Listening { url, .. } => (format!("→ {url}"), Severity::Ok),
            State::Ended { .. } => ("forward stopped".to_owned(), Severity::Critical),
        }
    }
}

/// A forward for the event loop to start: which, on which context, to where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardRequest {
    pub id: Id,
    pub context: String,
    pub target: PodPort,
}

/// What `f` on a port came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Started {
    /// A new forward, which the event loop should now start.
    New(Id),
    /// That port of that pod is forwarded already; the URL, once it has one.
    Already(Option<String>),
}

/// Every forward this dashboard has started and not dismissed, in the order
/// they were started.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Forwards {
    entries: Vec<Forward>,
    next_id: Id,
}

impl Forwards {
    /// Every forward, oldest first.
    #[must_use]
    pub fn all(&self) -> &[Forward] {
        &self.entries
    }

    /// How many are still running — what quitting would end.
    #[must_use]
    pub fn running(&self) -> usize {
        self.entries.iter().filter(|f| f.is_running()).count()
    }

    /// How many have stopped by themselves and are still listed.
    #[must_use]
    pub fn stopped(&self) -> usize {
        self.entries.len() - self.running()
    }

    /// Take every stopped forward off the strip — `c`, from any pane. The
    /// way to dismiss one whose pod is gone, whose port row went with it.
    pub fn clear_stopped(&mut self) {
        self.entries.retain(Forward::is_running);
    }

    /// The forward for this port of this pod on this context, running or
    /// ended.
    #[must_use]
    pub fn find(&self, context: &str, target: &PodPort) -> Option<&Forward> {
        self.entries
            .iter()
            .find(|f| f.context == context && f.target == *target)
    }

    /// Whether the forward with this id still wants its thread. `false` once
    /// it has ended or been stopped, which is the event loop's cue to drop
    /// the thread's handle.
    #[must_use]
    pub fn wants(&self, id: Id) -> bool {
        self.entries.iter().any(|f| f.id == id && f.is_running())
    }

    /// Start forwarding `target`, unless it is already.
    ///
    /// Ports are per pod, not per container — a pod's containers share one
    /// network namespace — so the same port reached from two containers'
    /// rows is the same forward. One that ended is replaced: starting it
    /// again is how it is retried.
    pub fn start(&mut self, context: &str, cluster: &str, target: PodPort) -> Started {
        if let Some(existing) = self.find(context, &target) {
            if existing.is_running() {
                let url = match &existing.state {
                    State::Listening { url, .. } => Some(url.clone()),
                    State::Starting | State::Ended { .. } => None,
                };
                return Started::Already(url);
            }
            self.entries
                .retain(|f| !(f.context == context && f.target == target));
        }
        let id = self.next_id;
        self.next_id += 1;
        self.entries.push(Forward {
            id,
            context: context.to_owned(),
            cluster: cluster.to_owned(),
            target,
            state: State::Starting,
            connections: 0,
            problem: None,
        });
        Started::New(id)
    }

    /// Stop the forward for this port, or dismiss it if it had already
    /// ended. Returns whether there was one.
    pub fn stop(&mut self, context: &str, target: &PodPort) -> bool {
        let before = self.entries.len();
        self.entries
            .retain(|f| !(f.context == context && f.target == *target));
        self.entries.len() != before
    }

    /// Apply what forward `id` reported. An event for a forward no longer
    /// listed — stopped while the event was in flight — is dropped.
    pub fn apply(&mut self, id: Id, event: Event) {
        let Some(forward) = self.entries.iter_mut().find(|f| f.id == id) else {
            return;
        };
        match event {
            Event::Listening { url, notes } => forward.state = State::Listening { url, notes },
            Event::Opened => forward.connections += 1,
            Event::Closed => forward.connections = forward.connections.saturating_sub(1),
            Event::Problem(message) => forward.problem = Some(message),
            Event::Ended {
                message,
                credentials,
            } => {
                forward.state = State::Ended {
                    message,
                    credentials,
                };
                // Its listener is closed, and every connection with it.
                forward.connections = 0;
            }
        }
    }

    /// After a successful `L` on `context`: put every forward there that
    /// ended for want of credentials back to starting, and return them for
    /// the event loop to start again — `L` promises to retry what failed, and
    /// a forward is one of the things that can have.
    pub fn retry_after_login(&mut self, context: &str) -> Vec<ForwardRequest> {
        let mut retried = Vec::new();
        for forward in &mut self.entries {
            if forward.context == context
                && matches!(
                    forward.state,
                    State::Ended {
                        credentials: true,
                        ..
                    }
                )
            {
                // A fresh id, so a late event from the attempt that failed
                // cannot be mistaken for one from this one.
                forward.id = self.next_id;
                self.next_id += 1;
                forward.state = State::Starting;
                forward.problem = None;
                retried.push(ForwardRequest {
                    id: forward.id,
                    context: forward.context.clone(),
                    target: forward.target.clone(),
                });
            }
        }
        retried
    }

    /// The strip's lines: one per forward, then whatever it has to say
    /// underneath — the reason it ended, or its last error.
    ///
    /// `selected` is the context the dashboard is showing; a forward on any
    /// other cluster is prefixed with that cluster's name, since its pod name
    /// alone would read as one of this cluster's.
    #[must_use]
    pub fn strip(&self, selected: Option<&str>, theme: Theme) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for forward in &self.entries {
            lines.push(headline(forward, selected, theme));
            let (said, severity) = match &forward.state {
                State::Ended { message, .. } => (Some(message), Severity::Critical),
                State::Starting | State::Listening { .. } => {
                    (forward.problem.as_ref(), Severity::Warn)
                }
            };
            if let Some(said) = said {
                lines.extend(
                    said.lines()
                        .map(|line| Line::styled(format!("  {line}"), theme.severity(severity))),
                );
            }
        }
        lines
    }
}

/// A forward's own line on the strip: where to click, where it goes, and how
/// it is doing.
fn headline(forward: &Forward, selected: Option<&str>, theme: Theme) -> Line<'static> {
    let PodPort { pod, port, .. } = &forward.target;
    let mut spans = Vec::new();
    if selected != Some(forward.context.as_str()) {
        spans.push(Span::styled(format!("[{}] ", forward.cluster), theme.dim()));
    }
    let to = format!("pod {pod} port {port}");
    match &forward.state {
        State::Starting => {
            spans.push(Span::styled(to, theme.body()));
            spans.push(Span::styled("  starting…", theme.dim()));
        }
        State::Listening { url, notes } => {
            spans.push(Span::styled(url.clone(), theme.heading()));
            spans.push(Span::styled(format!(" → {to}"), theme.body()));
            if !notes.is_empty() {
                spans.push(Span::styled(
                    format!("  ({})", notes.join("; ")),
                    theme.dim(),
                ));
            }
            spans.push(Span::styled(
                format!("  {}", connections(forward.connections)),
                theme.dim(),
            ));
        }
        State::Ended { .. } => {
            spans.push(Span::styled(to, theme.body()));
            spans.push(Span::styled(
                "  stopped",
                theme.severity(Severity::Critical),
            ));
        }
    }
    Line::from(spans)
}

/// `no connections`, `1 connection`, `3 connections`.
fn connections(count: usize) -> String {
    match count {
        0 => "no connections".to_owned(),
        1 => "1 connection".to_owned(),
        n => format!("{n} connections"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const PROD: &str = "arn:aws:eks:us-east-1:1234:cluster/prod";

    fn target(pod: &str, port: u16) -> PodPort {
        PodPort {
            namespace: "default".to_owned(),
            pod: pod.to_owned(),
            port,
        }
    }

    fn started(forwards: &mut Forwards, pod: &str, port: u16) -> Id {
        match forwards.start(PROD, "prod", target(pod, port)) {
            Started::New(id) => Some(id),
            Started::Already(_) => None,
        }
        .expect("expected a new forward")
    }

    fn listening(url: &str) -> Event {
        Event::Listening {
            url: url.to_owned(),
            notes: Vec::new(),
        }
    }

    fn ended(message: &str, credentials: bool) -> Event {
        Event::Ended {
            message: message.to_owned(),
            credentials,
        }
    }

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_new_forward_is_starting_until_it_says_it_is_listening() {
        let mut forwards = Forwards::default();
        let id = started(&mut forwards, "api-1", 8080);
        assert_eq!(forwards.all()[0].state, State::Starting);

        forwards.apply(id, listening("http://127.0.0.1:8080"));
        assert_eq!(
            forwards.all()[0].state,
            State::Listening {
                url: "http://127.0.0.1:8080".to_owned(),
                notes: Vec::new()
            }
        );
    }

    #[test]
    fn the_same_port_of_the_same_pod_is_one_forward_not_two() {
        let mut forwards = Forwards::default();
        let id = started(&mut forwards, "api-1", 8080);
        forwards.apply(id, listening("http://127.0.0.1:8080"));

        assert_eq!(
            forwards.start(PROD, "prod", target("api-1", 8080)),
            Started::Already(Some("http://127.0.0.1:8080".to_owned()))
        );
        assert_eq!(forwards.all().len(), 1);
    }

    #[test]
    fn a_forward_still_starting_is_already_running_without_a_url() {
        let mut forwards = Forwards::default();
        started(&mut forwards, "api-1", 8080);
        assert_eq!(
            forwards.start(PROD, "prod", target("api-1", 8080)),
            Started::Already(None)
        );
    }

    #[test]
    fn the_same_pod_and_port_on_another_cluster_is_another_forward() {
        let mut forwards = Forwards::default();
        started(&mut forwards, "api-1", 8080);
        assert!(matches!(
            forwards.start("staging", "staging", target("api-1", 8080)),
            Started::New(_)
        ));
        assert_eq!(forwards.all().len(), 2);
    }

    #[test]
    fn starting_an_ended_forward_again_replaces_it_with_a_fresh_id() {
        let mut forwards = Forwards::default();
        let first = started(&mut forwards, "api-1", 8080);
        forwards.apply(first, ended("pod api-1 was deleted", false));

        let second = started(&mut forwards, "api-1", 8080);
        assert_ne!(first, second);
        assert_eq!(forwards.all().len(), 1);
        assert_eq!(forwards.all()[0].state, State::Starting);
    }

    #[test]
    fn an_event_from_a_replaced_forward_does_not_reach_its_successor() {
        let mut forwards = Forwards::default();
        let first = started(&mut forwards, "api-1", 8080);
        forwards.apply(first, ended("gone", false));
        started(&mut forwards, "api-1", 8080);

        forwards.apply(first, listening("http://127.0.0.1:1"));
        assert_eq!(forwards.all()[0].state, State::Starting);
    }

    #[test]
    fn stopping_removes_the_forward_and_says_whether_there_was_one() {
        let mut forwards = Forwards::default();
        let id = started(&mut forwards, "api-1", 8080);
        assert!(forwards.stop(PROD, &target("api-1", 8080)));
        assert_eq!(forwards.all(), []);
        assert!(!forwards.wants(id));
        assert!(!forwards.stop(PROD, &target("api-1", 8080)));
    }

    #[test]
    fn connections_are_counted_up_and_down_and_never_below_zero() {
        let mut forwards = Forwards::default();
        let id = started(&mut forwards, "api-1", 8080);
        forwards.apply(id, Event::Opened);
        forwards.apply(id, Event::Opened);
        forwards.apply(id, Event::Closed);
        assert_eq!(forwards.all()[0].connections, 1);
        forwards.apply(id, Event::Closed);
        forwards.apply(id, Event::Closed);
        assert_eq!(forwards.all()[0].connections, 0);
    }

    #[test]
    fn an_ended_forward_no_longer_wants_its_thread_and_holds_no_connections() {
        let mut forwards = Forwards::default();
        let id = started(&mut forwards, "api-1", 8080);
        forwards.apply(id, Event::Opened);
        assert!(forwards.wants(id));
        assert_eq!(forwards.running(), 1);

        forwards.apply(id, ended("pod api-1 was deleted", false));
        assert!(!forwards.wants(id));
        assert_eq!(forwards.running(), 0);
        assert_eq!(forwards.all()[0].connections, 0);
    }

    #[test]
    fn a_problem_is_kept_as_the_last_error_until_a_newer_one() {
        let mut forwards = Forwards::default();
        let id = started(&mut forwards, "api-1", 8080);
        forwards.apply(id, Event::Problem("first".to_owned()));
        forwards.apply(id, Event::Opened);
        forwards.apply(id, Event::Problem("second".to_owned()));
        assert_eq!(forwards.all()[0].problem.as_deref(), Some("second"));
    }

    #[test]
    fn a_login_retries_only_the_forwards_that_ended_for_want_of_credentials() {
        let mut forwards = Forwards::default();
        let refused = started(&mut forwards, "api-1", 8080);
        let deleted = started(&mut forwards, "db-0", 5432);
        let fine = started(&mut forwards, "web-1", 80);
        forwards.apply(refused, ended("your session expired", true));
        forwards.apply(deleted, ended("pod db-0 was deleted", false));
        forwards.apply(fine, listening("http://127.0.0.1:80"));

        let retried = forwards.retry_after_login(PROD);
        assert_eq!(retried.len(), 1);
        assert_ne!(retried[0].id, refused, "a retry gets a fresh id");
        assert_eq!(retried[0].context, PROD);
        assert_eq!(retried[0].target, target("api-1", 8080));
        assert_eq!(forwards.all()[0].state, State::Starting);
        assert!(matches!(forwards.all()[1].state, State::Ended { .. }));
    }

    #[test]
    fn a_login_retries_nothing_on_another_cluster() {
        let mut forwards = Forwards::default();
        let refused = started(&mut forwards, "api-1", 8080);
        forwards.apply(refused, ended("your session expired", true));
        assert_eq!(forwards.retry_after_login("staging"), Vec::new());
        assert!(!forwards.wants(refused));
    }

    #[test]
    fn clearing_takes_off_only_the_forwards_that_stopped() {
        let mut forwards = Forwards::default();
        let lost = started(&mut forwards, "api-1", 8080);
        let running = started(&mut forwards, "db-0", 5432);
        forwards.apply(lost, ended("pod api-1 was deleted", false));
        assert_eq!(forwards.stopped(), 1);

        forwards.clear_stopped();
        assert_eq!(forwards.stopped(), 0);
        assert_eq!(forwards.all().len(), 1);
        assert!(forwards.wants(running));
    }

    #[test]
    fn an_empty_strip_has_no_lines() {
        assert_eq!(
            Forwards::default().strip(Some(PROD), Theme::dark()),
            Vec::<Line<'_>>::new()
        );
    }

    #[test]
    fn the_strip_gives_each_forward_its_url_pod_port_and_connections() {
        let mut forwards = Forwards::default();
        let id = started(&mut forwards, "api-1", 8080);
        forwards.apply(id, listening("http://127.0.0.1:8080"));
        forwards.apply(id, Event::Opened);
        forwards.apply(id, Event::Opened);

        assert_eq!(
            text(&forwards.strip(Some(PROD), Theme::dark())),
            ["http://127.0.0.1:8080 → pod api-1 port 8080  2 connections"]
        );
    }

    #[test]
    fn the_strip_says_why_a_port_is_not_the_pod_s_own_number() {
        let mut forwards = Forwards::default();
        let id = started(&mut forwards, "web-1", 80);
        forwards.apply(
            id,
            Event::Listening {
                url: "http://127.0.0.1:54321".to_owned(),
                notes: vec!["local port 80 needs root".to_owned()],
            },
        );
        assert_eq!(
            text(&forwards.strip(Some(PROD), Theme::dark())),
            [
                "http://127.0.0.1:54321 → pod web-1 port 80  (local port 80 needs root)  no connections"
            ]
        );
    }

    #[test]
    fn the_strip_shows_a_starting_forward_without_a_url() {
        let mut forwards = Forwards::default();
        started(&mut forwards, "api-1", 8080);
        assert_eq!(
            text(&forwards.strip(Some(PROD), Theme::dark())),
            ["pod api-1 port 8080  starting…"]
        );
    }

    #[test]
    fn the_strip_puts_the_last_error_under_a_running_forward() {
        let mut forwards = Forwards::default();
        let id = started(&mut forwards, "api-1", 8080);
        forwards.apply(id, listening("http://127.0.0.1:8080"));
        forwards.apply(
            id,
            Event::Problem("pod api-1 refused a connection on port 8080.\nCheck it.".to_owned()),
        );
        assert_eq!(
            text(&forwards.strip(Some(PROD), Theme::dark())),
            [
                "http://127.0.0.1:8080 → pod api-1 port 8080  no connections",
                "  pod api-1 refused a connection on port 8080.",
                "  Check it.",
            ]
        );
    }

    #[test]
    fn the_strip_says_why_a_forward_stopped_in_place_of_its_last_error() {
        let mut forwards = Forwards::default();
        let id = started(&mut forwards, "api-1", 8080);
        forwards.apply(id, Event::Problem("an older complaint".to_owned()));
        forwards.apply(id, ended("pod api-1 was deleted.", false));
        assert_eq!(
            text(&forwards.strip(Some(PROD), Theme::dark())),
            ["pod api-1 port 8080  stopped", "  pod api-1 was deleted."]
        );
    }

    #[test]
    fn a_stopped_forward_s_reason_is_drawn_as_a_failure() {
        let mut forwards = Forwards::default();
        let id = started(&mut forwards, "api-1", 8080);
        forwards.apply(id, ended("gone", false));
        let theme = Theme::dark();
        let lines = forwards.strip(Some(PROD), theme);
        assert_eq!(lines[1].style, theme.severity(Severity::Critical));
    }

    #[test]
    fn a_forward_on_another_cluster_is_labelled_with_that_cluster() {
        let mut forwards = Forwards::default();
        started(&mut forwards, "api-1", 8080);
        assert_eq!(
            text(&forwards.strip(Some("staging"), Theme::dark())),
            ["[prod] pod api-1 port 8080  starting…"]
        );
        assert_eq!(
            text(&forwards.strip(None, Theme::dark())),
            ["[prod] pod api-1 port 8080  starting…"]
        );
    }

    #[test]
    fn a_port_row_marks_its_forward_by_state() {
        let mut forwards = Forwards::default();
        let id = started(&mut forwards, "api-1", 8080);
        assert_eq!(forwards.all()[0].mark().0, "starting forward…");
        forwards.apply(id, listening("http://127.0.0.1:8080"));
        assert_eq!(
            forwards.all()[0].mark(),
            ("→ http://127.0.0.1:8080".to_owned(), Severity::Ok)
        );
        forwards.apply(id, ended("gone", false));
        assert_eq!(
            forwards.all()[0].mark(),
            ("forward stopped".to_owned(), Severity::Critical)
        );
    }
}
