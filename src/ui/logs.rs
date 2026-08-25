//! The container-logs pane: one container's log, followed live.
//!
//! Fetching streams over [`crate::commands::pods::spawn_stream_logs`]; this
//! module only reduces the [`LogEvent`]s it delivers into what to draw, the
//! same split [`super::containers`] and its siblings keep between delivery
//! and rendering. Unlike those panes, there is no listing to hold — a log
//! has no natural end, so what accumulates here is a bounded scrollback
//! buffer rather than a `Vec` of finished rows.

use std::collections::VecDeque;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::k8s::pods::LogEvent;
use crate::theme::{Severity, Theme};

/// How many lines a live log keeps in memory. Old lines drop off the front
/// once this is exceeded, the same way a terminal's own scrollback
/// eventually does, so a container tailed for a long session cannot grow the
/// pane without bound — and a sudden burst this size or larger costs one
/// `VecDeque` push per line rather than a reflow of everything already held.
const MAX_LINES: usize = 10_000;

/// How many lines one `PageUp`/`PageDown` moves, against the one line a bare
/// `j`/`k` moves.
pub(super) const PAGE: usize = 10;

/// What the container-logs pane is showing, independent of how it is drawn.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LogsState {
    /// The connection has not delivered anything yet — no line, and no word
    /// that it has already failed.
    #[default]
    Loading,
    /// At least one [`LogEvent`] has arrived, so there is a [`Log`] to show —
    /// possibly still empty, if the first thing heard from was the stream
    /// ending rather than a line.
    Streaming(Log),
    /// The stream ended, with never a line shown, before it could be told
    /// apart from one that simply has not printed anything yet. Already a
    /// full sentence, via `k8s::explain`.
    Error(String),
    /// There is nothing to connect for — `super::App::toggle_log_previous`
    /// refused to open a previous-instance log a container has never had,
    /// before starting a fetch that could only ever answer "not found."
    /// Distinct from [`Self::Error`], which is a connection that was
    /// attempted and failed: this is not a failure, so it is worded and
    /// coloured as information rather than as one.
    Unavailable(String),
}

impl LogsState {
    /// Feed one event from the stream into whatever this pane already knows.
    ///
    /// The state machine [`super::App::apply_log_event`] delegates to, kept
    /// here beside [`Log`] rather than on `App` itself for the same reason
    /// [`super::containers::ContainersState`] holds its own shape: `App`
    /// should not need to know a log has an "ended, but only after showing
    /// something" case in order to route an event to it.
    pub fn apply(&mut self, event: LogEvent) {
        match event {
            LogEvent::Line(line) => {
                if matches!(self, Self::Loading) {
                    *self = Self::Streaming(Log::default());
                }
                if let Self::Streaming(log) = self {
                    log.push(line);
                }
            }
            LogEvent::Ended(reason) => match (&mut *self, reason) {
                (Self::Streaming(log), reason) => {
                    log.status = reason.map_or(Status::Finished, Status::Failed);
                }
                (Self::Loading, Some(message)) => *self = Self::Error(message),
                (Self::Loading, None) => {
                    *self = Self::Streaming(Log {
                        status: Status::Finished,
                        ..Log::default()
                    });
                }
                // Already told apart from every other case: an `Error` never
                // had a `Log` to keep streaming into, and a second `Ended`
                // for one connection is not a shape the stream produces.
                // `Unavailable` never had one either, for a different
                // reason — no connection was ever opened for this to answer;
                // `App` always drops the fetch that would otherwise deliver
                // this event before setting that state, so it should not
                // arise in practice, and ignoring it here is the same choice
                // as `Error`'s.
                (Self::Error(_) | Self::Unavailable(_), _) => {}
            },
        }
    }
}

/// Why a stream is no longer adding lines, when it has stopped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Status {
    /// Still receiving lines, or waiting on the next one.
    #[default]
    Live,
    /// The container's log ended on its own — the ordinary shape for a
    /// completed Job, or a container about to be restarted.
    Finished,
    /// The stream broke after already showing something. The lines already
    /// on screen are kept rather than replaced by the failure, the same
    /// choice the node pane's `refresh_error` makes for a background refresh
    /// that fails after an earlier one succeeded.
    Failed(String),
}

