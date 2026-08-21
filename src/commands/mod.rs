//! One module per user-facing command.
//!
//! Commands are written as pure functions returning the text they want printed
//! wherever that is possible. Rendering and I/O stay separable so output can be
//! asserted on in unit tests instead of eyeballed.

use std::future::Future;

use anyhow::{Context as _, Result};

pub mod contexts;
pub mod nodes;
pub mod pods;

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
}
