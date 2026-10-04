//! The keyboard, for a session the dashboard starts: a reader that stops
//! when the session does.
//!
//! `eks exec` reads its stdin through `tokio::io::stdin`, which reads on a
//! blocking thread that nothing can interrupt. That is fine for a command that
//! exits when its session ends. The dashboard does not exit: it takes the
//! terminal back. The read still pending on that thread would then take the
//! first key the user presses there and send it nowhere.
//!
//! So this waits for input with `poll(2)` and a short timeout, and reads only
//! once there is something to read. Between two waits it checks whether the
//! session is over. Dropping a [`Keyboard`] therefore stops its thread within
//! one [`INTERVAL`] with no read outstanding, and `Drop` waits for that, so
//! the next byte typed reaches the dashboard.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::mpsc;

/// How long one wait for input lasts, and so the longest a dropped
/// [`Keyboard`] takes to stop. Short enough to go unnoticed between a shell's
/// `exit` and the dashboard's first frame.
const INTERVAL: Duration = Duration::from_millis(50);

/// What one wait for input produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Waited {
    /// The interval passed with nothing to read.
    Nothing,
    /// Bytes the user typed. Never empty.
    Bytes(Vec<u8>),
    /// The input has ended.
    End,
}

/// Keystrokes as an [`AsyncRead`], from a thread that stops when this is
/// dropped.
#[derive(Debug)]
pub(crate) struct Keyboard {
    received: mpsc::UnboundedReceiver<Vec<u8>>,
    /// The bytes most recently received, and how far into them reading has
    /// got — a read with a smaller buffer than one burst of typing takes the
    /// rest next time.
    pending: Vec<u8>,
    offset: usize,
    stop: Arc<AtomicBool>,
    pump: Option<JoinHandle<()>>,
}

impl Keyboard {
    /// The process's own stdin.
    #[cfg(unix)]
    pub(crate) fn stdin() -> Self {
        Self::over(wait_and_read)
    }

    /// Read from `source`, which waits up to the interval it is given and
    /// says what arrived. A test hands in a script.
    pub(crate) fn over(
        source: impl FnMut(Duration) -> io::Result<Waited> + Send + 'static,
    ) -> Self {
        let (sender, received) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let pump = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || pump(source, &sender, &stop))
        };
        Self {
            received,
            pending: Vec::new(),
            offset: 0,
            stop,
            pump: Some(pump),
        }
    }
}

impl Drop for Keyboard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(pump) = self.pump.take() {
            // At most one interval: the thread is either waiting, which times
            // out, or handing over bytes it has already read.
            let _ = pump.join();
        }
    }
}

/// Wait for input and pass it on, until told to stop, until the input ends,
/// or until nobody is listening.
fn pump(
    mut source: impl FnMut(Duration) -> io::Result<Waited>,
    sender: &mpsc::UnboundedSender<Vec<u8>>,
    stop: &AtomicBool,
) {
    while !stop.load(Ordering::Acquire) {
        match source(INTERVAL) {
            Ok(Waited::Nothing) => {}
            Ok(Waited::Bytes(bytes)) => {
                // An empty read would read as the end of input to the session.
                if !bytes.is_empty() && sender.send(bytes).is_err() {
                    return;
                }
            }
            Ok(Waited::End) => return,
            Err(error) => {
                tracing::debug!(%error, "reading the keyboard failed");
                return;
            }
        }
    }
}

impl AsyncRead for Keyboard {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        if this.offset >= this.pending.len() {
            match this.received.poll_recv(cx) {
                Poll::Pending => return Poll::Pending,
                // Nothing filled is the end of input.
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Ready(Some(bytes)) => {
                    this.pending = bytes;
                    this.offset = 0;
                }
            }
        }
        let available = &this.pending[this.offset..];
        let taken = available.len().min(buf.remaining());
        buf.put_slice(&available[..taken]);
        this.offset += taken;
        Poll::Ready(Ok(()))
    }
}