/// One container's log as the pane is currently showing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Log {
    lines: VecDeque<String>,
    /// Whether the view is pinned to the newest line. Turned off the moment
    /// the reader scrolls toward older lines on purpose, and back on by
    /// jumping to the end or scrolling all the way back down to it.
    follow: bool,
    /// How many of the newest lines are hidden below the bottom of the
    /// view; `0` is the bottom, where `follow` keeps it. Always `0` while
    /// `follow` is `true`.
    ///
    /// [`Self::push`] keeps this a count of lines rather than an index by
    /// incrementing it on every arrival while scrolled away — which sounds
    /// backwards until the buffer is at [`MAX_LINES`] and every arrival also
    /// evicts one from the front: growth and eviction both shift where the
    /// pinned line sits in `lines`, in opposite directions, and one counter
    /// going up on both events is what cancels the two out. The visible
    /// effect is the one the field's own name promises: new lines arriving
    /// off-screen never move what a paused reader is looking at.
    hidden_below: usize,
    wrap: bool,
    status: Status,
}

impl Default for Log {
    fn default() -> Self {
        Self {
            lines: VecDeque::new(),
            follow: true,
            hidden_below: 0,
            wrap: false,
            status: Status::Live,
        }
    }
}

impl Log {
    fn push(&mut self, line: String) {
        self.lines.push_back(line);
        if self.lines.len() > MAX_LINES {
            self.lines.pop_front();
        }
        // See `hidden_below`'s own doc comment: this is what keeps a paused
        // view pointed at the same lines while the buffer keeps moving
        // underneath it.
        if !self.follow {
            self.hidden_below = self.hidden_below.saturating_add(1);
        }
    }

    /// Scroll toward older lines, ending `follow` — the reader has just said
    /// they want to look at something other than the newest line. Not
    /// clamped here: [`Self::visible`] is where "there is nothing older to
    /// reveal" is actually decided, because that answer depends on how many
    /// rows the pane has to show them in, which only it is asked for.
    pub fn scroll_up(&mut self, amount: usize) {
        self.follow = false;
        self.hidden_below = self.hidden_below.saturating_add(amount);
    }

    /// Scroll toward newer lines. Reaching the bottom resumes `follow`,
    /// matching a pager: scrolling down far enough lands back where new
    /// lines keep arriving rather than leaving the reader one line short of
    /// it.
    pub fn scroll_down(&mut self, amount: usize) {
        self.hidden_below = self.hidden_below.saturating_sub(amount);
        if self.hidden_below == 0 {
            self.follow = true;
        }
    }

    /// Jump to the newest line and resume following.
    pub fn jump_to_end(&mut self) {
        self.hidden_below = 0;
        self.follow = true;
    }

    /// Jump to the oldest line still in the buffer, and stop following.
    ///
    /// Hiding every line in the buffer is not a bug here the way it would be
    /// for [`Self::scroll_up`]'s amount-at-a-time version: [`Self::visible`]
    /// treats "more hidden than there are lines" as "show me everything
    /// there is," which is exactly what jumping to the start asks for.
    pub fn jump_to_start(&mut self) {
        self.follow = false;
        self.hidden_below = self.lines.len();
    }

    /// `f`: hop straight to the bottom and resume following, or stop if it
    /// already was — a bare on/off would leave a reader who is already at
    /// the bottom with no way to ask for the same thing `End` does.
    pub fn toggle_follow(&mut self) {
        if self.follow {
            self.follow = false;
        } else {
            self.jump_to_end();
        }
    }

    pub fn toggle_wrap(&mut self) {
        self.wrap = !self.wrap;
    }

    #[must_use]
    pub fn follow(&self) -> bool {
        self.follow
    }

    #[must_use]
    pub fn wrap(&self) -> bool {
        self.wrap
    }

