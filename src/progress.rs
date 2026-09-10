//! Saying that a command is still working, while it still is.
//!
//! Paging turned a listing into several requests, and `--timeout` put the
//! credential helper on the same clock as one — so `eks nodes` against a large
//! cluster, or against a laptop whose SSO endpoint has gone, now spends real
//! time doing something it never says a word about. This module is the word: a
//! single line on **stderr** naming what is outstanding, rewritten in place as
//! it changes, and erased before anything else is printed.
//!
//! Three rules shape everything here.
//!
//! **It never touches stdout.** The table is what a pipe carries away, and a
//! progress line is not part of the listing. `eks nodes | grep NotReady` has to
//! be unchanged to the byte, which is also why nothing is drawn at all unless
//! *stdout itself* is a terminal — see [`wanted`].
//!
//! **It never fails a command.** Every write here is discarded if it fails. A
//! command that could not draw its progress line still has a table to print,
//! and turning a closed stderr into an error would be inventing a failure out
//! of decoration.
//!
//! **The wording is a pure function.** [`line()`] takes the outstanding steps and
//! how long they have been outstanding and returns the text; [`redraw`] turns
//! text into the bytes that put it on screen. Everything impure — a lock, a
//! clock, a file descriptor — is in [`Progress`] and [`Task`], and a test drives
//! those through an ordinary `Vec<u8>`.

use std::ffi::OsStr;
use std::fmt;
use std::future::Future;
use std::io::Write;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::format;
use crate::theme::{ColourChoice, Palette};

/// How often a step redraws while nothing about it changes.
///
/// Only the elapsed count can change in that time, and it changes once a
/// second, so this is about how stale that figure is allowed to look rather
/// than about how fast anything moves. A redraw whose text is identical to
/// what is already on screen writes nothing (see `Meter::draw`), so the cost
/// of the tick between two seconds is one string comparison.
const TICK: Duration = Duration::from_millis(250);

/// Returns to the start of the line and clears it.
///
/// `\r` rather than a cursor-up sequence, and `\x1b[K` rather than a row of
/// spaces: between them they touch exactly the one row this module ever wrote
/// on, which is what lets the line disappear without disturbing whatever the
/// user's shell had above it.
const ERASE: &str = "\r\x1b[K";

/// One thing a command is still waiting for.
///
/// Two shapes rather than one because they answer the reader's question
/// differently. A listing can say how far it has got and the number moving is
/// the reassurance; the credential helper is a single subprocess that either
/// answers or does not, and the only honest thing to show is its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Something with no progress to report, described as a verb phrase:
    /// `running aws eks get-token`.
    Waiting(String),

    /// A listing arriving in pages. `what` is the plural noun it is a listing
    /// of — `nodes`, `pods`, `node metrics` — and `read` is how many objects
    /// have landed, `None` until the first page arrives.
    Reading { what: String, read: Option<usize> },
}

/// The whole progress line for a set of outstanding steps.
///
/// Empty when nothing is outstanding, which is what the meter behind
/// [`Progress`] reads as "take the line off the screen".
///
/// The readings are gathered into one clause rather than written out
/// separately, because `eks nodes` has three listings in flight at once and
/// `read 1,500 nodes, read 12,000 pods, read 1,500 node metrics` says the verb
/// three times for one answer. In practice the two groups never appear
/// together — the helper has finished before the first listing starts — but the
/// wording does not depend on that being true.
///
/// The elapsed count is dropped below a second. It exists to show that
/// something is still happening on a command that has gone quiet, and a
/// listing that finishes in 200 ms should not flash a `0s` on its way past.
#[must_use]
pub fn line(steps: &[Step], elapsed: Duration) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut readings: Vec<String> = Vec::new();

    for step in steps {
        match step {
            Step::Waiting(phrase) => parts.push(phrase.clone()),
            Step::Reading { what, read: None } => readings.push(what.clone()),
            Step::Reading {
                what,
                read: Some(read),
            } => readings.push(format!("{} {what}", format::count(*read))),
        }
    }

    if !readings.is_empty() {
        parts.push(format!("reading {}", readings.join(", ")));
    }

    if parts.is_empty() {
        return String::new();
    }

    let mut line = parts.join(", ");
    line.push('…');

    let seconds = elapsed.as_secs();
    if seconds >= 1 {
        line.push(' ');
        line.push_str(&format::exact_duration(Duration::from_secs(seconds)));
    }
    line
}

