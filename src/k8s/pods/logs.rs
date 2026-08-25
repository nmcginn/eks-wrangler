//! Following a container's log, live.
//!
//! Every other listing in this tool is a request that answers once. A log is
//! the opposite shape: `kube`'s [`LogParams::follow`] keeps the HTTP response
//! open and the API server writes a new line onto it as the container prints
//! one, for as long as anybody is reading. What this module gets right is
//! therefore not a fetch but a *stream* — [`LogEvent`] is what the connection
//! hands back a piece at a time, and the pump that turns it into those events
//! lives in [`crate::commands::pods::spawn_stream_logs`], behind
//! [`crate::commands::spawn_stream`] so leaving the pane can actually cancel
//! the read rather than merely stop waiting for one.

use kube::api::LogParams;

/// How many lines of history to open a log with, before following whatever
/// is printed after.
///
/// `kubectl logs` defaults to every line the kubelet has kept, which on a
/// long-lived container can be megabytes the reader almost never wants —
/// they asked to see what a container is doing, not to download its whole
/// history. A couple of screens' worth of recent context is enough to say
/// what led up to now; `follow` carries everything from here on regardless.
pub const TAIL_LINES: i64 = 200;

/// The parameters one container's log is opened with: this container by
/// name, followed live, starting from [`TAIL_LINES`] lines of backlog.
#[must_use]
pub fn params(container: &str) -> LogParams {
    LogParams {
        container: Some(container.to_owned()),
        follow: true,
        tail_lines: Some(TAIL_LINES),
        ..LogParams::default()
    }
}

/// One piece of a container's log stream, as the pane's channel carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogEvent {
    /// One line of output.
    Line(String),
    /// The stream will not produce any more lines. `None` is a clean end —
    /// the API server closed the connection because the container's log
    /// itself ended, most often because the container has terminated.
    /// `Some` is a failure, already worded through [`crate::k8s::explain`],
    /// which covers both a connection that never opened and one that broke
    /// partway through.
    Ended(Option<String>),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn params_name_the_container_and_ask_to_follow_from_a_tail() {
        let lp = params("app");

        assert_eq!(lp.container.as_deref(), Some("app"));
        assert!(lp.follow);
        assert_eq!(lp.tail_lines, Some(TAIL_LINES));
    }

    #[test]
    fn params_leave_everything_else_at_the_grammars_default() {
        let lp = params("app");

        assert!(!lp.previous);
        assert_eq!(lp.since_seconds, None);
        assert_eq!(lp.limit_bytes, None);
        assert!(!lp.timestamps);
    }
}
