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
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

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
    /// A shell session ran, with the terminal handed to it.
    Session,
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
    /// How many container fetches and log streams the loop started.
    container_fetches: usize,
    log_streams: usize,
    /// The containers the checks behind `x` were started for.
    exec_checks: Vec<ExecTarget>,
    /// The ports `f` started forwards for, in order.
    forwards_started: Vec<PodPort>,
    /// How many of those forwards' handles the loop had dropped — each one
    /// a port closed — by the time it quit, or shortly after.
    forwards_stopped: Arc<AtomicUsize>,
    /// The control-plane reads the loop started, and how many it stopped.
    control_plane: ControlPlaneStub,
}

/// How the stubs behind `x` and `f` answer.
#[derive(Clone)]
struct Stubs {
    /// What the checks answer with.
    checked: Result<Plan, FetchError>,
    /// Hold the answer until the script sends `Event::FocusGained`, rather
    /// than having it ready at once — a check still running when the next
    /// key arrives.
    held: bool,
    /// How the session ends.
    session: Result<(), FetchError>,
    /// What every forward started with `f` reports, at once, before it
    /// waits to be stopped.
    forward_events: Vec<ForwardEvent>,
    /// What every control-plane read started with `C` or `t` reports, at
    /// once, before it waits to be stopped.
    control_plane: Vec<ControlPlaneUpdate>,
}

impl Default for Stubs {
    fn default() -> Self {
        Self {
            checked: Ok(plan()),
            held: false,
            session: Ok(()),
            forward_events: Vec::new(),
            control_plane: Vec::new(),
        }
    }
}

fn plan() -> Plan {
    Plan {
        namespace: "default".to_owned(),
        pod: "api-1".to_owned(),
        container: "app".to_owned(),
        shell: vec!["/bin/sh".to_owned()],
        os: crate::k8s::remote::Os::Linux,
    }
}

/// The terminal's events, read off `events` one per call. Reading
/// `Event::FocusGained` also releases a check's answer held back in `held`.
fn scripted(
    events: &Rc<RefCell<VecDeque<Event>>>,
    log: &Log,
    held: &Rc<RefCell<Held>>,
) -> impl FnMut(Duration) -> std::io::Result<Option<Event>> {
    let events = Rc::clone(events);
    let log = Rc::clone(log);
    let held = Rc::clone(held);
    move |_timeout: Duration| {
        log.borrow_mut().push(Step::Read);
        // Running dry means the script never quit the loop: fail the test
        // rather than spin forever.
        let event = events
            .borrow_mut()
            .pop_front()
            .ok_or_else(|| std::io::Error::other("script ran out before the loop quit"))?;
        if event == Event::FocusGained
            && let Some((tx, checked)) = held.borrow_mut().take()
        {
            // Nobody may be listening any more; that is the point.
            let _ = tx.send(checked);
        }
        Ok(Some(event))
    }
}

/// A check's answer the stub is holding back, and where to send it.
type Held = Option<(
    mpsc::Sender<Result<Plan, FetchError>>,
    Result<Plan, FetchError>,
)>;

/// The stub behind `x`'s checks: records each target in `checks`, and
/// answers at once or into `held`, as `exec` says.
fn exec_preparer(
    exec: &Stubs,
    checks: &Rc<RefCell<Vec<ExecTarget>>>,
    held: &Rc<RefCell<Held>>,
) -> ExecPreparer {
    let checks = Rc::clone(checks);
    let held = Rc::clone(held);
    let exec = exec.clone();
    Box::new(move |_, target| {
        checks.borrow_mut().push(target.clone());
        let (tx, rx) = mpsc::channel();
        if exec.held {
            *held.borrow_mut() = Some((tx, exec.checked.clone()));
        } else {
            tx.send(exec.checked.clone()).unwrap();
        }
        rx
    })
}