    #[must_use]
    pub fn status(&self) -> &Status {
        &self.status
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The lines the pane should currently draw, oldest first and windowed
    /// by `hidden_below`. `rows` is how many the pane has room for.
    ///
    /// `end` is clamped to show at least `rows` lines (or every line there
    /// is, if fewer) even when `hidden_below` alone would hide more than
    /// that: once the whole log already fits in the pane, or scrolling has
    /// gone back past the oldest line, there is nothing further "up" to
    /// reveal, so the window stops retreating and simply shows what is
    /// there — the reading [`Self::jump_to_start`] relies on, and the one
    /// that keeps an over-eager `PageUp` from blanking a short log.
    fn visible(&self, rows: usize) -> impl Iterator<Item = &str> {
        let len = self.lines.len();
        let end = len.saturating_sub(self.hidden_below).max(len.min(rows));
        let start = end.saturating_sub(rows);
        self.lines.range(start..end).map(String::as_str)
    }
}

/// Draw whatever the container-logs pane currently knows. `previous` is
/// [`super::View::ContainerLogs`]'s own flag, threaded down here rather than
/// read off `state` — a loading connection and an unavailable one both need
/// to say which log they are (or were) asking for, and neither carries a
/// [`Log`] of its own to hold it.
pub(super) fn draw(frame: &mut Frame, area: Rect, state: &LogsState, previous: bool, theme: Theme) {
    let lines = match state {
        LogsState::Loading if previous => {
            vec![Line::styled("Loading previous logs…", theme.dim())]
        }
        LogsState::Loading => vec![Line::styled("Loading logs…", theme.dim())],
        LogsState::Unavailable(message) => vec![Line::styled(message.clone(), theme.dim())],
        LogsState::Error(message) => vec![Line::styled(
            message.clone(),
            theme.severity(Severity::Critical),
        )],
        LogsState::Streaming(log) => log_lines(log, previous, area, theme),
    };

    let mut paragraph = Paragraph::new(lines);
    if matches!(state, LogsState::Streaming(log) if log.wrap()) {
        paragraph = paragraph.wrap(Wrap { trim: false });
    }
    frame.render_widget(paragraph, area);
}

fn log_lines(log: &Log, previous: bool, area: Rect, theme: Theme) -> Vec<Line<'_>> {
    let mut lines = vec![heading(log, previous, theme)];
    if let Status::Failed(message) = log.status() {
        lines.push(Line::styled(
            format!("Stream ended: {message}"),
            theme.severity(Severity::Warn),
        ));
    }

    if log.is_empty() {
        let text = match log.status() {
            Status::Live => "No log output yet.",
            Status::Finished | Status::Failed(_) => "This container has no log output.",
        };
        lines.push(Line::styled(text, theme.dim()));
        return lines;
    }

    let rows = usize::from(area.height).saturating_sub(lines.len());
    lines.extend(
        log.visible(rows)
            .map(|line| Line::styled(line, theme.body())),
    );
    lines
}

