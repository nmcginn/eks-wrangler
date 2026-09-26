//! The whole event loop, driven end to end: scripted terminal events in,
//! frames out on a `TestBackend`, with every fetcher a stub that counts its
//! calls.
//!
//! Most of the dashboard is tested one state change at a time through
//! `App::on_key`. What that cannot show is *order* — that the background
//! query leaves only after the first frame is drawn, and that a reply's
//! characters are picked out before either `on_key` or the refresh check
//! sees them — because order lives in `event_loop` itself. Decision 111 asks
//! for "never delays first paint" to be proved by the order of events rather
//! than by a timer, and these tests are that proof.

use std::cell::{Cell as Counter, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

use super::*;

/// Something that happened, in the order it happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// A frame reached the terminal (`Backend::flush`, which `Terminal::draw`
    /// calls once per frame after writing its cells).
    Frame,
    /// The background query was sent.
    Query,
    /// The loop asked the terminal for its next event.
    Read,
}

type Log = Rc<RefCell<Vec<Step>>>;

/// `TestBackend`, logging each frame into the same [`Log`] the query and the
/// event source write to — the only way to put "drawn" and "asked" on one
/// timeline, since `event_loop` holds the terminal while it runs.
struct Recording {
    inner: TestBackend,
    log: Log,
}

impl Backend for Recording {
    type Error = <TestBackend as Backend>::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }
    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()
    }
    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }
    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        self.inner.set_cursor_position(position)
    }
    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }
    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }
    fn size(&self) -> Result<Size, Self::Error> {
        self.inner.size()
    }
    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }
    fn flush(&mut self) -> Result<(), Self::Error> {
        self.log.borrow_mut().push(Step::Frame);
        self.inner.flush()
    }
}

/// One run of `event_loop` to completion.
struct Run {
    log: Vec<Step>,
    /// The last frame drawn before the loop quit.
    screen: ratatui::buffer::Buffer,
    /// How many node fetches the loop started — `r`, and nothing else in
    /// these scripts, starts one.
    node_fetches: usize,
}

/// Drive `event_loop` over `app` with `script` as everything the terminal
/// sends, then `q` `q` to quit. `terminal_answers` is what the terminal
/// sends back the moment the query arrives, queued ahead of whatever of the
/// script is left — which is how a real terminal's reply lands: after the
/// query, and before any key pressed later.
fn run_loop(app: App, script: Vec<Event>, terminal_answers: Option<&[u8]>) -> Run {
    let log: Log = Rc::default();
    let events: Rc<RefCell<VecDeque<Event>>> = Rc::new(RefCell::new(script.into()));
    events.borrow_mut().extend([
        Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
    ]);

    let mut terminal = Terminal::new(Recording {
        inner: TestBackend::new(100, 24),
        log: Rc::clone(&log),
    })
    .unwrap();

    let node_fetches = Rc::new(Counter::new(0));
    let spawn_nodes: NodesFetcher = {
        let node_fetches = Rc::clone(&node_fetches);
        Box::new(move |_| {
            node_fetches.set(node_fetches.get() + 1);
            mpsc::channel().1
        })
    };
    let spawn_pods: PodsFetcher = Box::new(|_, _, _| mpsc::channel().1);
    let spawn_containers: ContainersFetcher = Box::new(|_, _, _| mpsc::channel().1);
    let spawn_logs: LogsFetcher =
        Box::new(|_, _, _, _, _| crate::commands::spawn_stream(|_tx, _stop| async {}));
    let drill = DrillFetchers {
        spawn_pods: &spawn_pods,
        spawn_containers: &spawn_containers,
        spawn_logs: &spawn_logs,
    };

    let mut next_event = {
        let events = Rc::clone(&events);
        let log = Rc::clone(&log);
        move |_timeout: Duration| -> std::io::Result<Option<Event>> {
            log.borrow_mut().push(Step::Read);
            // Running dry means the script never quit the loop: fail the test
            // rather than spin forever.
            events
                .borrow_mut()
                .pop_front()
                .map(Some)
                .ok_or_else(|| std::io::Error::other("script ran out before the loop quit"))
        }
    };
    let mut query = {
        let events = Rc::clone(&events);
        let log = Rc::clone(&log);
        move || -> std::io::Result<()> {
            log.borrow_mut().push(Step::Query);
            if let Some(reply) = terminal_answers {
                let mut events = events.borrow_mut();
                for key in crossterm_keys(reply).into_iter().rev() {
                    events.push_front(Event::Key(key));
                }
            }
            Ok(())
        }
    };
    let asks = app.asks_terminal_background();

    event_loop(
        &mut terminal,
        app,
        None,
        &spawn_nodes,
        &drill,
        RefreshInterval::never(),
        TerminalIo {
            suspend: &|_| Ok(()),
            next_event: &mut next_event,
            query_background: if asks { Some(&mut query) } else { None },
        },
    )
    .unwrap();

    let screen = terminal.backend().inner.buffer().clone();
    let log = log.borrow().clone();
    Run {
        log,
        screen,
        node_fetches: node_fetches.get(),
    }
}