/// `line`, shortened to what fits on one row of a terminal `cols` wide.
///
/// Not cosmetic. A line that wraps onto a second row leaves the first row
/// somewhere `\r` can no longer reach, so the erase would clear the tail and
/// leave the head of the old line on screen permanently — under the table,
/// under the next prompt, under everything. Truncating is what keeps the line
/// erasable.
///
/// One column is left spare, because a line filling the last column leaves the
/// cursor in a state terminals disagree about: some wrap immediately, some
/// wait for the next character.
#[must_use]
pub fn fit(line: &str, cols: usize) -> String {
    let room = cols.saturating_sub(1);
    if room == 0 {
        return String::new();
    }
    if line.chars().count() <= room {
        return line.to_owned();
    }
    // The ellipsis costs a column of its own, so a line cut short still ends
    // with a character saying it was cut short rather than mid-word.
    let kept: String = line.chars().take(room - 1).collect();
    format!("{kept}…")
}

/// The bytes that put `line` on screen, over whatever is there already.
///
/// `\r` first so the text starts at the left margin, and `\x1b[K` last so a
/// line shorter than the one it replaces does not leave the old line's tail
/// beside it.
#[must_use]
pub fn redraw(line: &str, cols: usize) -> String {
    format!("\r{}\x1b[K", fit(line, cols))
}

/// The width to draw at, from what the terminal said about itself.
///
/// A terminal reporting **zero** columns has not answered the question — no
/// terminal is nought columns wide. A pty opened without a window size does
/// this, which is what `script`, a handful of CI runners, and some editors'
/// embedded terminals hand a program. Believing the zero draws an empty line
/// four times a second and makes the tool look broken on exactly the setups
/// that are hardest to look at; falling back to the conventional eighty draws
/// a line that is right nearly every time and, when it is not, is a line
/// wrapped once rather than nothing at all.
///
/// `None` is the same answer for the same reason: it is the ioctl having
/// failed, since a caller who knows stdout is not a terminal has already
/// stopped at [`wanted`].
#[must_use]
pub fn width(terminal_cols: Option<u16>) -> usize {
    match terminal_cols {
        Some(cols) if cols > 0 => usize::from(cols),
        _ => DEFAULT_COLS,
    }
}

/// The width to draw at when the terminal will not say how wide it is.
///
/// Eighty, for the reason everything else that has to guess picks eighty.
const DEFAULT_COLS: usize = 80;

/// Whether this run may draw a progress line at all.
///
/// Both ends have to be terminals, and for different reasons. **Stdout**,
/// because a listing being piped or redirected is a listing nobody is watching
/// arrive — and because a spinner on stderr beside `eks nodes | grep foo`
/// writing its own output to the same screen is two programs drawing on one
/// row. **Stderr**, because this line is only ever legible if it can be
/// rewritten in place; written into a file it would be a column of half-erased
/// duplicates.
///
/// `logging` is the third: whether this run is also writing log lines to
/// stderr, which `-v` and `RUST_LOG` turn on. Those cannot share a row with a
/// line that rewrites itself — a log line lands wherever the cursor was left
/// and the next redraw writes back over it, so both come out shredded — and
/// between the two, the lines the user explicitly asked for win. A tool being
/// debugged should print what it is being debugged for.
///
/// Beyond that the question is the one [`Palette`] already answers, asked
/// again: movement is ink. `--color never` and `NO_COLOR` are the switches a
/// user reaches for to say "write plainly", and a line that redraws itself is
/// not plain; `TERM=dumb` is a terminal promising it does not understand the
/// escape sequences [`redraw`] is made of. Reusing the rule rather than
/// inventing a `--progress` beside it means there is one answer to "how do I
/// turn this off" — see decision 92.
///
/// `stdout_is_terminal` is passed to [`Palette::choose`] as `true` rather than
/// as itself: by this point both ends are known to be terminals, and what is
/// being asked of the palette is only what the *flags and environment* say.
#[must_use]
pub fn wanted(
    choice: ColourChoice,
    stdout_is_terminal: bool,
    stderr_is_terminal: bool,
    logging: bool,
    no_color: Option<&OsStr>,
    term: Option<&OsStr>,
) -> bool {
    stdout_is_terminal
        && stderr_is_terminal
        && !logging
        && Palette::choose(choice, true, no_color, term).is_colour()
}

