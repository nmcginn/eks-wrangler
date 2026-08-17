//! One module per user-facing command.
//!
//! Commands are written as pure functions returning the text they want printed
//! wherever that is possible. Rendering and I/O stay separable so output can be
//! asserted on in unit tests instead of eyeballed.

use std::future::Future;

use anyhow::{Context as _, Result};

pub mod contexts;
pub mod nodes;

/// Run one async command to completion.
///
/// The runtime is built here, per invocation, rather than by wrapping `main` in
/// `#[tokio::main]`: `eks contexts` and `eks use` touch nothing but the
/// filesystem, and they should not pay for a reactor they never use. A
/// current-thread runtime is enough — a one-shot command spends its life
/// waiting on a single request, and spawning worker threads to watch it idle
/// would only cost startup time.
pub fn block_on<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("could not start the async runtime needed to talk to the cluster")?;

    runtime.block_on(future)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

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
    fn block_on_drives_timers_so_a_command_can_wait_on_one() {
        // `enable_all` is what makes this work; without the time driver a
        // sleeping request would hang forever instead of timing out.
        let value = block_on(async {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            Ok("awake")
        })
        .unwrap();

        assert_eq!(value, "awake");
    }
}