/// The keys crossterm 0.29 parses `bytes` into, for the bytes a reply is made
/// of — `ESC x` as Alt+`x`, BEL as Ctrl+G, uppercase with SHIFT. The same
/// rule `ui::background`'s own tests write out, repeated here because that
/// helper is private to its module.
fn crossterm_keys(bytes: &[u8]) -> Vec<KeyEvent> {
    let mut out = Vec::new();
    let mut iter = bytes.iter().copied();
    while let Some(byte) = iter.next() {
        out.push(match byte {
            0x1b => {
                let next = iter.next().unwrap();
                KeyEvent::new(KeyCode::Char(char::from(next)), KeyModifiers::ALT)
            }
            0x07 => KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
            b if b.is_ascii_uppercase() => {
                KeyEvent::new(KeyCode::Char(char::from(b)), KeyModifiers::SHIFT)
            }
            b => KeyEvent::new(KeyCode::Char(char::from(b)), KeyModifiers::NONE),
        });
    }
    out
}

fn asking_app() -> App {
    let mut app = app();
    app.set_asks_terminal_background(true);
    app
}

/// Whether any cell on screen is inked in `colour` — each theme's body text
/// is a colour the other theme never uses, so this tells which theme drew
/// the frame.
fn inks(screen: &ratatui::buffer::Buffer, colour: ratatui::style::Color) -> bool {
    screen.content().iter().any(|cell| cell.fg == colour)
}

#[test]
fn the_background_query_is_sent_only_after_the_first_frame_is_drawn() {
    let run = run_loop(asking_app(), Vec::new(), None);
    assert_eq!(run.log[..3], [Step::Frame, Step::Query, Step::Read]);
}

#[test]
fn the_background_query_is_sent_once_however_many_frames_follow() {
    let run = run_loop(
        asking_app(),
        vec![Event::Key(press(KeyCode::Char('j'))); 5],
        None,
    );
    let queries = run.log.iter().filter(|&&step| step == Step::Query).count();
    let frames = run.log.iter().filter(|&&step| step == Step::Frame).count();
    assert_eq!(queries, 1);
    assert!(frames > 5, "{:?}", run.log);
}

#[test]
fn nothing_is_asked_when_the_theme_is_already_settled() {
    let run = run_loop(app(), Vec::new(), None);
    assert!(!run.log.contains(&Step::Query), "{:?}", run.log);
}

#[test]
fn a_light_answer_redraws_the_dashboard_in_the_light_theme() {
    let run = run_loop(
        asking_app(),
        Vec::new(),
        Some(b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\"),
    );
    assert!(inks(&run.screen, Theme::light().text));
    assert!(!inks(&run.screen, Theme::dark().text));
}

#[test]
fn a_dark_answer_keeps_the_dark_theme() {
    let run = run_loop(
        asking_app(),
        Vec::new(),
        Some(b"\x1b]11;rgb:1e1e/1e1e/1e1e\x07"),
    );
    assert!(inks(&run.screen, Theme::dark().text));
    assert!(!inks(&run.screen, Theme::light().text));
}

#[test]
fn no_answer_keeps_the_dark_theme() {
    let run = run_loop(asking_app(), Vec::new(), None);
    assert!(inks(&run.screen, Theme::dark().text));
}

#[test]
fn an_unreadable_answer_keeps_the_dark_theme_and_types_nothing() {
    let before = run_loop(asking_app(), Vec::new(), None);
    let after = run_loop(asking_app(), Vec::new(), Some(b"\x1b]11;nonsense\x07"));
    assert!(inks(&after.screen, Theme::dark().text));
    // Nothing the reply spelled reached a filter, a selection, or anything
    // else that would show on screen.
    assert_eq!(after.screen, before.screen);
}

#[test]
fn a_replys_characters_never_reach_the_refresh_key() {
    // `rgb` contains an `r`, which is the refresh key when it reaches the
    // loop as a keypress.
    let run = run_loop(
        asking_app(),
        Vec::new(),
        Some(b"\x1b]11;rgb:ffff/ffff/ffff\x07"),
    );
    assert_eq!(run.node_fetches, 0);
}

#[test]
fn a_real_r_after_the_reply_still_refreshes() {
    let run = run_loop(
        asking_app(),
        vec![Event::Key(press(KeyCode::Char('r')))],
        Some(b"\x1b]11;rgb:ffff/ffff/ffff\x07"),
    );
    assert_eq!(run.node_fetches, 1);
}

#[test]
fn keys_given_back_by_the_reader_are_each_handled() {
    // An Alt+`]` the user pressed, then `j`: the reader holds the first until
    // the second shows it was not a reply, then gives both back. `j` moving
    // the selection is the proof it was handled rather than dropped.
    let alt_bracket = KeyEvent::new(KeyCode::Char(']'), KeyModifiers::ALT);
    let moved = run_loop(
        asking_app(),
        vec![
            Event::Key(alt_bracket),
            Event::Key(press(KeyCode::Char('j'))),
        ],
        None,
    );
    let unmoved = run_loop(asking_app(), Vec::new(), None);
    assert_ne!(moved.screen, unmoved.screen);

    let direct = run_loop(
        asking_app(),
        vec![Event::Key(press(KeyCode::Char('j')))],
        None,
    );
    assert_eq!(moved.screen, direct.screen);
}