/// Whether this run writes log lines to stderr, from the two things that turn
/// them on.
///
/// `-v` in any quantity, or a `RUST_LOG` set to anything at all — which is
/// exactly the pair `main`'s own tracing setup consults, `RUST_LOG` first.
/// Anything set is enough: a filter this function cannot parse is still a user
/// who has asked to watch stderr, and guessing at whether it will actually
/// emit anything is a worse answer than believing them.
///
/// The quiet default is not "no logging" — a `warn!` can still fire, and one
/// of them lands in [`crate::k8s::page::collect`] when a server repeats its
/// page marker. That is a once-in-a-listing event that leaves one shredded row
/// behind an erase, against a `-v` that leaves every row shredded, so it is
/// not worth turning the line off for.
#[must_use]
pub fn logging_to_stderr(verbose: u8, rust_log: Option<&OsStr>) -> bool {
    verbose > 0 || rust_log.is_some_and(|filter| !filter.is_empty())
}

/// The outstanding steps, where they are drawn, and what is on screen now.
struct Meter {
    out: Box<dyn Write + Send>,
    cols: usize,
    /// Outstanding steps in the order they started, each with the identifier
    /// its [`Task`] will finish it by. A `Vec` rather than a map because the
    /// order is the rendering order and there are never more than a handful.
    steps: Vec<(u64, Step)>,
    next: u64,
    /// When the current run of activity began — reset each time the last step
    /// finishes, so the count beside `reading 1,500 nodes…` is that listing's
    /// own age and not the whole command's.
    since: Instant,
    /// What is on screen, so an unchanged line is not rewritten and an empty
    /// one is only erased once.
    shown: String,
}

impl fmt::Debug for Meter {
    /// Hand-written because a `Box<dyn Write>` has no `Debug`, and the writer
    /// is not what anybody debugging this wants to see anyway.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Meter")
            .field("cols", &self.cols)
            .field("steps", &self.steps)
            .field("shown", &self.shown)
            .finish_non_exhaustive()
    }
}

impl Meter {
    fn start(&mut self, step: Step) -> u64 {
        if self.steps.is_empty() {
            self.since = Instant::now();
        }
        let id = self.next;
        self.next += 1;
        self.steps.push((id, step));
        self.draw();
        id
    }

    fn finish(&mut self, id: u64) {
        self.steps.retain(|(each, _)| *each != id);
        self.draw();
    }

    fn advance(&mut self, id: u64, by: usize) {
        if let Some((_, Step::Reading { read, .. })) =
            self.steps.iter_mut().find(|(each, _)| *each == id)
        {
            *read = Some(read.unwrap_or(0) + by);
        }
        self.draw();
    }

    /// Put the current line on screen, if it is not there already.
    ///
    /// Every write is discarded on failure. Stderr closing under a running
    /// command is not something the command should die of, and there is
    /// nowhere left to report it to in any case.
    fn draw(&mut self) {
        let line = line(
            &self
                .steps
                .iter()
                .map(|(_, step)| step.clone())
                .collect::<Vec<_>>(),
            self.since.elapsed(),
        );

        if line == self.shown {
            return;
        }

        let bytes = if line.is_empty() {
            ERASE.to_owned()
        } else {
            redraw(&line, self.cols)
        };
        let _ = self.out.write_all(bytes.as_bytes());
        let _ = self.out.flush();
        self.shown = line;
    }
}

/// Somewhere to report progress to — or nowhere at all.
///
/// Cheap to clone and safe to hand across threads, so the same handle reaches
/// the credential helper and each of the three listings a node table waits on.
/// [`Progress::none`] is the default and every method on it is a no-op that
/// writes nothing and allocates nothing: that is what the dashboard's
/// background fetches get, since a thread that does not own the screen has no
/// business drawing on it, and it is what a piped command gets.
#[derive(Debug, Clone, Default)]
pub struct Progress(Option<Arc<Mutex<Meter>>>);

impl Progress {
    /// Nowhere. Every step started against this is invisible.
    #[must_use]
    pub fn none() -> Self {
        Self(None)
    }