fn heading(log: &Log, previous: bool, theme: Theme) -> Line<'static> {
    let follow = if log.follow() {
        "following"
    } else {
        "scrolled — f to resume"
    };
    let wrap = if log.wrap() { "wrap on" } else { "wrap off" };
    let ended = match log.status() {
        Status::Live => "",
        Status::Finished => "  · stream ended",
        Status::Failed(_) => "  · stream failed",
    };
    // Silent for the common case — the current log — the same rule
    // `k8s::order::note` follows for the default ordering: only the
    // unusual reading says anything about itself.
    let title = if previous { "LOGS · previous" } else { "LOGS" };

    Line::from(vec![
        Span::styled(title, theme.heading()),
        Span::styled(format!("  {follow} · {wrap}{ended}"), theme.dim()),
    ])
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn lines(log: &mut LogsState, texts: &[&str]) {
        for text in texts {
            log.apply(LogEvent::Line((*text).to_owned()));
        }
    }

    fn render(state: &LogsState) -> String {
        render_with(state, false)
    }

    fn render_with(state: &LogsState, previous: bool) -> String {
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        terminal
            .draw(|frame| draw(frame, frame.area(), state, previous, Theme::dark()))
            .unwrap();
        terminal.backend().to_string()
    }

    /// Unwrap the `Streaming` a test expects, via `expect` rather than a
    /// bare `panic!` — `clippy::panic` is denied crate-wide and does not
    /// carve out tests the way `unwrap_used`/`expect_used` do above.
    fn streaming(state: &LogsState) -> &Log {
        match state {
            LogsState::Streaming(log) => Some(log),
            LogsState::Loading | LogsState::Error(_) | LogsState::Unavailable(_) => None,
        }
        .expect("expected Streaming")
    }

    // --- LogsState::apply ---------------------------------------------------

    #[test]
    fn the_first_line_moves_loading_into_streaming() {
        let mut state = LogsState::default();
        state.apply(LogEvent::Line("hello".to_owned()));

        assert!(matches!(state, LogsState::Streaming(_)));
    }

    #[test]
    fn a_clean_end_before_any_line_is_an_empty_finished_stream_not_an_error() {
        // A container that exited before printing anything is a real answer,
        // not a failure — `Error` is reserved for a connection that never
        // worked.
        let mut state = LogsState::default();
        state.apply(LogEvent::Ended(None));

        let log = streaming(&state);
        assert!(log.is_empty());
        assert_eq!(log.status(), &Status::Finished);
    }

    #[test]
    fn a_failure_before_any_line_is_an_error_state() {
        let mut state = LogsState::default();
        state.apply(LogEvent::Ended(Some("could not connect".to_owned())));

        assert_eq!(state, LogsState::Error("could not connect".to_owned()));
    }

    #[test]
    fn a_failure_after_lines_keeps_them_rather_than_replacing_them_with_an_error() {
        let mut state = LogsState::default();
        lines(&mut state, &["one", "two"]);
        state.apply(LogEvent::Ended(Some("connection reset".to_owned())));

        let log = streaming(&state);
        assert!(!log.is_empty());
        assert_eq!(log.status(), &Status::Failed("connection reset".to_owned()));
    }

    #[test]
    fn a_clean_end_after_lines_marks_the_stream_finished_without_losing_them() {
        let mut state = LogsState::default();
        lines(&mut state, &["one"]);
        state.apply(LogEvent::Ended(None));

        let log = streaming(&state);
        assert!(!log.is_empty());
        assert_eq!(log.status(), &Status::Finished);
    }

    // --- Log: the bounded buffer ---------------------------------------------

    #[test]
    fn old_lines_drop_once_the_buffer_is_full() {
        let mut log = Log::default();
        for line in 0..MAX_LINES + 500 {
            log.push(line.to_string());
        }

        let newest = (MAX_LINES + 499).to_string();
        assert_eq!(log.visible(1).next(), Some(newest.as_str()));
        // The oldest surviving line is the 500th ever pushed (indices 0..499
        // were evicted), not the first one this log ever saw.
        assert_eq!(log.visible(usize::MAX).next(), Some("500"));
    }

    #[test]
    fn a_paused_view_stays_on_the_same_lines_even_while_the_buffer_is_evicting() {
        // The case `hidden_below`'s doc comment describes: once the buffer
        // is at capacity, every arrival also evicts one from the front, so
        // staying pointed at the same lines needs the opposite adjustment
        // from the plain-growth case in `new_lines_do_not_move_the_view_while_scrolled_up`.
        let mut log = Log::default();
        for line in 0..MAX_LINES {
            log.push(line.to_string());
        }
        log.scroll_up(5);
        let before: Vec<String> = log.visible(3).map(str::to_owned).collect();

        for line in MAX_LINES..MAX_LINES + 50 {
            log.push(line.to_string());
        }

        assert_eq!(
            log.visible(3).collect::<Vec<_>>(),
            before.iter().map(String::as_str).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_ten_thousand_line_burst_lands_in_one_push_per_line() {
        // The acceptance criterion, as a shape rather than a timing: the
        // buffer holds at most `MAX_LINES` regardless of how many arrived,
        // so a burst this size costs the same per-line work as a trickle
        // rather than a growing reflow.
        let mut log = Log::default();
        for line in 0..10_000 {
            log.push(line.to_string());
        }

        assert_eq!(log.visible(usize::MAX).count(), 10_000);
    }

    // --- Log: scrolling and follow -------------------------------------------

    #[test]
    fn following_shows_the_newest_lines() {
        let mut log = Log::default();
        for line in 1..=5 {
            log.push(line.to_string());
        }

        let shown: Vec<&str> = log.visible(3).collect();
        assert_eq!(shown, vec!["3", "4", "5"]);
    }

    #[test]
    fn scrolling_up_stops_following_and_shows_older_lines() {
        let mut log = Log::default();
        for line in 1..=5 {
            log.push(line.to_string());
        }

        log.scroll_up(2);

        assert!(!log.follow());
        let shown: Vec<&str> = log.visible(3).collect();
        assert_eq!(shown, vec!["1", "2", "3"]);
    }

    #[test]
    fn scrolling_up_stops_at_the_oldest_line_rather_than_wrapping() {
        let mut log = Log::default();
        for line in 1..=3 {
            log.push(line.to_string());
        }

        log.scroll_up(100);

        let shown: Vec<&str> = log.visible(10).collect();
        assert_eq!(shown, vec!["1", "2", "3"]);
    }

    #[test]
    fn scrolling_all_the_way_back_down_resumes_following() {
        let mut log = Log::default();
        for line in 1..=5 {
            log.push(line.to_string());
        }
        log.scroll_up(2);

        log.scroll_down(2);

        assert!(log.follow());
        let shown: Vec<&str> = log.visible(3).collect();
        assert_eq!(shown, vec!["3", "4", "5"]);
    }

    #[test]
    fn new_lines_do_not_move_the_view_while_scrolled_up() {
        let mut log = Log::default();
        for line in 1..=5 {
            log.push(line.to_string());
        }
        log.scroll_up(2);
        let before: Vec<String> = log.visible(3).map(str::to_owned).collect();

        log.push("6".to_owned());

        assert_eq!(
            log.visible(3).collect::<Vec<_>>(),
            before.iter().map(String::as_str).collect::<Vec<_>>()
        );
    }

    #[test]
    fn jump_to_start_and_end_move_to_the_edges() {
        let mut log = Log::default();
        for line in 1..=5 {
            log.push(line.to_string());
        }

        log.jump_to_start();
        assert!(!log.follow());
        assert_eq!(log.visible(1).collect::<Vec<_>>(), vec!["1"]);

        log.jump_to_end();
        assert!(log.follow());
        assert_eq!(log.visible(1).collect::<Vec<_>>(), vec!["5"]);
    }

    #[test]
    fn toggle_follow_turns_it_off_and_back_on_at_the_bottom() {
        let mut log = Log::default();
        log.push("only".to_owned());
        assert!(log.follow());

        log.toggle_follow();
        assert!(!log.follow());

        log.toggle_follow();
        assert!(log.follow());
        assert_eq!(log.hidden_below, 0);
    }

    #[test]
    fn toggle_wrap_flips_it_each_press() {
        let mut log = Log::default();
        assert!(!log.wrap());
        log.toggle_wrap();
        assert!(log.wrap());
        log.toggle_wrap();
        assert!(!log.wrap());
    }

    // --- Rendering -------------------------------------------------------------

    #[test]
    fn loading_state_renders_before_any_line_arrives() {
        let rendered = render(&LogsState::Loading);
        assert!(rendered.contains("Loading logs"), "{rendered}");
    }

    #[test]
    fn loading_a_previous_log_says_so_rather_than_reading_like_the_current_one() {
        let rendered = render_with(&LogsState::Loading, true);
        assert!(rendered.contains("Loading previous logs"), "{rendered}");
    }

    #[test]
    fn a_streaming_previous_log_names_itself_in_the_heading() {
        let mut state = LogsState::default();
        lines(&mut state, &["one"]);

        let rendered = render_with(&state, true);
        assert!(rendered.contains("LOGS · previous"), "{rendered}");
    }

    #[test]
    fn the_current_log_heading_says_nothing_extra() {
        let mut state = LogsState::default();
        lines(&mut state, &["one"]);

        let rendered = render(&state);
        assert!(!rendered.contains("previous"), "{rendered}");
    }

    #[test]
    fn an_unavailable_previous_log_says_so_rather_than_reading_like_a_failure() {
        let rendered = render(&LogsState::Unavailable(
            "This container has never restarted, so it has no previous log.".to_owned(),
        ));
        assert!(rendered.contains("has never restarted"), "{rendered}");
    }

    #[test]
    fn an_empty_stream_says_so_rather_than_rendering_nothing() {
        let mut state = LogsState::default();
        state.apply(LogEvent::Ended(None));

        let rendered = render(&state);
        assert!(rendered.contains("no log output"), "{rendered}");
    }

    #[test]
    fn error_state_renders_the_message_instead_of_a_log() {
        let rendered = render(&LogsState::Error("could not connect: nope".to_owned()));
        assert!(rendered.contains("could not connect"), "{rendered}");
    }

    #[test]
    fn loaded_lines_are_shown_under_the_heading() {
        let mut state = LogsState::default();
        lines(&mut state, &["starting up", "listening on :8080"]);

        let rendered = render(&state);
        assert!(rendered.contains("LOGS"), "{rendered}");
        assert!(rendered.contains("starting up"), "{rendered}");
        assert!(rendered.contains("listening on :8080"), "{rendered}");
        assert!(rendered.contains("following"), "{rendered}");
    }

    #[test]
    fn a_stream_that_failed_after_showing_lines_names_the_failure_and_keeps_them() {
        let mut state = LogsState::default();
        lines(&mut state, &["one line before it broke"]);
        state.apply(LogEvent::Ended(Some("connection reset".to_owned())));

        let rendered = render(&state);
        assert!(rendered.contains("one line before it broke"), "{rendered}");
        assert!(rendered.contains("connection reset"), "{rendered}");
    }

    #[test]
    fn rendering_the_logs_pane_survives_a_tiny_terminal() {
        let mut state = LogsState::default();
        lines(&mut state, &["a line long enough to need wrapping maybe"]);

        for (width, height) in [(1, 1), (8, 3), (20, 2), (200, 60)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| draw(frame, frame.area(), &state, false, Theme::dark()))
                .unwrap();
        }
    }

    #[test]
    fn a_ten_thousand_line_burst_still_renders_one_frame() {
        // The UI-facing half of the acceptance criterion: feeding a burst
        // through the same `apply` the event loop uses, then drawing it,
        // must not hang or panic.
        let mut state = LogsState::default();
        for line in 0..10_000 {
            state.apply(LogEvent::Line(format!("line {line}")));
        }

        let rendered = render(&state);
        assert!(rendered.contains("line 9999"), "{rendered}");
    }
}
