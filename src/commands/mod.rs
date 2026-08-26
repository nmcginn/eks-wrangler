//! One module per user-facing command.
//!
//! Commands are written as pure functions returning the text they want printed
//! wherever that is possible. Rendering and I/O stay separable so output can be
//! asserted on in unit tests instead of eyeballed.

use std::future::Future;
use std::sync::mpsc;

use anyhow::{Context as _, Result};

pub mod contexts;
pub mod credentials;
pub mod nodes;
pub mod pods;

/// A failed background fetch, as the dashboard receives it.
///
/// The panes render `message` and nothing else — it is already a full sentence
/// from `k8s::client::explain`. `credentials` is the one fact about it they
/// cannot recover from the text: whether this is the failure a fresh AWS login
/// would fix, which is what decides whether `L` does anything.
///
/// It exists because the classification is lost at the thread boundary.
/// [`spawn`] hands back whatever the future produced, and by then a typed
/// `k8s::client::Error` has been flattened into an `anyhow::Error` inside a
/// `Result` the receiving end only knows how to print. Asking the question
/// while the typed error is still in hand — in [`Self::of`] — and carrying the
/// answer across is what stops the dashboard from re-deriving it by matching on
/// English prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchError {
    pub message: String,
    pub credentials: bool,
}

impl FetchError {
    /// Classify and flatten a failed fetch on its way back to the dashboard.
    #[must_use]
    pub fn of(error: &anyhow::Error) -> Self {
        Self {
            // `{:#}` prints the whole context chain on one line, which is what
            // a pane has room for.
            message: format!("{error:#}"),
            credentials: credentials::refused_credentials(error),
        }
    }
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Run one async command to completion.
///
/// The runtime is built here, per invocation, rather than by wrapping `main` in
/// `#[tokio::main]`: `eks contexts` and `eks use` touch nothing but the
/// filesystem, and they should not pay for a reactor they never use. A
/// current-thread runtime is enough — a one-shot command spends its life
/// waiting on a single request, and spawning worker threads to watch it idle
/// would only cost startup time.
///
/// The runtime is shut down rather than dropped, and that line is the second
/// half of `--timeout` covering the credential helper. Dropping a runtime waits
/// for its blocking tasks to finish, and exactly one blocking task exists in
/// this tool: the kubeconfig's exec plugin, run inside `Client::try_from` (see
/// [`crate::k8s::client`]). A helper that never exits cannot be cancelled and
/// cannot be killed from here, so once the budget has given up on it, waiting
/// for it at the door would reinstate the hang the budget just ended.
pub fn block_on<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("could not start the async runtime needed to talk to the cluster")?;

    let outcome = runtime.block_on(future);

    // Abandoned rather than awaited: the thread finishes on its own if the
    // helper ever exits, and returning from `main` ends the process either way.
    runtime.shutdown_background();

    outcome
}

/// Run one async computation to completion on a background OS thread,
/// delivering its result over a channel instead of returning it.
///
/// The counterpart to [`block_on`] for a caller that cannot block: the
/// dashboard's render loop has to acknowledge a keypress within one frame, so
/// it cannot wait on a cluster request the way a one-shot command does. This
/// spawns a plain thread rather than a `tokio` worker — nothing here wants a
/// shared multi-thread runtime for one request at a time — builds the same
/// kind of current-thread runtime `block_on` does, and shuts it down the same
/// way: a hung credential helper is left running rather than waited for a
/// second time, for the reason `block_on`'s own doc comment gives.
///
/// A caller who stops listening (drops the [`mpsc::Receiver`]) simply never
/// hears back; `tx.send` failing is not an error worth reporting; there is
/// nobody left to report it to.
pub fn spawn<T: Send + 'static>(
    future: impl Future<Output = T> + Send + 'static,
) -> mpsc::Receiver<T> {
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            // Nothing here fits every caller's `T` well enough to report this
            // with; the receiver simply never hears back, exactly as it would
            // if the send below failed. Resource exhaustion, rare enough not
            // to be worth constraining `T` to work around.
            return;
        };

        let outcome = runtime.block_on(future);
        runtime.shutdown_background();
        let _ = tx.send(outcome);
    });

    rx
}

/// A handle to a background stream, cancelled when this is dropped.
///
/// [`spawn`] has no way to say "stop" — its future runs once and the receiver
/// is the whole contract. A live log tail is a different shape: it has no
/// natural end a caller waits for, and the moment that matters is the reader
/// leaving the view that was watching it. Cancellation here can reach
/// *inside* the running task, unlike the credential helper `block_on`/`spawn`
/// abandon rather than stop (see their own doc comments): a log stream is
/// ordinary async I/O, so a signal the task is `select!`-ing against actually
/// interrupts an idle read instead of a wait nothing can end.
#[derive(Debug)]
pub struct StreamHandle(Option<tokio::sync::oneshot::Sender<()>>);

impl Drop for StreamHandle {
    fn drop(&mut self) {
        if let Some(stop) = self.0.take() {
            // The receiving end is gone once the task has already finished on
            // its own — nothing to tell it, and nothing wrong either.
            let _ = stop.send(());
        }
    }
}

