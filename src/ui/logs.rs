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
use ratatui::crossterm::event::{KeyCode, KeyEvent};
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

    /// Whether this pane's own `/` search is currently capturing keystrokes
    /// — [`super::App::is_filtering`]'s counterpart for this pane's
    /// different `/` (see [`LogSearch`]'s module docs), read by
    /// [`super::App::on_key`] to route every following key as query text
    /// rather than its usual meaning, and by the footer for its hints.
    #[must_use]
    pub(super) fn is_search_editing(&self) -> bool {
        matches!(self, Self::Streaming(log) if log.is_search_editing())
    }

    /// Whether a search has been committed with `Enter` — `n`/`N` have
    /// something to step through, and the footer should offer them only
    /// then, the same "only offer a key where it does something" rule
    /// `R`'s hint already follows.
    #[must_use]
    pub(super) fn is_search_applied(&self) -> bool {
        matches!(self, Self::Streaming(log) if log.is_search_applied())
    }
}

/// The container-logs pane's own `/` search.
///
/// [`super::Filter`]'s counterpart for text rather than rows, and
/// deliberately not a second caller of [`crate::fuzzy::rank`]: a log's order
/// is the one thing about it nobody wants re-ranked or thinned, so `/` here
/// answers "jump to the next matching line" rather than "show only the
/// matching ones." Matching is plain, case-insensitive substring search
/// rather than a scored subsequence — the more familiar reading for "find
/// this text" than fuzzy ranking is, and the way a pager's own `/` behaves.
/// `Editing` captures every keystroke as query text, the same life cycle
/// `Filter` follows; `Applied` is what committing it with `Enter` leaves
/// behind, and it is what gives `n`/`N` something to step through.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum LogSearch {
    #[default]
    Inactive,
    Editing(String),
    Applied(String),
}

impl LogSearch {
    /// The text a line is currently being matched against — empty when no
    /// search is active, whether or not one is being typed. Also the seed a
    /// fresh `/` press starts editing from, so a second press refines a
    /// committed query rather than starting over.
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

/// Which way `n`/`N` step through a search's matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SearchDirection {
    Forward,
    Backward,
}

/// Case-insensitive substring test — the one rule a line either matches a
/// query by or does not, pulled out so [`Log::matches`] and the draw path
/// that highlights against the same query share one answer.
fn contains_ci(line: &str, query: &str) -> bool {
    line.to_lowercase().contains(&query.to_lowercase())
}