    /// Draw into `out`, on a terminal `cols` columns wide.
    ///
    /// The writer is owned rather than borrowed because a [`Task`] outlives
    /// the call that made it and may be dropped on the way out of an error
    /// path; `cols` is fixed for the life of the command, since a one-shot
    /// listing that is resized mid-flight is not worth a `SIGWINCH` handler.
    #[must_use]
    pub fn to(out: Box<dyn Write + Send>, cols: usize) -> Self {
        Self(Some(Arc::new(Mutex::new(Meter {
            out,
            cols,
            steps: Vec::new(),
            next: 0,
            since: Instant::now(),
            shown: String::new(),
        }))))
    }

    /// Start a listing of `what`, counting objects as they arrive.
    ///
    /// `#[must_use]` because the returned [`Task`] *is* the step: dropping it
    /// on the spot would start the step and end it in the same breath.
    #[must_use]
    pub fn reading(&self, what: &str) -> Task {
        self.start(Step::Reading {
            what: what.to_owned(),
            read: None,
        })
    }

    /// Start something with nothing to count, described as a verb phrase:
    /// `running aws eks get-token`.
    ///
    /// `#[must_use]` for the reason [`Progress::reading`] is.
    #[must_use]
    pub fn waiting(&self, phrase: &str) -> Task {
        self.start(Step::Waiting(phrase.to_owned()))
    }

    fn start(&self, step: Step) -> Task {
        let Some(meter) = &self.0 else {
            return Task::default();
        };
        let id = with(meter, |meter| meter.start(step));
        Task(Some((Arc::clone(meter), id)))
    }
}

/// One outstanding step, which ends when this is dropped.
///
/// Dropping is the whole contract: a listing that fails partway through has to
/// take its line off the screen just as surely as one that finishes, and a
/// `?` on the error path is not somewhere anybody remembers to write an
/// erase. [`Task::default`] is a detached handle — what every method on a
/// [`Progress::none`] hands back — and does nothing at all.
#[derive(Debug, Default)]
pub struct Task(Option<(Arc<Mutex<Meter>>, u64)>);

impl Task {
    /// Report that `by` more objects have arrived.
    pub fn advance(&self, by: usize) {
        if let Some((meter, id)) = &self.0 {
            with(meter, |meter| meter.advance(*id, by));
        }
    }

    /// Await `future`, redrawing the line a few times a second until it resolves.
    ///
    /// What makes a step with nothing to count look alive: the credential
    /// helper produces no events of its own between "started" and "answered",
    /// so without this the line naming it would sit motionless for the whole
    /// thirty seconds it is allowed. It is used around each page of a listing
    /// too, since one slow page is the same silence in miniature.
    ///
    /// A detached task awaits the future and nothing else — no timer, no lock,
    /// nothing on the dashboard's threads that was not there before.
    pub async fn tick<T>(&self, future: impl Future<Output = T>) -> T {
        // Boxed before the branch, so this function's own future is one
        // pointer wide whichever way it goes. A `kube` request future is
        // measured in kilobytes, and holding one inline here would add a
        // second copy of it to every `page::collect` frame — enough, across
        // the three listings `eks nodes` joins, to push the whole command past
        // what `clippy::large_futures` allows. The allocation is one per page,
        // against a page that is an HTTP round trip.
        let mut future = Box::pin(future);

        let Some((meter, _)) = &self.0 else {
            return future.await;
        };

        loop {
            tokio::select! {
                done = &mut future => return done,
                () = tokio::time::sleep(TICK) => with(meter, Meter::draw),
            }
        }
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        if let Some((meter, id)) = &self.0 {
            with(meter, |meter| meter.finish(*id));
        }
    }
}

/// Run `f` against the meter, taking a poisoned lock's contents rather than
/// giving up on them.
///
/// A panic elsewhere while this lock was held would poison it, and the state
/// behind it is a line of text on a terminal — there is nothing here that
/// could be left inconsistent enough to be worth refusing to draw over, and
/// `unwrap` is denied in this crate for good reason.
fn with<T>(meter: &Mutex<Meter>, f: impl FnOnce(&mut Meter) -> T) -> T {
    let mut guard = meter.lock().unwrap_or_else(PoisonError::into_inner);
    f(&mut guard)
}

/// A writer that keeps what was written to it, for tests anywhere in the
/// crate.
///
/// The progress line is only ever real on a terminal, so the only way to
/// assert on it is to point it somewhere a test can read back. Cloneable, and
/// every clone shares one buffer: one copy goes into the [`Progress`] and the
/// other stays with the test.
///
/// Here rather than in this module's own `tests` because the guarantees worth
/// asserting are not all local — that the credential helper is named while it
/// runs, and that the line is gone before the table is printed, are claims
/// about `k8s::client` and `k8s::page`.
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct Recorder(Arc<Mutex<Vec<u8>>>);

