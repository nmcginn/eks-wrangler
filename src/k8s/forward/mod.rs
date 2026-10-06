//! `eks port-forward`: everything about it that can be decided without a
//! socket.
//!
//! [`spec`] reads what was typed, [`ports`] works out which port in the pod
//! that means, and [`choose`] which pod — and, later, whether to move to
//! another. This module holds what is left: which local port to listen on,
//! and the lines printed about each forward and each connection that fails.
//! `commands::forward` is the I/O around all of it.

use std::io;
use std::net::IpAddr;

use crate::k8s::forward::spec::{Kind, Local, Target};

pub mod choose;
pub mod ports;
pub mod spec;

/// How to bind the local end of one forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bind {
    /// This port, or fail: somebody typed it.
    Exactly(u16),
    /// This port if it can be had, and any free one otherwise.
    Prefer(u16),
    /// Any free port.
    Any,
}

/// Which local port a forward to `remote` listens on.
///
/// With no local port given, the remote's own number is the friendliest
/// choice — `localhost:5432` for a database, the same as in the docs — and
/// when it cannot be had, any port beats refusing to start. One that was
/// given is kept to, or refused.
#[must_use]
pub fn bind(local: Local, remote: u16) -> Bind {
    match local {
        Local::Same => Bind::Prefer(remote),
        Local::Any => Bind::Any,
        Local::Port(port) => Bind::Exactly(port),
    }
}

/// Why a preferred local port could not be had, in the few words the
/// forward's line has room for.
#[must_use]
pub fn unavailable(port: u16, error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::AddrInUse => format!("local port {port} is in use"),
        io::ErrorKind::PermissionDenied => format!("local port {port} needs root"),
        _ => format!("local port {port} could not be used ({error})"),
    }
}

/// An exact local port that could not be listened on, and what to type
/// instead.
#[must_use]
pub fn bind_failed(address: IpAddr, port: u16, remote: &str, error: &io::Error) -> String {
    let at = spec::url(address, port);
    let at = at.trim_start_matches("http://");
    let why = match error.kind() {
        io::ErrorKind::AddrInUse => "something else is already listening there".to_owned(),
        io::ErrorKind::PermissionDenied if port < 1024 => {
            "ports below 1024 need root on this machine".to_owned()
        }
        io::ErrorKind::AddrNotAvailable => "this machine has no such address".to_owned(),
        _ => error.to_string(),
    };
    format!(
        "could not listen on {at}: {why}.\n\
         Pick another local port, e.g. `{other}:{remote}`, or write `:{remote}` to let eks pick a free one.",
        other = another(port),
    )
}

/// A port near `port` to suggest in its place: ten thousand up, the way
/// people tend to write `15432:5432`, or down when that would be past the end.
fn another(port: u16) -> u16 {
    port.checked_add(10000)
        .unwrap_or_else(|| port.saturating_sub(10000))
}

/// Where one forward's connections land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landing<'a> {
    pub target: &'a Target,
    /// For `svc/`, the service port that was asked for — what the service's
    /// own clients dial.
    pub service_port: Option<u16>,
    pub pod: &'a str,
    pub pod_port: u16,
}

/// The line printed for one forward, starting with the URL to click.
///
/// `notes` go in brackets at the end: why the local port is not the remote's
/// own number when it was meant to be, so a person looking for `:5432` finds
/// out why it is `:54321` from the same line, and [`exposure`]'s warning.
#[must_use]
pub fn forwarding(url: &str, landing: &Landing<'_>, notes: &[String]) -> String {
    let Landing {
        target,
        service_port,
        pod,
        pod_port,
    } = landing;
    let to = match (target.kind, service_port) {
        (Kind::Pod, _) => format!("pod {pod} port {pod_port}"),
        (Kind::Service, Some(port)) => {
            format!("{target} port {port} (pod {pod} port {pod_port})")
        }
        _ => format!("{target} (pod {pod} port {pod_port})"),
    };
    if notes.is_empty() {
        return format!("{url} → {to}");
    }
    format!("{url} → {to}  ({})", notes.join("; "))
}