/// Find the nearest of `matches` to `from` in `direction`, wrapping past
/// either end of the buffer — the "no further matches, start over" rule a
/// pager's own `n`/`N` follows. `None` only when `matches` is empty.
///
/// `inclusive` is `true` only for the jump a fresh commit makes, which
/// should treat the line already in view as a hit if it is one; `n`/`N`
/// always pass `false`; so pressing `n` right after committing a search
/// whose nearest hit is the line already on screen moves on to the next one
/// instead of appearing to do nothing.
fn step(
    matches: &[usize],
    from: usize,
    direction: SearchDirection,
    inclusive: bool,
) -> Option<usize> {
    match direction {
        SearchDirection::Forward => matches
            .iter()
            .copied()
            .find(|&m| m > from || (inclusive && m == from))
            .or_else(|| matches.first().copied()),
        SearchDirection::Backward => matches
            .iter()
            .rev()
            .copied()
            .find(|&m| m < from || (inclusive && m == from))
            .or_else(|| matches.last().copied()),
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
    search: LogSearch,
}

impl Default for Log {
    fn default() -> Self {
        Self {
            lines: VecDeque::new(),
            follow: true,
            hidden_below: 0,
            wrap: false,
            status: Status::Live,
            search: LogSearch::Inactive,
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

    /// `/`: begin typing a search query, seeded with whatever was last
    /// applied so a second press refines it — [`super::App::start_filter`]'s
    /// own seeding for the row-list filter.
    pub(super) fn start_search(&mut self) {
        self.search = LogSearch::Editing(self.search.query().to_owned());
    }

    /// `Esc`/`Left` while a search is applied and not being edited: clear
    /// it, rather than backing out a drill-down level — [`super::App::clear_filter`]'s
    /// own "unwind the newest thing first" rule, extended to this pane's
    /// own `/`.
    pub(super) fn clear_search(&mut self) {
        self.search = LogSearch::Inactive;
    }

    /// Handle one key while [`LogSearch::Editing`] is capturing text —
    /// [`super::App::edit_filter`]'s life cycle, kept here beside `Log`'s
    /// other key handling rather than a second copy of it on `App`. `Enter`
    /// commits the query (collapsing an empty one back to
    /// [`LogSearch::Inactive`] rather than leaving an `Applied("")` with
    /// nothing to jump to) and jumps to the nearest match; `Esc` cancels
    /// outright; `Backspace` and any other character edit the text.
    pub(super) fn handle_search_key(&mut self, key: KeyEvent) {
        let LogSearch::Editing(query) = &self.search else {
            return;
        };
        let mut query = query.clone();
        match key.code {
            KeyCode::Enter => {
                self.search = if query.is_empty() {
                    LogSearch::Inactive
                } else {
                    LogSearch::Applied(query)
                };
                self.jump_to_match(SearchDirection::Forward, true);
            }
            KeyCode::Esc => self.search = LogSearch::Inactive,
            KeyCode::Backspace => {
                query.pop();
                self.search = LogSearch::Editing(query);
            }
            KeyCode::Char(c) => {
                query.push(c);
                self.search = LogSearch::Editing(query);
            }
            _ => {}
        }
    }

    /// `n`/`N`: move to the committed search's next or previous match
    /// relative to whatever line is currently at the bottom of the view,
    /// wrapping past either end of the buffer. A no-op before `Enter` has
    /// committed a query, or once one has and finds nothing anywhere in the
    /// buffer to jump to — [`log_lines`] is what tells the reader that, since
    /// nothing here needs to remember the answer past this one call.
    pub(super) fn jump_to_match(&mut self, direction: SearchDirection, inclusive: bool) {
        let LogSearch::Applied(query) = &self.search else {
            return;
        };
        let matches = self.matches(query);
        let Some(target) = step(&matches, self.current_bottom(), direction, inclusive) else {
            return;
        };
        self.follow = false;
        self.hidden_below = self.lines.len().saturating_sub(1).saturating_sub(target);
    }

    /// The absolute, oldest-first index of the newest line currently in
    /// view — [`Self::jump_to_match`]'s anchor. The same arithmetic
    /// [`Self::visible`]'s `end` uses, without needing to know `rows`: a
    /// search step happens on a key press, off the render path, so there is
    /// no pane height to hand it.
    fn current_bottom(&self) -> usize {
        self.lines
            .len()
            .saturating_sub(self.hidden_below)
            .saturating_sub(1)
    }

    /// Absolute, oldest-first indices of every line containing `query`,
    /// case-insensitively. Recomputed fresh on every call rather than kept
    /// on `Self::search`: the buffer evicts from the front as new lines
    /// arrive, which would renumber a cached list on every push, and a full
    /// scan of at most [`MAX_LINES`] short strings costs nothing next to
    /// redrawing the frame that already needs one — the same trade-off
    /// `crate::fuzzy::rank` makes for a row list's `/` filter.
    fn matches(&self, query: &str) -> Vec<usize> {
        self.lines
            .iter()
            .enumerate()
            .filter(|(_, line)| contains_ci(line, query))
            .map(|(index, _)| index)
            .collect()
    }

    /// Whether `query` matches anywhere in the buffer — [`Self::matches`]'s
    /// own short-circuiting cousin for the draw path, which only ever asks
    /// "is there at least one," not "which ones."
    fn has_match(&self, query: &str) -> bool {
        self.lines.iter().any(|line| contains_ci(line, query))
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

    #[must_use]
    pub(super) fn is_search_editing(&self) -> bool {
        self.search.is_editing()
    }

    #[must_use]
    pub(super) fn is_search_applied(&self) -> bool {
        self.search.is_applied()
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
    pub(super) fn visible(&self, rows: usize) -> impl Iterator<Item = &str> {
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

    // Shown under the heading rather than folded into it, the same
    // placement `nodes::draw`'s own `Filter: "…"` line takes: unlike that
    // one, a search here never removes a line, so — distinct from an empty
    // row listing — the query and the log both stay on screen even when
    // nothing currently matches, and this is the one place that says so.
    let query = log.search.query();
    if !query.is_empty() {
        lines.push(Line::styled(format!("Search: \"{query}\""), theme.dim()));
        if !log.has_match(query) {
            lines.push(Line::styled(
                format!("No matches for \"{query}\"."),
                theme.severity(Severity::Warn),
            ));
        }
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
    lines.extend(log.visible(rows).map(|line| {
        let style = if !query.is_empty() && contains_ci(line, query) {
            theme.match_highlight()
        } else {
            theme.body()
        };
        Line::styled(line, style)
    }));
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

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, ratatui::crossterm::event::KeyModifiers::NONE)
    }

    /// Type `query` character by character into a fresh search prompt —
    /// [`Log::handle_search_key`]'s own path, exercised the way a keystroke
    /// actually arrives rather than by setting `search` directly.
    fn type_query(log: &mut Log, query: &str) {
        log.start_search();
        for c in query.chars() {
            log.handle_search_key(key(KeyCode::Char(c)));
        }
    }

    /// A log with a query already committed against it: five lines, two of
    /// which contain "error", searched and landed on the first.
    fn search_committed_log() -> Log {
        let mut log = Log::default();
        for line in ["alpha", "beta error", "gamma", "delta error", "epsilon"] {
            log.push(line.to_owned());
        }
        type_query(&mut log, "error");
        log.handle_search_key(key(KeyCode::Enter));
        log
    }

    fn render_log(log: &Log, previous: bool) -> String {
        render_with(&LogsState::Streaming(log.clone()), previous)
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

    // --- step: the `/`-search's pure stepping rule ---------------------------

    #[test]
    fn step_forward_finds_the_next_match_after_the_anchor() {
        assert_eq!(
            step(&[1, 3, 5], 1, SearchDirection::Forward, false),
            Some(3)
        );
    }

    #[test]
    fn step_forward_wraps_to_the_first_match_when_none_remain() {
        assert_eq!(
            step(&[1, 3, 5], 5, SearchDirection::Forward, false),
            Some(1)
        );
    }

    #[test]
    fn step_forward_inclusive_counts_the_anchor_itself_as_a_hit() {
        assert_eq!(step(&[1, 3, 5], 3, SearchDirection::Forward, true), Some(3));
    }

    #[test]
    fn step_forward_exclusive_skips_past_the_anchor_even_when_it_matches() {
        assert_eq!(
            step(&[1, 3, 5], 3, SearchDirection::Forward, false),
            Some(5)
        );
    }

    #[test]
    fn step_backward_finds_the_previous_match_before_the_anchor() {
        assert_eq!(
            step(&[1, 3, 5], 5, SearchDirection::Backward, false),
            Some(3)
        );
    }

    #[test]
    fn step_backward_wraps_to_the_last_match_when_none_remain() {
        assert_eq!(
            step(&[1, 3, 5], 1, SearchDirection::Backward, false),
            Some(5)
        );
    }

    #[test]
    fn step_with_no_matches_at_all_returns_none() {
        assert_eq!(step(&[], 0, SearchDirection::Forward, true), None);
    }

    // --- contains_ci / Log::matches / Log::has_match --------------------------

    #[test]
    fn contains_ci_ignores_case() {
        assert!(contains_ci("Connection ERROR", "error"));
        assert!(!contains_ci("all good", "error"));
    }

    #[test]
    fn matches_returns_every_matching_lines_absolute_index() {
        let mut log = Log::default();
        for line in ["alpha", "beta error", "gamma", "delta ERROR"] {
            log.push(line.to_owned());
        }

        assert_eq!(log.matches("error"), vec![1, 3]);
    }

    #[test]
    fn has_match_is_false_over_an_empty_buffer_or_a_query_with_no_hits() {
        assert!(!Log::default().has_match("anything"));

        let mut log = Log::default();
        log.push("nothing to see here".to_owned());
        assert!(!log.has_match("error"));
    }

    // --- Log: the `/` search itself -------------------------------------------

    #[test]
    fn starting_a_search_seeds_it_from_the_last_applied_query() {
        let mut log = Log {
            search: LogSearch::Applied("alpha".to_owned()),
            ..Log::default()
        };

        log.start_search();

        assert_eq!(log.search, LogSearch::Editing("alpha".to_owned()));
    }

    #[test]
    fn typing_and_committing_a_query_applies_it() {
        let mut log = Log::default();
        log.push("beta error".to_owned());

        type_query(&mut log, "error");
        assert!(log.is_search_editing());

        log.handle_search_key(key(KeyCode::Enter));
        assert!(!log.is_search_editing());
        assert!(log.is_search_applied());
        assert_eq!(log.search, LogSearch::Applied("error".to_owned()));
    }

    #[test]
    fn backspace_removes_the_last_typed_character() {
        let mut log = Log::default();
        log.start_search();
        log.handle_search_key(key(KeyCode::Char('a')));
        log.handle_search_key(key(KeyCode::Char('b')));

        log.handle_search_key(key(KeyCode::Backspace));

        assert_eq!(log.search, LogSearch::Editing("a".to_owned()));
    }

    #[test]
    fn escape_while_editing_cancels_outright_even_over_a_previously_applied_query() {
        let mut log = Log {
            search: LogSearch::Applied("old".to_owned()),
            ..Log::default()
        };
        type_query(&mut log, "x");

        log.handle_search_key(key(KeyCode::Esc));

        assert_eq!(log.search, LogSearch::Inactive);
    }

    #[test]
    fn committing_an_empty_query_leaves_the_search_inactive() {
        let mut log = Log::default();
        log.start_search();

        log.handle_search_key(key(KeyCode::Enter));

        assert_eq!(log.search, LogSearch::Inactive);
    }

    #[test]
    fn committing_a_query_jumps_to_the_nearest_match_and_stops_following() {
        let log = search_committed_log();

        assert!(!log.follow());
        assert_eq!(log.visible(1).collect::<Vec<_>>(), vec!["beta error"]);
    }

    #[test]
    fn committing_a_search_whose_only_match_is_already_in_view_still_stops_following() {
        let mut log = Log::default();
        log.push("alpha".to_owned());
        log.push("beta error".to_owned());

        type_query(&mut log, "error");
        log.handle_search_key(key(KeyCode::Enter));

        assert!(!log.follow());
        assert_eq!(log.hidden_below, 0, "the match was already at the bottom");
    }

    #[test]
    fn n_steps_to_the_next_match_and_wraps_past_the_newest_line() {
        let mut log = search_committed_log();

        log.jump_to_match(SearchDirection::Forward, false);
        assert_eq!(log.visible(1).collect::<Vec<_>>(), vec!["delta error"]);

        log.jump_to_match(SearchDirection::Forward, false);
        assert_eq!(
            log.visible(1).collect::<Vec<_>>(),
            vec!["beta error"],
            "wraps back to the oldest match"
        );
    }

    #[test]
    fn shift_n_steps_to_the_previous_match_and_wraps_past_the_oldest_line() {
        let mut log = search_committed_log();

        log.jump_to_match(SearchDirection::Backward, false);

        assert_eq!(
            log.visible(1).collect::<Vec<_>>(),
            vec!["delta error"],
            "wraps to the newest match"
        );
    }

    #[test]
    fn a_query_with_no_match_anywhere_leaves_the_view_untouched() {
        let mut log = Log::default();
        log.push("alpha".to_owned());
        log.push("beta".to_owned());

        type_query(&mut log, "zzz");
        log.handle_search_key(key(KeyCode::Enter));

        assert!(
            log.follow(),
            "nothing to jump to, so nothing to stop following for"
        );
        assert_eq!(log.visible(1).collect::<Vec<_>>(), vec!["beta"]);
    }

    #[test]
    fn n_does_nothing_before_a_search_has_been_committed() {
        let mut log = Log::default();
        log.push("alpha".to_owned());
        log.push("beta".to_owned());

        log.jump_to_match(SearchDirection::Forward, false);

        assert!(log.follow());
    }

    #[test]
    fn clearing_a_search_turns_it_off() {
        let mut log = search_committed_log();

        log.clear_search();

        assert_eq!(log.search, LogSearch::Inactive);
        assert!(!log.is_search_applied());
    }

    #[test]
    fn logs_state_reports_search_editing_only_while_streaming_and_editing() {
        let mut state = LogsState::default();
        assert!(!state.is_search_editing());

        state.apply(LogEvent::Line("hello".to_owned()));
        assert!(!state.is_search_editing());

        if let LogsState::Streaming(log) = &mut state {
            log.start_search();
        }
        assert!(state.is_search_editing());
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

    // --- Rendering: the `/` search ---------------------------------------------

    #[test]
    fn a_committed_search_names_itself_above_the_lines() {
        let log = search_committed_log();

        let rendered = render_log(&log, false);
        assert!(rendered.contains("Search: \"error\""), "{rendered}");
        assert!(!rendered.contains("No matches"), "{rendered}");
    }

    #[test]
    fn a_search_with_no_match_says_so_without_losing_the_log() {
        let mut log = Log::default();
        log.push("alpha".to_owned());
        log.push("beta".to_owned());
        log.search = LogSearch::Applied("zzz".to_owned());

        let rendered = render_log(&log, false);
        assert!(rendered.contains("No matches for \"zzz\""), "{rendered}");
        assert!(rendered.contains("alpha"), "{rendered}");
        assert!(rendered.contains("beta"), "{rendered}");
    }

    #[test]
    fn a_matching_line_is_highlighted_and_a_non_matching_line_is_not() {
        // Both render the same plain text either way, so the distinction that
        // matters is the style carried on the line's span, not anything a
        // rendered string comparison could see — `containers.rs`'s own
        // `a_warning_events_reason_is_coloured_and_a_normal_ones_is_not`
        // follows the same shape for the same reason.
        let theme = Theme::dark();
        let mut log = Log::default();
        log.push("starting up".to_owned());
        log.push("connection error: refused".to_owned());
        log.search = LogSearch::Applied("error".to_owned());

        let area = Rect::new(0, 0, 90, 20);
        let lines = log_lines(&log, false, area, theme);

        let matching = lines
            .iter()
            .find(|line| line.spans[0].content.contains("connection error"))
            .expect("the matching line is present");
        let non_matching = lines
            .iter()
            .find(|line| line.spans[0].content.contains("starting up"))
            .expect("the non-matching line is present");

        // `Line::styled` — what `log_lines` builds these two lines with —
        // carries the style on the `Line` itself rather than on its one
        // `Span`, unlike `event_lines`' own `Span::styled` in `containers.rs`.
        assert_eq!(matching.style, theme.match_highlight());
        assert_eq!(non_matching.style, theme.body());
        assert_ne!(matching.style, non_matching.style);
    }

    #[test]
    fn clearing_a_search_removes_the_highlight_and_the_search_line() {
        let mut log = search_committed_log();

        log.clear_search();

        let rendered = render_log(&log, false);
        assert!(!rendered.contains("Search:"), "{rendered}");
    }

    #[test]
    fn rendering_an_active_search_survives_a_tiny_terminal() {
        let log = search_committed_log();

        for (width, height) in [(1, 1), (8, 3), (20, 2), (200, 60)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    draw(
                        frame,
                        frame.area(),
                        &LogsState::Streaming(log.clone()),
                        false,
                        Theme::dark(),
                    );
                })
                .unwrap();
        }
    }
}