/// Wait up to `timeout` for stdin to have something to read, and read it.
///
/// The read goes straight to the file descriptor rather than through
/// `std::io::stdin()`, whose buffer could keep bytes past the session's end
/// where nothing would ever read them.
#[cfg(unix)]
fn wait_and_read(timeout: Duration) -> io::Result<Waited> {
    use std::os::fd::AsFd;

    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    use rustix::io::Errno;

    let stdin = io::stdin();
    let fd = stdin.as_fd();
    let timeout = Timespec::try_from(timeout).unwrap_or(Timespec {
        tv_sec: 0,
        tv_nsec: 50_000_000,
    });
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    match poll(&mut fds, Some(&timeout)) {
        Ok(0) | Err(Errno::INTR) => return Ok(Waited::Nothing),
        Ok(_) => {}
        Err(error) => return Err(error.into()),
    }

    let mut buffer = vec![0; 4096];
    match rustix::io::read(fd, &mut buffer) {
        Ok(0) => Ok(Waited::End),
        Ok(read) => {
            buffer.truncate(read);
            Ok(Waited::Bytes(buffer))
        }
        Err(Errno::INTR | Errno::AGAIN) => Ok(Waited::Nothing),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Mutex;

    use tokio::io::AsyncReadExt;

    use super::*;

    /// A source that hands out `script` one wait at a time, then waits with
    /// nothing to read, counting every wait it is asked for.
    fn scripted(
        script: Vec<Waited>,
    ) -> (
        impl FnMut(Duration) -> io::Result<Waited>,
        Arc<Mutex<usize>>,
    ) {
        let waits = Arc::new(Mutex::new(0));
        let mut script = script.into_iter();
        let counted = Arc::clone(&waits);
        let source = move |interval: Duration| {
            *counted.lock().unwrap() += 1;
            Ok(script.next().unwrap_or_else(|| {
                std::thread::sleep(interval);
                Waited::Nothing
            }))
        };
        (source, waits)
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn typed_bytes_are_read_in_order_across_several_bursts() {
        let (source, _) = scripted(vec![
            Waited::Bytes(b"ls ".to_vec()),
            Waited::Nothing,
            Waited::Bytes(b"-la\r".to_vec()),
        ]);
        let mut keyboard = Keyboard::over(source);

        let mut read = vec![0; 7];
        runtime().block_on(keyboard.read_exact(&mut read)).unwrap();

        assert_eq!(read, b"ls -la\r");
    }

    #[test]
    fn a_burst_bigger_than_the_readers_buffer_is_kept_for_the_next_read() {
        let (source, _) = scripted(vec![Waited::Bytes(b"abcdef".to_vec())]);
        let mut keyboard = Keyboard::over(source);
        let runtime = runtime();

        let mut first = [0; 4];
        let got = runtime.block_on(keyboard.read(&mut first)).unwrap();
        assert_eq!(&first[..got], b"abcd");

        let mut rest = [0; 4];
        let got = runtime.block_on(keyboard.read(&mut rest)).unwrap();
        assert_eq!(&rest[..got], b"ef");
    }

    #[test]
    fn the_end_of_input_reads_as_the_end() {
        let (source, _) = scripted(vec![Waited::Bytes(b"x".to_vec()), Waited::End]);
        let mut keyboard = Keyboard::over(source);

        let mut read = Vec::new();
        runtime().block_on(keyboard.read_to_end(&mut read)).unwrap();

        assert_eq!(read, b"x");
    }

    #[test]
    fn an_empty_read_is_not_mistaken_for_the_end_of_input() {
        let (source, _) = scripted(vec![
            Waited::Bytes(Vec::new()),
            Waited::Bytes(b"y".to_vec()),
            Waited::End,
        ]);
        let mut keyboard = Keyboard::over(source);

        let mut read = Vec::new();
        runtime().block_on(keyboard.read_to_end(&mut read)).unwrap();

        assert_eq!(read, b"y");
    }

    #[test]
    fn once_dropped_the_keyboard_waits_for_nothing_more() {
        // The guarantee the dashboard relies on: after the session, nothing
        // is left asking the terminal for input, so the next key goes to the
        // dashboard rather than to a reader nobody is listening to.
        let (source, waits) = scripted(Vec::new());
        let keyboard = Keyboard::over(source);
        std::thread::sleep(INTERVAL * 2);

        drop(keyboard);
        let after_drop = *waits.lock().unwrap();
        std::thread::sleep(INTERVAL * 3);

        assert_eq!(*waits.lock().unwrap(), after_drop);
    }

    #[test]
    fn a_failing_source_ends_the_input_rather_than_spinning() {
        let source = |_: Duration| Err(io::Error::other("not a terminal"));
        let mut keyboard = Keyboard::over(source);

        let mut read = Vec::new();
        runtime().block_on(keyboard.read_to_end(&mut read)).unwrap();

        assert!(read.is_empty());
    }
}