/// A note for a listener reachable from other machines.
///
/// Its URL says loopback (see [`spec::url`]), which is true but not the whole
/// truth: anyone who can reach this machine can reach the pod through it, and
/// that should be on screen rather than only in the flag somebody typed.
#[must_use]
pub fn exposure(address: IpAddr) -> Option<String> {
    if address.is_unspecified() {
        Some("open to other machines: listening on every interface".to_owned())
    } else if !address.is_loopback() {
        Some(format!("open to other machines: listening on {address}"))
    } else {
        None
    }
}

/// Printed once every forward is listening.
pub const STOP_HINT: &str = "Forwarding until Ctrl-C.";

/// Where a line about a forward is read, which decides the advice in it —
/// a command to run, or the pane already on screen. The diagnosis is the
/// same on both, as for `eks exec`'s own `Surface`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// `eks port-forward`.
    Command,
    /// `f` in the dashboard's pod-containers pane.
    Dashboard,
}

/// A connection the pod's end of the forward refused or dropped, from the
/// error the kubelet sent back.
///
/// The kubelet's own text is long and names network namespaces nobody needs
/// to see. "Connection refused" in it is by far the commonest case, and the
/// one with clear advice: nothing is listening on that port.
#[must_use]
pub fn connection_failed(
    target: &Target,
    pod: &str,
    port: u16,
    message: &str,
    surface: Surface,
) -> String {
    if message.contains("connection refused") {
        let check = match (surface, target.kind) {
            (Surface::Dashboard, _) => "Check which port the app listens on; the pane lists \
                 the ports each container declares, and an app may listen on one it does not."
                .to_owned(),
            (Surface::Command, Kind::Service) => format!(
                "Check that the service's targetPort is the port the app listens on; \
                 `eks port-forward {target}` with no port shows where each of its ports goes."
            ),
            (Surface::Command, _) => format!(
                "Check which port the app listens on; `eks port-forward {target}` with no port lists the ones it declares."
            ),
        };
        return format!(
            "pod {pod} refused a connection on port {port}: nothing in it is listening there.\n{check}"
        );
    }
    let message = message.trim();
    format!("a connection to pod {pod} port {port} failed: {message}")
}

/// The pod a dashboard forward was started on is not there any more.
///
/// Reached between the pane listing the pod and `f`: a rollout or an
/// eviction in the moment between. Its replacement, if it has one, has a new
/// name, so the advice is where to find it.
#[must_use]
pub fn pod_gone(pod: &str, namespace: &str) -> String {
    format!(
        "pod {pod} is no longer in namespace {namespace}, so there is nothing to forward to.\n\
         Go back to the node's pods and press r to see what replaced it."
    )
}

/// A connection arrived while no pod was ready to take it.
#[must_use]
pub fn refused_while_waiting(target: &Target) -> String {
    format!("refused a connection: no pod behind {target} is ready yet.")
}

/// One port of one pod, as the dashboard forwards it: always to that pod by
/// name, on loopback, with the pod's own port number preferred locally.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PodPort {
    pub namespace: String,
    pub pod: String,
    pub port: u16,
}

/// What a forward the dashboard started reports about itself while it runs.
///
/// The command line prints these as lines on stderr; the dashboard has no
/// stderr it can write to without tearing the screen, so the same moments
/// arrive here instead, for its forwards strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The local end is listening. `notes` are [`forwarding`]'s: why the
    /// port is not the pod's own number, when it is not.
    Listening { url: String, notes: Vec<String> },
    /// A local connection was accepted.
    Opened,
    /// A local connection finished, however it finished.
    Closed,
    /// Something went wrong that did not end the forward: a connection the
    /// pod refused, a look at the pod that failed, a pod shutting down.
    Problem(String),
    /// The forward has stopped and its port is closed: it could not start,
    /// or its pod is gone. `credentials` is set when a fresh login could fix
    /// it, as for every other dashboard fetch.
    Ended { message: String, credentials: bool },
}