#[cfg(test)]
impl Recorder {
    /// Everything written so far.
    pub(crate) fn written(&self) -> String {
        let bytes = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Whether the line has been taken off the screen — the last thing
    /// written was an erase, so a table printed after this starts on a clean
    /// row.
    pub(crate) fn is_erased(&self) -> bool {
        let written = self.written();
        written.is_empty() || written.ends_with(ERASE)
    }
}

#[cfg(test)]
impl fmt::Debug for Recorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Recorder").field(&self.written()).finish()
    }
}

#[cfg(test)]
impl Write for Recorder {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn reading(what: &str, read: Option<usize>) -> Step {
        Step::Reading {
            what: what.to_owned(),
            read,
        }
    }

    #[test]
    fn nothing_outstanding_is_an_empty_line() {
        // Which is what tells the meter to take the line off the screen, so
        // this is the erase rule as much as it is the wording.
        assert_eq!(line(&[], Duration::from_secs(4)), "");
    }

    #[test]
    fn a_listing_says_what_it_is_reading_before_the_first_page_lands() {
        assert_eq!(
            line(&[reading("nodes", None)], Duration::from_secs(2)),
            "reading nodes… 2s"
        );
    }

    #[test]
    fn a_listing_counts_the_objects_that_have_arrived() {
        assert_eq!(
            line(&[reading("nodes", Some(1500))], Duration::from_secs(4)),
            "reading 1,500 nodes… 4s"
        );
    }

    #[test]
    fn concurrent_listings_share_one_clause_in_the_order_they_started() {
        // `eks nodes` waits on three at once, and the verb belongs to all of
        // them rather than to each.
        let steps = [
            reading("nodes", Some(1500)),
            reading("pods", Some(12000)),
            reading("node metrics", None),
        ];
        assert_eq!(
            line(&steps, Duration::from_secs(4)),
            "reading 1,500 nodes, 12,000 pods, node metrics… 4s"
        );
    }

    #[test]
    fn a_step_with_nothing_to_count_is_its_own_phrase() {
        assert_eq!(
            line(
                &[Step::Waiting("running aws eks get-token".to_owned())],
                Duration::from_secs(12)
            ),
            "running aws eks get-token… 12s"
        );
    }

    #[test]
    fn the_elapsed_count_is_left_off_below_a_second() {
        // A listing that finishes in 200 ms should not flash `0s` on its way
        // past.
        assert_eq!(
            line(&[reading("nodes", Some(3))], Duration::from_millis(200)),
            "reading 3 nodes…"
        );
        assert_eq!(
            line(&[reading("nodes", Some(3))], Duration::from_millis(999)),
            "reading 3 nodes…"
        );
    }

    #[test]
    fn a_long_wait_is_worded_the_way_a_timeout_is() {
        // `format::exact_duration`, the same spelling `--timeout` echoes back,
        // so a minute is not `60s` here and `1m` in the error underneath it.
        assert_eq!(
            line(&[reading("pods", Some(1))], Duration::from_secs(60)),
            "reading 1 pods… 1m"
        );
        assert_eq!(
            line(&[reading("pods", Some(1))], Duration::from_secs(90)),
            "reading 1 pods… 90s"
        );
    }

    #[test]
    fn a_zero_count_is_a_real_answer_rather_than_a_missing_one() {
        // A page that arrived empty is not the same as no page yet, and an
        // empty cluster should say so instead of looking stuck.
        assert_eq!(
            line(&[reading("nodes", Some(0))], Duration::from_secs(1)),
            "reading 0 nodes… 1s"
        );
    }

    #[test]
    fn a_line_that_fits_is_left_exactly_as_it_is() {
        assert_eq!(fit("reading 3 nodes…", 80), "reading 3 nodes…");
    }

    #[test]
    fn a_line_too_long_for_the_terminal_is_cut_short_with_an_ellipsis() {
        // Wrapping is what would make the line unerasable, so this is a
        // correctness rule wearing a cosmetic one's clothes.
        assert_eq!(fit("abcdefghij", 6), "abcd…");
        assert_eq!(fit("abcdefghij", 11), "abcdefghij");
        assert_eq!(fit("abcdefghij", 10), "abcdefgh…");
    }