/// Run a task that sends zero or more values back over a channel, until it
/// either finishes on its own or the returned [`StreamHandle`] is dropped.
///
/// `task` is handed the sender to push results into and a `stop` future that
/// resolves the instant the handle is dropped; racing it against a read
/// inside a `tokio::select!` loop is what lets a caller end a live connection
/// rather than only stop waiting for one. Like [`spawn`], this runs on its
/// own OS thread with a fresh current-thread runtime, shut down rather than
/// dropped once the task returns — see that function's doc comment for why.
pub fn spawn_stream<T, F, Fut>(task: F) -> (mpsc::Receiver<T>, StreamHandle)
where
    T: Send + 'static,
    F: FnOnce(mpsc::Sender<T>, tokio::sync::oneshot::Receiver<()>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();

    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };

        runtime.block_on(task(tx, stop_rx));
        runtime.shutdown_background();
    });

    (rx, StreamHandle(Some(stop_tx)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::time::Duration;

    use super::*;

    #[test]
    fn block_on_returns_what_the_future_resolved_to() {
        let value = block_on(async { Ok(21 * 2) }).unwrap();
        assert_eq!(value, 42);
    }

    #[test]
    fn block_on_propagates_failure_rather_than_panicking() {
        let error = block_on(async { Err::<(), _>(anyhow::anyhow!("nope")) }).unwrap_err();
        assert_eq!(error.to_string(), "nope");
    }

    #[test]
    fn block_on_does_not_wait_for_a_blocking_task_that_will_not_end() {
        // The credential-helper case with no credential helper in it: a
        // blocking task cannot be cancelled, so the only way out of one that
        // has stopped answering is to stop waiting for it. Dropping the
        // runtime here instead of shutting it down would make this test take
        // thirty seconds — which is exactly the hang `--timeout` ends.
        let started = std::time::Instant::now();

        let value = block_on(async {
            let _abandoned =
                tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_secs(30)));
            Ok("not waiting")
        })
        .unwrap();
        let elapsed = started.elapsed();

        assert_eq!(value, "not waiting");
        assert!(
            elapsed < Duration::from_secs(5),
            "waited {elapsed:?} at the door for a task that had already been given up on"
        );
    }

    #[test]
    fn block_on_drives_timers_so_a_command_can_wait_on_one() {
        // `enable_all` is what makes this work; without the time driver a
        // sleeping request would hang forever instead of timing out.
        let value = block_on(async {
            tokio::time::sleep(Duration::from_millis(1)).await;
            Ok("awake")
        })
        .unwrap();

        assert_eq!(value, "awake");
    }

    #[test]
    fn spawn_sends_back_what_the_future_resolved_to() {
        let rx = spawn(async { 21 * 2 });
        assert_eq!(rx.recv().unwrap(), 42);
    }

    #[test]
    fn spawn_does_not_wait_for_a_blocking_task_that_will_not_end() {
        // The same abandonment `block_on` relies on, proven on the thread
        // `spawn` actually runs on rather than the calling one: if the
        // runtime were dropped instead of shut down, this would take thirty
        // seconds waiting for a task nothing can cancel.
        let started = std::time::Instant::now();

        let rx = spawn(async {
            let _abandoned =
                tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_secs(30)));
            "not waiting"
        });
        let value = rx.recv().unwrap();
        let elapsed = started.elapsed();

        assert_eq!(value, "not waiting");
        assert!(
            elapsed < Duration::from_secs(5),
            "waited {elapsed:?} for a task that had already been given up on"
        );
    }

    #[test]
    fn spawn_stream_delivers_every_value_the_task_sends() {
        let (rx, _handle) = spawn_stream(|tx, _stop| async move {
            for value in [1, 2, 3] {
                let _ = tx.send(value);
            }
        });

        assert_eq!(rx.iter().collect::<Vec<_>>(), vec![1, 2, 3]);
    }

    #[test]
    fn dropping_the_handle_ends_a_task_that_would_otherwise_run_forever() {
        // The whole reason this exists rather than `spawn`: a log tail has no
        // natural end, and the only way out is a signal the task itself can
        // notice — proven here by racing it against a future that never
        // resolves on its own.
        let started = std::time::Instant::now();

        let (rx, handle) = spawn_stream(|tx, mut stop| async move {
            let _ = tx.send("first");
            // Blocks until either the stop signal fires or the sleep ends —
            // the sleep is the "forever" this test must not wait out.
            tokio::select! {
                _ = &mut stop => {}
                () = tokio::time::sleep(Duration::from_secs(3600)) => {}
            }
            let _ = tx.send("after select");
        });

        assert_eq!(rx.recv().unwrap(), "first");
        drop(handle);
        // The task's second send only happens once `select!` returns, so
        // receiving it here proves the drop actually interrupted the sleep
        // rather than merely stopping this test from waiting on it.
        assert_eq!(rx.recv().unwrap(), "after select");

        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "waited {elapsed:?} for a stream that should have been cancelled"
        );
    }

    #[test]
    fn a_task_that_finishes_on_its_own_needs_no_cancellation() {
        let (rx, handle) = spawn_stream(|tx, _stop| async move {
            let _ = tx.send("done");
        });

        assert_eq!(rx.recv().unwrap(), "done");
        drop(handle);
    }
}