/// Port-forwarding is not allowed for this person in `namespace`.
#[must_use]
pub fn forbidden(cluster: &str, namespace: &str) -> String {
    format!(
        "{cluster} will not let you forward ports from pods in namespace {namespace}: \
         your access is missing the `create` verb on `pods/portforward`.\n\
         Ask a cluster admin to grant it (the built-in `edit` and `admin` roles include it). \
         `kubectl auth can-i create pods/portforward -n {namespace}` checks whether you have it."
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    fn target(text: &str) -> Target {
        Target::parse(text).unwrap()
    }

    #[test]
    fn no_local_port_prefers_the_remote_s_own_number() {
        assert_eq!(bind(Local::Same, 5432), Bind::Prefer(5432));
    }

    #[test]
    fn a_typed_local_port_is_kept_to() {
        assert_eq!(bind(Local::Port(15432), 5432), Bind::Exactly(15432));
    }

    #[test]
    fn an_empty_local_port_takes_any() {
        assert_eq!(bind(Local::Any, 5432), Bind::Any);
    }

    #[test]
    fn a_taken_or_privileged_port_says_which_in_a_few_words() {
        let taken = io::Error::from(io::ErrorKind::AddrInUse);
        let root = io::Error::from(io::ErrorKind::PermissionDenied);
        assert_eq!(unavailable(80, &taken), "local port 80 is in use");
        assert_eq!(unavailable(80, &root), "local port 80 needs root");
    }

    #[test]
    fn an_exact_port_in_use_suggests_another_and_the_any_spelling() {
        let text = bind_failed(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            8080,
            "80",
            &io::Error::from(io::ErrorKind::AddrInUse),
        );
        assert!(
            text.starts_with(
                "could not listen on 127.0.0.1:8080: something else is already listening there."
            ),
            "{text}"
        );
        assert!(text.contains("`18080:80`"), "{text}");
        assert!(text.contains("`:80`"), "{text}");
    }

    #[test]
    fn an_exact_ipv6_port_is_written_with_brackets() {
        let text = bind_failed(
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            443,
            "http",
            &io::Error::from(io::ErrorKind::PermissionDenied),
        );
        assert!(
            text.starts_with("could not listen on [::1]:443: ports below 1024"),
            "{text}"
        );
        assert!(text.contains("`10443:http`"), "{text}");
    }

    #[test]
    fn the_suggested_port_never_runs_past_65535() {
        let text = bind_failed(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            65000,
            "80",
            &io::Error::from(io::ErrorKind::AddrInUse),
        );
        assert!(text.contains("`55000:80`"), "{text}");
    }

    #[test]
    fn a_pod_forward_line_names_the_pod_and_port() {
        let t = target("api");
        let line = forwarding(
            "http://127.0.0.1:8080",
            &Landing {
                target: &t,
                service_port: None,
                pod: "api-1",
                pod_port: 8080,
            },
            &[],
        );
        assert_eq!(line, "http://127.0.0.1:8080 → pod api-1 port 8080");
    }

    #[test]
    fn a_service_forward_line_names_both_ports() {
        let t = target("svc/api");
        let line = forwarding(
            "http://127.0.0.1:80",
            &Landing {
                target: &t,
                service_port: Some(80),
                pod: "api-1",
                pod_port: 8080,
            },
            &[],
        );
        assert_eq!(
            line,
            "http://127.0.0.1:80 → svc/api port 80 (pod api-1 port 8080)"
        );
    }

    #[test]
    fn a_fallen_back_forward_line_says_why() {
        let t = target("deploy/api");
        let line = forwarding(
            "http://127.0.0.1:54321",
            &Landing {
                target: &t,
                service_port: None,
                pod: "api-1",
                pod_port: 80,
            },
            &["local port 80 needs root".to_owned()],
        );
        assert_eq!(
            line,
            "http://127.0.0.1:54321 → deploy/api (pod api-1 port 80)  (local port 80 needs root)"
        );
    }

    #[test]
    fn several_notes_share_one_bracket() {
        let t = target("api");
        let line = forwarding(
            "http://127.0.0.1:54321",
            &Landing {
                target: &t,
                service_port: None,
                pod: "api-1",
                pod_port: 80,
            },
            &["local port 80 is in use".to_owned(), "open".to_owned()],
        );
        assert!(
            line.ends_with("  (local port 80 is in use; open)"),
            "{line}"
        );
    }

    #[test]
    fn a_listener_beyond_loopback_is_called_out() {
        assert_eq!(
            exposure(IpAddr::V4(Ipv4Addr::UNSPECIFIED)).as_deref(),
            Some("open to other machines: listening on every interface")
        );
        assert_eq!(
            exposure("10.0.0.5".parse().unwrap()).as_deref(),
            Some("open to other machines: listening on 10.0.0.5")
        );
        assert_eq!(exposure(IpAddr::V4(Ipv4Addr::LOCALHOST)), None);
        assert_eq!(exposure(IpAddr::V6(Ipv6Addr::LOCALHOST)), None);
    }

    #[test]
    fn a_refused_connection_says_nothing_is_listening() {
        let message = "error forwarding port 8080 to pod 1f2e, uid : failed to execute portforward \
                       in network namespace \"/var/run/netns/cni-1\": failed to connect to \
                       localhost:8080 inside namespace \"1f2e\", IPv4: dial tcp4 127.0.0.1:8080: \
                       connect: connection refused";
        let text = connection_failed(&target("api"), "api-1", 8080, message, Surface::Command);
        assert!(
            text.starts_with(
                "pod api-1 refused a connection on port 8080: nothing in it is listening there."
            ),
            "{text}"
        );
        assert!(
            text.contains("`eks port-forward api` with no port"),
            "{text}"
        );
        assert!(!text.contains("netns"), "{text}");
    }

    #[test]
    fn a_refused_connection_through_a_service_points_at_its_target_port() {
        let text = connection_failed(
            &target("svc/api"),
            "api-1",
            8080,
            "connection refused",
            Surface::Command,
        );
        assert!(text.contains("targetPort"), "{text}");
    }

    #[test]
    fn a_refused_connection_in_the_dashboard_points_at_the_pane_not_a_command() {
        let text = connection_failed(
            &target("api-1"),
            "api-1",
            8080,
            "connect: connection refused",
            Surface::Dashboard,
        );
        assert!(
            text.starts_with("pod api-1 refused a connection on port 8080"),
            "{text}"
        );
        assert!(text.contains("the pane lists"), "{text}");
        assert!(!text.contains("eks port-forward"), "{text}");
    }

    #[test]
    fn any_other_failure_is_passed_on_as_the_kubelet_said_it() {
        let text = connection_failed(&target("api"), "api-1", 8080, "timeout\n", Surface::Command);
        assert_eq!(text, "a connection to pod api-1 port 8080 failed: timeout");
    }

    #[test]
    fn a_pod_gone_before_its_forward_started_says_where_to_look() {
        let text = pod_gone("api-1", "payments");
        assert!(
            text.starts_with("pod api-1 is no longer in namespace payments"),
            "{text}"
        );
        assert!(text.contains("press r"), "{text}");
    }

    #[test]
    fn forbidden_names_the_verb_and_the_check() {
        let text = forbidden("prod", "payments");
        assert!(
            text.contains("`create` verb on `pods/portforward`"),
            "{text}"
        );
        assert!(
            text.contains("kubectl auth can-i create pods/portforward -n payments"),
            "{text}"
        );
    }

    #[test]
    fn a_connection_while_waiting_is_refused_with_a_reason() {
        assert_eq!(
            refused_while_waiting(&target("svc/api")),
            "refused a connection: no pod behind svc/api is ready yet."
        );
    }
}