    #[test]
    fn a_terminal_with_no_room_gets_no_line() {
        for cols in [0, 1] {
            assert_eq!(fit("anything", cols), "", "{cols} columns");
        }
        assert_eq!(fit("anything", 2), "…");
    }

    #[test]
    fn a_line_is_cut_on_a_character_rather_than_a_byte() {
        // A cluster label can carry anything a kubeconfig does; slicing a
        // multi-byte character in half would panic in a tool that forbids
        // panics.
        assert_eq!(fit("ααααα", 4), "αα…");
    }

    #[test]
    fn drawing_returns_to_the_margin_and_clears_what_it_does_not_cover() {
        assert_eq!(redraw("hello", 80), "\rhello\x1b[K");
    }

    #[test]
    fn logs_on_stderr_take_the_row_the_progress_line_would_have_used() {
        // They cannot share one: a log line lands wherever the cursor was left
        // and the next redraw writes back over it, so both come out shredded.
        // A user running `-vv` asked for the logs.
        assert!(!wanted(ColourChoice::Auto, true, true, true, None, None));
        // And `--color always` does not buy the row back.
        assert!(!wanted(ColourChoice::Always, true, true, true, None, None));
    }

    #[test]
    fn logging_is_on_when_either_switch_for_it_is() {
        assert!(!logging_to_stderr(0, None));
        assert!(logging_to_stderr(1, None));
        assert!(logging_to_stderr(3, None));
        assert!(logging_to_stderr(0, Some(OsStr::new("debug"))));
        // Unparseable is still a user asking to watch stderr; `main`'s own
        // setup falls back to the `-v` filter rather than ignoring it, and
        // either way something is going to be written there.
        assert!(logging_to_stderr(0, Some(OsStr::new("not,a=filter"))));
        // Set-but-empty is `RUST_LOG=` in a shell profile, which turns
        // nothing on.
        assert!(!logging_to_stderr(0, Some(OsStr::new(""))));
    }

    #[test]
    fn a_terminal_that_will_not_say_how_wide_it_is_gets_the_conventional_eighty() {
        // A pty opened without a window size reports zero columns. Believing
        // it would draw an empty line four times a second, which is how this
        // was found: under `script`, the line was five erases and no words.
        assert_eq!(width(Some(0)), 80);
        assert_eq!(width(None), 80);
        assert_eq!(width(Some(120)), 120);
        assert_eq!(width(Some(1)), 1);
    }

    #[test]
    fn a_progress_line_is_drawn_only_when_both_ends_are_terminals() {
        let choice = ColourChoice::Auto;
        assert!(wanted(choice, true, true, false, None, None));
        // Piped or redirected stdout: the listing is unchanged to the byte,
        // and nothing is written anywhere.
        assert!(!wanted(choice, false, true, false, None, None));
        // Redirected stderr: a line that cannot be rewritten in place would
        // be a column of half-erased duplicates in a file.
        assert!(!wanted(choice, true, false, false, None, None));
    }

    #[test]
    fn turning_colour_off_turns_the_progress_line_off_with_it() {
        // Movement is ink: the switches for "write plainly" are the same
        // ones, rather than a second flag nobody would find.
        assert!(!wanted(ColourChoice::Never, true, true, false, None, None));
        assert!(!wanted(
            ColourChoice::Auto,
            true,
            true,
            false,
            Some(OsStr::new("1")),
            None
        ));
        assert!(!wanted(
            ColourChoice::Auto,
            true,
            true,
            false,
            None,
            Some(OsStr::new("dumb"))
        ));
        // `NO_COLOR=` set-but-empty is the spec's way of saying "not set".
        assert!(wanted(
            ColourChoice::Auto,
            true,
            true,
            false,
            Some(OsStr::new("")),
            None
        ));
    }

    #[test]
    fn colour_forced_on_still_does_not_draw_over_a_pipe() {
        // `--color always` is about how bytes are written, not about who is
        // reading them; the piped listing stays untouched either way.
        assert!(!wanted(
            ColourChoice::Always,
            false,
            true,
            false,
            None,
            None
        ));
        assert!(!wanted(
            ColourChoice::Always,
            true,
            false,
            false,
            None,
            None
        ));
        assert!(wanted(ColourChoice::Always, true, true, false, None, None));
    }