/// The stub behind `f`: records each port in `started`, hands back
/// `stubs.forward_events` already queued, and counts in `stopped` each
/// forward whose handle the loop drops.
fn forward_starter(
    stubs: &Stubs,
    started: &Rc<RefCell<Vec<PodPort>>>,
    stopped: &Arc<AtomicUsize>,
) -> ForwardStarter {
    let started = Rc::clone(started);
    let stopped = Arc::clone(stopped);
    let events = stubs.forward_events.clone();
    Box::new(move |_, target| {
        started.borrow_mut().push(target.clone());
        // Queued before the receiver is handed back, so they are there for
        // the loop's very next pass rather than whenever a thread gets round
        // to them.
        let (tx, rx) = mpsc::channel();
        for event in events.clone() {
            tx.send(event).unwrap();
        }
        let stopped = Arc::clone(&stopped);
        let (_, handle) =
            crate::commands::spawn_stream(move |_: mpsc::Sender<()>, stop| async move {
                // The sender lives as long as the forward, as a real one's
                // does: a channel that closes early is a thread that died,
                // and the loop says so.
                let _events = tx;
                if stop.await.is_ok() {
                    stopped.fetch_add(1, Ordering::SeqCst);
                }
            });
        (rx, handle)
    })
}

/// The stub behind `C`, and what it saw.
struct ControlPlaneStub {
    fetcher: ControlPlaneFetcher,
    /// The reads the loop started, by type, in order.
    reads: Rc<RefCell<Vec<LogType>>>,
    /// How many of those reads' handles the loop had dropped, by the time it
    /// quit or shortly after.
    stopped: Arc<AtomicUsize>,
}

impl ControlPlaneStub {
    fn reads(&self) -> Vec<LogType> {
        self.reads.borrow().clone()
    }
}

/// The stub behind `C`: records each type it is asked for, hands back
/// `stubs.control_plane` already queued, and counts each read whose handle
/// the loop drops.
fn control_plane_reader(stubs: &Stubs) -> ControlPlaneStub {
    let reads: Rc<RefCell<Vec<LogType>>> = Rc::default();
    let stopped = Arc::new(AtomicUsize::new(0));
    let (started, dropped) = (Rc::clone(&reads), Arc::clone(&stopped));
    let updates = stubs.control_plane.clone();
    let fetcher: ControlPlaneFetcher = Box::new(move |_, kind| {
        started.borrow_mut().push(kind);
        let (tx, rx) = mpsc::channel();
        for update in updates.clone() {
            tx.send(update).unwrap();
        }
        let stopped = Arc::clone(&dropped);
        let (_, handle) =
            crate::commands::spawn_stream(move |_: mpsc::Sender<()>, stop| async move {
                let _updates = tx;
                if stop.await.is_ok() {
                    stopped.fetch_add(1, Ordering::SeqCst);
                }
            });
        (rx, handle)
    });
    ControlPlaneStub {
        fetcher,
        reads,
        stopped,
    }
}

fn ctrl_c() -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
}

/// Drive `event_loop` over `app` with `script` as everything the terminal
/// sends, then `q` `q` to quit. `terminal_answers` is what the terminal
/// sends back the moment the query arrives, queued ahead of whatever of the
/// script is left — which is how a real terminal's reply lands: after the
/// query, and before any key pressed later.
fn run_loop(app: App, script: Vec<Event>, terminal_answers: Option<&[u8]>) -> Run {
    run_loop_with(app, script, terminal_answers, &Stubs::default())
}

/// `script`, then `q` `q` to quit.
fn then_quit(script: Vec<Event>) -> Rc<RefCell<VecDeque<Event>>> {
    let events: Rc<RefCell<VecDeque<Event>>> = Rc::new(RefCell::new(script.into()));
    events.borrow_mut().extend([
        Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
    ]);
    events
}

