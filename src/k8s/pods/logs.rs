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

use k8s_openapi::jiff::Timestamp;
use kube::api::LogParams;

use crate::aws::logs::Since;

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
/// name, starting from [`TAIL_LINES`] lines of backlog.
///
/// `previous` asks for the log of the instance before the one currently
/// running — `kubectl logs -p` — which forces `follow` off regardless of
/// what the caller might otherwise want: a terminated container's log has
/// already stopped growing, so following it would wait forever for a line
/// that is never coming. The current instance's log is still followed live,
/// as it always was.
#[must_use]
pub fn params(container: &str, previous: bool) -> LogParams {
    LogParams {
        container: Some(container.to_owned()),
        follow: !previous,
        previous,
        tail_lines: Some(TAIL_LINES),
        ..LogParams::default()
    }
}

/// The parameters `eks logs` opens a running container's log with.
///
/// Unlike the dashboard's [`params`], no tail: `kubectl logs` prints every
/// line the kubelet kept, and a command whose output is piped into `grep` or
/// a file should too. `--since` narrows it instead. `--previous` turns
/// `follow` off for [`params`]' reason: that instance has stopped.
///
/// `timestamps` is `--json`'s: the kubelet then starts each line with the
/// time it was written, which [`split_timestamp`] takes off again so the
/// line can carry it as a field. Plain output never asks, so a pipe gets
/// exactly what `kubectl logs` would give it.
#[must_use]
pub fn command_params(
    container: &str,
    previous: bool,
    follow: bool,
    since: Option<Since>,
    timestamps: bool,
) -> LogParams {
    let (since_seconds, since_time) = match since {
        None => (None, None),
        Some(Since::Ago(span)) => (
            Some(i64::try_from(span.as_secs()).unwrap_or(i64::MAX)),
            None,
        ),
        Some(Since::At(at)) => (None, Some(at)),
    };
    LogParams {
        container: Some(container.to_owned()),
        follow: follow && !previous,
        previous,
        since_seconds,
        since_time,
        timestamps,
        ..LogParams::default()
    }
}

/// A line read with `timestamps=true`, split into the kubelet's time and the
/// line the container printed.
///
/// The kubelet writes the time as RFC 3339 with nanoseconds and one space
/// before the line, so the split is at the first space. A line whose prefix
/// does not read as a time (an API server that ignored the parameter) is
/// returned whole with no time: the line is the thing asked for, and losing
/// its first word to a parse would be worse than a `null`.
#[must_use]
pub fn split_timestamp(line: &str) -> (Option<Timestamp>, &str) {
    let (prefix, rest) = line.split_once(' ').unwrap_or((line, ""));
    match prefix.parse::<Timestamp>() {
        Ok(at) => (Some(at), rest),
        Err(_) => (None, line),
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
    /// The stream never opened, for a reason signing in again could fix —
    /// a refused credential, or a helper the dashboard would not let prompt.
    /// Read like `Ended(Some(_))`, and kept apart so the dashboard can offer
    /// `L` for it as it does for every other pane's refusal.
    Refused(String),
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn params_name_the_container_and_ask_to_follow_from_a_tail() {
        let lp = params("app", false);

        assert_eq!(lp.container.as_deref(), Some("app"));
        assert!(lp.follow);
        assert_eq!(lp.tail_lines, Some(TAIL_LINES));
    }

    #[test]
    fn params_leave_everything_else_at_the_grammars_default() {
        let lp = params("app", false);

        assert!(!lp.previous);
        assert_eq!(lp.since_seconds, None);
        assert_eq!(lp.limit_bytes, None);
        assert!(!lp.timestamps);
    }

    #[test]
    fn the_command_reads_every_line_kept_unless_since_narrows_it() {
        let lp = command_params("app", false, false, None, false);

        assert_eq!(lp.container.as_deref(), Some("app"));
        assert_eq!(lp.tail_lines, None);
        assert_eq!(lp.since_seconds, None);
        assert_eq!(lp.since_time, None);
        assert!(!lp.follow);
        assert!(!lp.previous);
        assert!(
            !lp.timestamps,
            "plain output is what `kubectl logs` prints, with no time in front"
        );
    }

    #[test]
    fn the_command_asks_for_timestamps_only_when_told_to() {
        assert!(command_params("app", false, true, None, true).timestamps);
        assert!(!command_params("app", false, true, None, false).timestamps);
    }

    #[test]
    fn a_kubelet_timestamp_is_split_from_the_line_it_starts() {
        let (at, line) = split_timestamp("2026-10-07T06:21:02.123456789Z GET /orders 200");

        assert_eq!(at, Some("2026-10-07T06:21:02.123456789Z".parse().unwrap()));
        assert_eq!(line, "GET /orders 200");
    }

    #[test]
    fn only_the_first_space_separates_time_from_line() {
        let (_, line) = split_timestamp("2026-10-07T06:21:02.1Z   indented  twice ");

        assert_eq!(line, "  indented  twice ");
    }

    #[test]
    fn an_empty_line_keeps_its_time() {
        assert_eq!(
            split_timestamp("2026-10-07T06:21:02Z "),
            (Some("2026-10-07T06:21:02Z".parse().unwrap()), "")
        );
        // A kubelet that dropped the trailing space as well.
        assert_eq!(
            split_timestamp("2026-10-07T06:21:02Z"),
            (Some("2026-10-07T06:21:02Z".parse().unwrap()), "")
        );
    }

    #[test]
    fn a_line_without_a_time_in_front_is_kept_whole() {
        assert_eq!(
            split_timestamp("listening on :8080"),
            (None, "listening on :8080")
        );
        assert_eq!(split_timestamp(""), (None, ""));
        assert_eq!(
            split_timestamp("2026-13-45 nope"),
            (None, "2026-13-45 nope")
        );
    }

    #[test]
    fn a_time_with_an_offset_is_still_a_time() {
        let (at, line) = split_timestamp("2026-10-07T08:21:02+02:00 hello");

        assert_eq!(at, Some("2026-10-07T06:21:02Z".parse().unwrap()));
        assert_eq!(line, "hello");
    }

    #[test]
    fn the_command_s_since_is_seconds_or_an_instant() {
        let ago = command_params(
            "app",
            false,
            true,
            Some(Since::Ago(std::time::Duration::from_secs(900))),
            false,
        );
        assert_eq!(ago.since_seconds, Some(900));
        assert!(ago.follow);

        let at = "2026-10-07T05:00:00Z".parse().unwrap();
        let instant = command_params("app", false, false, Some(Since::At(at)), false);
        assert_eq!(instant.since_time, Some(at));
        assert_eq!(instant.since_seconds, None);
    }

    #[test]
    fn the_command_never_follows_a_previous_instance() {
        let lp = command_params("app", true, true, None, false);

        assert!(lp.previous);
        assert!(!lp.follow);
    }

    #[test]
    fn previous_asks_for_the_prior_instance_and_never_follows() {
        let lp = params("app", true);

        assert!(lp.previous);
        assert!(
            !lp.follow,
            "a terminated container's log has stopped growing; following it would hang"
        );
        assert_eq!(lp.tail_lines, Some(TAIL_LINES));
    }
}