    #[test]
    fn a_step_is_drawn_when_it_starts_and_erased_when_it_ends() {
        let recorder = Recorder::default();
        let progress = Progress::to(Box::new(recorder.clone()), 80);

        let task = progress.reading("nodes");
        assert_eq!(recorder.written(), "\rreading nodes…\x1b[K");

        drop(task);
        assert_eq!(
            recorder.written(),
            "\rreading nodes…\x1b[K\r\x1b[K",
            "the line was not taken off the screen"
        );
    }

    #[test]
    fn a_listing_that_fails_partway_through_still_erases_its_line() {
        // The reason the handle erases on drop rather than on a call: the `?`
        // on an error path is nowhere anybody remembers to write one.
        let recorder = Recorder::default();
        let progress = Progress::to(Box::new(recorder.clone()), 80);

        // A scope rather than a call, but the same thing a `?` does: the
        // handle goes out of scope on the way out, and the line goes with it.
        let failed: Result<(), &str> = {
            let task = progress.reading("nodes");
            task.advance(500);
            Err("the cluster stopped answering")
        };

        assert!(failed.is_err());
        assert!(
            recorder.written().ends_with(ERASE),
            "left on screen: {:?}",
            recorder.written()
        );
    }

    #[test]
    fn each_page_rewrites_the_count_in_place() {
        let recorder = Recorder::default();
        let progress = Progress::to(Box::new(recorder.clone()), 80);

        let task = progress.reading("pods");
        task.advance(500);
        task.advance(500);
        drop(task);

        assert_eq!(
            recorder.written(),
            "\rreading pods…\x1b[K\
             \rreading 500 pods…\x1b[K\
             \rreading 1,000 pods…\x1b[K\
             \r\x1b[K"
        );
    }

    #[test]
    fn a_redraw_that_would_change_nothing_writes_nothing() {
        // The tick between two seconds costs a string comparison rather than
        // a write; without this a quiet command would flicker four times a
        // second for no reason.
        let recorder = Recorder::default();
        let progress = Progress::to(Box::new(recorder.clone()), 80);

        let task = progress.reading("nodes");
        task.advance(500);
        let after_first_page = recorder.written();

        // An empty page, twice: real — the API server is entitled to hand
        // back a page of nothing and a token for the next one.
        task.advance(0);
        task.advance(0);

        assert_eq!(recorder.written(), after_first_page);
        drop(task);
    }

    #[test]
    fn concurrent_steps_leave_the_line_up_until_the_last_one_finishes() {
        let recorder = Recorder::default();
        let progress = Progress::to(Box::new(recorder.clone()), 80);

        let nodes = progress.reading("nodes");
        let pods = progress.reading("pods");
        drop(nodes);

        assert!(
            recorder.written().ends_with("\rreading pods…\x1b[K"),
            "written: {:?}",
            recorder.written()
        );

        drop(pods);
        assert!(recorder.written().ends_with(ERASE));
    }

    #[test]
    fn a_progress_that_goes_nowhere_writes_nothing_and_still_works() {
        // The dashboard's fetches and every piped command take this path, so
        // it has to be a complete no-op rather than a writer pointed at
        // nothing.
        let progress = Progress::none();
        let task = progress.reading("nodes");
        task.advance(10);
        drop(task);

        let task = progress.waiting("running aws eks get-token");
        drop(task);
    }

    #[test]
    fn a_detached_task_awaits_its_future_and_nothing_else() {
        let outcome = crate::commands::block_on(async {
            let task = Task::default();
            Ok(task.tick(async { 42 }).await)
        })
        .unwrap();

        assert_eq!(outcome, 42);
    }

    #[test]
    fn a_ticking_task_redraws_while_its_future_is_still_running() {
        // The credential-helper case: nothing about the step changes, so the
        // only thing that can prove it is alive is the elapsed count moving.
        let recorder = Recorder::default();

        let written = crate::commands::block_on(async {
            let progress = Progress::to(Box::new(recorder.clone()), 80);
            let task = progress.waiting("running aws eks get-token");
            task.tick(tokio::time::sleep(Duration::from_millis(1100)))
                .await;
            drop(task);
            Ok(recorder.written())
        })
        .unwrap();

        assert!(
            written.contains("running aws eks get-token… 1s"),
            "the line never showed a second passing: {written:?}"
        );
        assert!(written.ends_with(ERASE));
    }
}