/// [`run_loop`], with `x`'s checks and session answering as `exec` says.
fn run_loop_with(
    app: App,
    script: Vec<Event>,
    terminal_answers: Option<&[u8]>,
    exec: &Stubs,
) -> Run {
    let log: Log = Rc::default();
    let events = then_quit(script);

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
    let container_fetches = Rc::new(Counter::new(0));
    let spawn_containers: ContainersFetcher = {
        let container_fetches = Rc::clone(&container_fetches);
        Box::new(move |_, _, _| {
            container_fetches.set(container_fetches.get() + 1);
            mpsc::channel().1
        })
    };
    let log_streams = Rc::new(Counter::new(0));
    let spawn_logs: LogsFetcher = {
        let log_streams = Rc::clone(&log_streams);
        Box::new(move |_, _, _, _, _| {
            log_streams.set(log_streams.get() + 1);
            crate::commands::spawn_stream(|_tx, _stop| async {})
        })
    };
    let exec_checks: Rc<RefCell<Vec<ExecTarget>>> = Rc::default();
    let held: Rc<RefCell<Held>> = Rc::default();
    let prepare_exec = exec_preparer(exec, &exec_checks, &held);
    let forwards_started: Rc<RefCell<Vec<PodPort>>> = Rc::default();
    let forwards_stopped = Arc::new(AtomicUsize::new(0));
    let start_forward = forward_starter(exec, &forwards_started, &forwards_stopped);
    let control_plane = control_plane_reader(exec);
    let drill = DrillFetchers {
        spawn_pods: &spawn_pods,
        spawn_containers: &spawn_containers,
        spawn_logs: &spawn_logs,
        spawn_control_plane: &control_plane.fetcher,
        prepare_exec: &prepare_exec,
        start_forward: &start_forward,
    };
    let session = {
        let log = Rc::clone(&log);
        let outcome = exec.session.clone();
        move |_: &str, _: &Plan| -> Result<(), FetchError> {
            log.borrow_mut().push(Step::Session);
            outcome.clone()
        }
    };

    let mut next_event = scripted(&events, &log, &held);
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
            session: &session,
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
        container_fetches: container_fetches.get(),
        log_streams: log_streams.get(),
        exec_checks: exec_checks.borrow().clone(),
        forwards_started: forwards_started.borrow().clone(),
        forwards_stopped,
        control_plane,
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

/// `L`, then enough `Esc` to back out of any drill-down and quit — `q` is a
/// no-op while drilled in.
fn l_then_back_out() -> Vec<Event> {
    let mut script = vec![Event::Key(press(KeyCode::Char('L')))];
    script.extend(std::iter::repeat_n(Event::Key(press(KeyCode::Esc)), 6));
    script
}

#[test]
fn a_successful_l_reopens_the_log_whose_refusal_offered_it() {
    let mut app = app_with_container();
    app.on_key(press(KeyCode::Enter));
    app.apply_log_event(LogEvent::Refused("refused".to_owned()));

    let run = run_loop(app, l_then_back_out(), None);

    assert_eq!(run.log_streams, 1);
    // The node pane refetches on `L` as it always has.
    assert_eq!(run.node_fetches, 1);
}

#[test]
fn a_successful_l_refetches_the_containers_whose_refusal_offered_it() {
    let mut app = app_with_pod();
    app.on_key(press(KeyCode::Enter));
    app.apply_containers(Err(refused("prod rejected your credentials")));

    let run = run_loop(app, l_then_back_out(), None);

    assert_eq!(run.container_fetches, 1);
}

#[test]
fn l_leaves_a_container_pane_that_loaded_alone() {
    let mut app = app_with_container();
    app.apply_nodes(Err(refused("prod rejected your credentials")));

    let run = run_loop(app, l_then_back_out(), None);

    assert_eq!(run.container_fetches, 0);
    assert_eq!(run.node_fetches, 1);
}

// --- `x`: a shell from the dashboard ---

fn text(screen: &ratatui::buffer::Buffer) -> String {
    screen
        .content()
        .chunks(usize::from(screen.area.width))
        .map(|row| row.iter().map(Cell::symbol).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn x_on_a_container_checks_it_then_hands_the_terminal_to_the_shell() {
    let run = run_loop(
        app_with_container(),
        vec![Event::Key(press(KeyCode::Char('x'))), ctrl_c()],
        None,
    );

    assert_eq!(
        run.exec_checks,
        [ExecTarget {
            namespace: "default".to_owned(),
            pod: "api-1".to_owned(),
            container: Some("app".to_owned()),
        }]
    );
    // The key is read, the session runs, and the very next thing is a frame:
    // the dashboard is back on screen before it waits for another key.
    let session = run.log.iter().position(|&step| step == Step::Session);
    let session = session.expect("the session never ran");
    assert_eq!(run.log[session - 1], Step::Read, "{:?}", run.log);
    assert_eq!(run.log[session + 1], Step::Frame, "{:?}", run.log);
}

#[test]
fn x_on_a_pod_asks_for_its_default_container() {
    let run = run_loop(
        app_with_pod(),
        vec![Event::Key(press(KeyCode::Char('x'))), ctrl_c()],
        None,
    );

    assert_eq!(run.exec_checks.len(), 1);
    assert_eq!(run.exec_checks[0].pod, "api-1");
    assert_eq!(run.exec_checks[0].container, None);
}

#[test]
fn after_the_shell_exits_the_dashboard_is_redrawn_as_it_was_left() {
    let with_shell = run_loop(
        app_with_container(),
        vec![Event::Key(press(KeyCode::Char('x'))), ctrl_c()],
        None,
    );
    let without = run_loop(app_with_container(), vec![ctrl_c()], None);

    assert_eq!(with_shell.screen, without.screen);
}

#[test]
fn a_refused_check_says_why_on_the_status_line_and_never_leaves_the_screen() {
    let exec = Stubs {
        checked: Err(FetchError {
            message: "container app in pod api-1 is not running (CrashLoopBackOff).\n\
                      Open its log and press p to see how its last run ended."
                .to_owned(),
            credentials: false,
        }),
        ..Stubs::default()
    };
    let run = run_loop_with(
        app_with_container(),
        vec![Event::Key(press(KeyCode::Char('x'))), ctrl_c()],
        None,
        &exec,
    );

    assert!(!run.log.contains(&Step::Session), "{:?}", run.log);
    let screen = text(&run.screen);
    assert!(
        screen.contains("is not running (CrashLoopBackOff)."),
        "{screen}"
    );
    assert!(
        screen.contains("press p to see how its last run ended"),
        "{screen}"
    );
    // The pane under it is untouched.
    assert!(screen.contains("› api-1 "), "{screen}");
}

#[test]
fn the_next_key_clears_a_refusal_from_the_status_line() {
    let exec = Stubs {
        checked: Err(FetchError {
            message: "pod api-1 has no shell.".to_owned(),
            credentials: false,
        }),
        ..Stubs::default()
    };
    let run = run_loop_with(
        app_with_container(),
        vec![
            Event::Key(press(KeyCode::Char('x'))),
            Event::Key(press(KeyCode::Char('j'))),
            ctrl_c(),
        ],
        None,
        &exec,
    );

    assert!(!text(&run.screen).contains("has no shell"));
}

#[test]
fn esc_cancels_the_check_and_its_late_answer_opens_nothing() {
    let exec = Stubs {
        held: true,
        ..Stubs::default()
    };
    let run = run_loop_with(
        app_with_container(),
        vec![
            Event::Key(press(KeyCode::Char('x'))),
            Event::Key(press(KeyCode::Esc)),
            Event::FocusGained,
            ctrl_c(),
        ],
        None,
        &exec,
    );

    assert_eq!(run.exec_checks.len(), 1);
    assert!(!run.log.contains(&Step::Session), "{:?}", run.log);
    let screen = text(&run.screen);
    assert!(!screen.contains("Opening a shell"), "{screen}");
    // Esc cancelled rather than backing out of the pane.
    assert!(screen.contains("› api-1 "), "{screen}");
}

#[test]
fn a_check_still_running_shows_that_it_is_and_how_to_cancel_it() {
    let exec = Stubs {
        held: true,
        ..Stubs::default()
    };
    let run = run_loop_with(
        app_with_container(),
        vec![Event::Key(press(KeyCode::Char('x'))), ctrl_c()],
        None,
        &exec,
    );

    let screen = text(&run.screen);
    assert!(
        screen.contains("Opening a shell in container app of pod api-1…   esc cancel"),
        "{screen}"
    );
}

#[test]
fn leaving_the_pane_cancels_the_check() {
    let exec = Stubs {
        held: true,
        ..Stubs::default()
    };
    let run = run_loop_with(
        app_with_container(),
        vec![
            Event::Key(press(KeyCode::Char('x'))),
            Event::Key(press(KeyCode::Left)),
            Event::FocusGained,
            ctrl_c(),
        ],
        None,
        &exec,
    );

    assert!(!run.log.contains(&Step::Session), "{:?}", run.log);
}

#[test]
fn a_session_that_broke_says_why_once_the_dashboard_is_back() {
    let exec = Stubs {
        session: Err(FetchError {
            message: "the connection to pod api-1 closed before `/bin/sh` reported how it ended."
                .to_owned(),
            credentials: false,
        }),
        ..Stubs::default()
    };
    let run = run_loop_with(
        app_with_container(),
        vec![Event::Key(press(KeyCode::Char('x'))), ctrl_c()],
        None,
        &exec,
    );

    assert!(run.log.contains(&Step::Session));
    let screen = text(&run.screen);
    assert!(
        screen.contains("the connection to pod api-1 closed"),
        "{screen}"
    );
}

#[test]
fn a_credential_refusal_from_the_check_offers_l() {
    let exec = Stubs {
        checked: Err(FetchError {
            message: "prod rejected your credentials.".to_owned(),
            credentials: true,
        }),
        ..Stubs::default()
    };
    let run = run_loop_with(
        app_with_container(),
        vec![Event::Key(press(KeyCode::Char('x'))), ctrl_c()],
        None,
        &exec,
    );

    assert!(text(&run.screen).contains("L log in"));
}

#[test]
fn x_on_the_node_pane_checks_nothing() {
    let run = run_loop(
        app(),
        vec![Event::Key(press(KeyCode::Char('x'))), ctrl_c()],
        None,
    );

    assert_eq!(run.exec_checks, Vec::<ExecTarget>::new());
    assert!(!run.log.contains(&Step::Session));
}

/// [`Stubs`] whose every forward reports `events` at once.
fn forwarding(events: Vec<ForwardEvent>) -> Stubs {
    Stubs {
        forward_events: events,
        ..Stubs::default()
    }
}

fn listening_on_8080() -> ForwardEvent {
    ForwardEvent::Listening {
        url: "http://127.0.0.1:8080".to_owned(),
        notes: Vec::new(),
    }
}

/// `j` onto `web`'s port 8080 in [`app_with_ports`], then `f`.
fn forward_8080() -> Vec<Event> {
    vec![
        Event::Key(press(KeyCode::Char('j'))),
        Event::Key(press(KeyCode::Char('f'))),
    ]
}

/// Wait up to two seconds for `count` forwards' threads to have been told
/// to stop — their handles dropped, their ports closed.
fn stopped(run: &Run, count: usize) -> bool {
    reaches(&run.forwards_stopped, count)
}

/// Whether `counter` reaches `count` within a couple of seconds: a dropped
/// handle is seen on the stream's own thread, not this one.
fn reaches(counter: &AtomicUsize, count: usize) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while counter.load(Ordering::SeqCst) < count {
        if std::time::Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

#[test]
fn f_on_a_port_starts_a_forward_whose_url_is_on_the_next_frame() {
    let mut script = forward_8080();
    script.push(ctrl_c());
    let run = run_loop_with(
        app_with_ports(),
        script,
        None,
        &forwarding(vec![listening_on_8080(), ForwardEvent::Opened]),
    );

    assert_eq!(
        run.forwards_started,
        [PodPort {
            namespace: "default".to_owned(),
            pod: "api-1".to_owned(),
            port: 8080,
        }]
    );
    let screen = text(&run.screen);
    assert!(
        screen.contains("http://127.0.0.1:8080 → pod api-1 port 8080  1 connection"),
        "{screen}"
    );
    assert!(
        screen.contains("port 8080 (http)  → http://127.0.0.1:8080"),
        "{screen}"
    );
}

#[test]
fn capital_f_closes_the_port_and_takes_it_off_the_strip() {
    let mut script = forward_8080();
    script.extend([Event::Key(press(KeyCode::Char('F'))), ctrl_c()]);
    let run = run_loop_with(
        app_with_ports(),
        script,
        None,
        &forwarding(vec![listening_on_8080()]),
    );

    assert!(stopped(&run, 1), "the forward was never told to stop");
    assert!(!text(&run.screen).contains("Forwards"));
}

#[test]
fn quitting_ends_every_forward_the_dashboard_started() {
    let mut script = forward_8080();
    script.push(ctrl_c());
    let run = run_loop_with(
        app_with_ports(),
        script,
        None,
        &forwarding(vec![listening_on_8080()]),
    );

    assert!(stopped(&run, 1), "a forward outlived the dashboard");
}

#[test]
fn a_forward_that_stops_by_itself_says_why_on_the_strip_and_raises_nothing() {
    let mut script = forward_8080();
    script.push(ctrl_c());
    let run = run_loop_with(
        app_with_ports(),
        script,
        None,
        &forwarding(vec![ForwardEvent::Ended {
            message:
                "could not listen on 127.0.0.1:8080: something else is already listening there."
                    .to_owned(),
            credentials: false,
        }]),
    );

    let screen = text(&run.screen);
    assert!(screen.contains("pod api-1 port 8080  stopped"), "{screen}");
    assert!(
        screen.contains("could not listen on 127.0.0.1:8080"),
        "{screen}"
    );
    // The pane is as it was: the container list, still under its title.
    assert!(screen.contains("Overview › worker-1 › api-1"), "{screen}");
    assert!(screen.contains("CONTAINERS"), "{screen}");
}

#[test]
fn a_successful_l_starts_again_a_forward_refused_for_credentials() {
    let mut script = forward_8080();
    script.extend([Event::Key(press(KeyCode::Char('L'))), ctrl_c()]);
    let run = run_loop_with(
        app_with_ports(),
        script,
        None,
        &forwarding(vec![ForwardEvent::Ended {
            message: "beta rejected your credentials.".to_owned(),
            credentials: true,
        }]),
    );

    assert_eq!(run.forwards_started.len(), 2, "the forward was not retried");
}

// --- `C`: the control plane's logs ---

fn reading(updates: Vec<ControlPlaneUpdate>) -> Stubs {
    Stubs {
        control_plane: updates,
        ..Stubs::default()
    }
}

fn key(c: char) -> Event {
    Event::Key(press(KeyCode::Char(c)))
}

#[test]
fn c_starts_one_read_of_the_audit_log_and_esc_stops_it() {
    let run = run_loop(app(), vec![key('C'), Event::Key(press(KeyCode::Esc))], None);

    assert_eq!(run.control_plane.reads(), [LogType::Audit]);
    assert!(reaches(&run.control_plane.stopped, 1));
}

#[test]
fn a_read_s_lines_are_on_the_next_frame() {
    let run = run_loop_with(
        app(),
        vec![key('C'), key('j'), ctrl_c()],
        None,
        &reading(vec![ControlPlaneUpdate::Read {
            lines: vec!["2026-10-09T06:12:40Z  Admin/alice  delete  pods shop/api  200".to_owned()],
            note: None,
        }]),
    );

    let screen = text(&run.screen);
    assert!(
        screen.contains("2026-10-09T06:12:40Z  Admin/alice  delete  pods shop/api  200"),
        "{screen}"
    );
    assert!(screen.contains("Control plane › audit"), "{screen}");
}

#[test]
fn t_reads_the_next_type_and_stops_the_read_before_it() {
    let run = run_loop(app(), vec![key('C'), key('t'), key('T'), ctrl_c()], None);

    assert_eq!(
        run.control_plane.reads(),
        [LogType::Audit, LogType::Authenticator, LogType::Audit]
    );
    assert!(reaches(&run.control_plane.stopped, 2));
}

#[test]
fn r_reads_a_type_that_was_off_again() {
    let run = run_loop_with(
        app(),
        vec![key('C'), key('r'), ctrl_c()],
        None,
        &reading(vec![ControlPlaneUpdate::Off("off".to_owned())]),
    );

    assert_eq!(run.control_plane.reads(), [LogType::Audit, LogType::Audit]);
}

#[test]
fn r_leaves_a_read_that_is_going_alone() {
    let run = run_loop_with(
        app(),
        vec![key('C'), key('r'), key('r'), ctrl_c()],
        None,
        &reading(vec![ControlPlaneUpdate::Read {
            lines: Vec::new(),
            note: None,
        }]),
    );

    assert_eq!(run.control_plane.reads(), [LogType::Audit]);
}

#[test]
fn a_successful_l_reads_again_the_control_plane_log_whose_refusal_offered_it() {
    let refused = ControlPlaneUpdate::Failed(FetchError {
        message: "AWS refused: expired".to_owned(),
        credentials: true,
    });
    // `j` gives the loop a pass to take the refusal before `L` is pressed.
    let mut script = vec![key('C'), key('j')];
    script.extend(l_then_back_out());

    let run = run_loop_with(app(), script, None, &reading(vec![refused]));

    assert_eq!(run.control_plane.reads(), [LogType::Audit, LogType::Audit]);
}
