//! What `eks port-forward` was asked for, read off the command line.
//!
//! Three things are typed: a target (`api`, `svc/api`, `deploy/api`), zero or
//! more port specs (`8080`, `9000:80`, `:http`), and `--address`. Each is
//! parsed here, before anything connects, so a typo is a sentence naming the
//! text that is wrong rather than a request the API server refuses with a
//! message about something else.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Why something typed could not be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Error(String);

/// What kind of thing a target names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Pod,
    Service,
    Deployment,
}

impl Kind {
    /// The prefix this kind is written with, as `eks` prints it back.
    #[must_use]
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Pod => "pod",
            Self::Service => "svc",
            Self::Deployment => "deploy",
        }
    }

    /// The kind in a sentence: "service", "deployment".
    #[must_use]
    pub fn noun(self) -> &'static str {
        match self {
            Self::Pod => "pod",
            Self::Service => "service",
            Self::Deployment => "deployment",
        }
    }
}

/// The thing to forward to, as typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub kind: Kind,
    /// A full name, or the start of exactly one name — the same rule
    /// `eks exec` uses for pods, applied to every kind.
    pub name: String,
}

impl Target {
    /// Read `svc/api`, `deploy/api`, `pod/api`, or a bare `api` (a pod).
    ///
    /// The prefixes `kubectl` accepts are accepted too — `service/`,
    /// `services/`, `deployment/`, `deployments/`, `po/`, `pods/` — since that
    /// is what a person who has used it once will type.
    pub fn parse(input: &str) -> Result<Self, Error> {
        let (kind, name) = match input.split_once('/') {
            None => (Kind::Pod, input),
            Some((prefix, name)) => {
                let kind = match prefix.to_ascii_lowercase().as_str() {
                    "pod" | "pods" | "po" => Kind::Pod,
                    "svc" | "service" | "services" => Kind::Service,
                    "deploy" | "deployment" | "deployments" | "deployment.apps"
                    | "deployments.apps" => Kind::Deployment,
                    _ => return Err(Error(unknown_kind(prefix))),
                };
                (kind, name)
            }
        };
        if name.is_empty() {
            return Err(Error(format!(
                "{input:?} names no {noun}: put its name after the slash, e.g. `{prefix}/api`.",
                noun = kind.noun(),
                prefix = kind.prefix(),
            )));
        }
        if name.contains('/') {
            return Err(Error(format!(
                "{input:?} has more than one slash; a target is a name, or a kind and a name, \
                 e.g. `svc/api`."
            )));
        }
        Ok(Self {
            kind,
            name: name.to_owned(),
        })
    }
}

impl fmt::Display for Target {
    /// `svc/api`, `deploy/api`, or a bare pod name — the way it would be typed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            Kind::Pod => f.write_str(&self.name),
            kind => write!(f, "{}/{}", kind.prefix(), self.name),
        }
    }
}

fn unknown_kind(prefix: &str) -> String {
    let lower = prefix.to_ascii_lowercase();
    // The workload kinds whose pods have stable names are worth a specific
    // pointer: their pods are the thing to name.
    let instead = match lower.as_str() {
        "sts" | "statefulset" | "statefulsets" => {
            "A StatefulSet's pods keep their names, so forward to one of them, e.g. `db-0`, or to the service in front of it."
        }
        "ds" | "daemonset" | "daemonsets" => {
            "Forward to one of the DaemonSet's pods by name, or to a service in front of it."
        }
        "rs" | "replicaset" | "replicasets" => "Forward to the deployment that owns it instead.",
        _ => "Name a pod, or prefix a service with `svc/` or a deployment with `deploy/`.",
    };
    format!(
        "eks port-forward reaches pods, services (`svc/`), and deployments (`deploy/`); \
         {prefix:?} is not one of those.\n{instead}"
    )
}

/// The remote end of one forward: a port number, or a port's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remote {
    Number(u16),
    /// A name declared on a container (for a pod or deployment) or on the
    /// service (for `svc/`).
    Name(String),
}

impl fmt::Display for Remote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(number) => write!(f, "{number}"),
            Self::Name(name) => f.write_str(name),
        }
    }
}

/// Which local port to listen on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Local {
    /// Nothing given: the remote port's own number when it is free here, and
    /// any free port when it is not.
    Same,
    /// `:80` or `0:80` — any free port, chosen by the OS.
    Any,
    /// `8080:80` — exactly this one, or fail. A port somebody typed is a port
    /// they are going to point something at, so a silent substitute would
    /// be worse than an error.
    Port(u16),
}

/// One `[LOCAL:]REMOTE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub local: Local,
    pub remote: Remote,
}

impl Spec {
    /// Read `80`, `8080:80`, `:80`, `http`, or `8080:http`.
    pub fn parse(input: &str) -> Result<Self, Error> {
        let (local, remote) = match input.split_once(':') {
            None => (Local::Same, input),
            Some(("" | "0", remote)) => (Local::Any, remote),
            Some((local, remote)) => {
                let number = local_number(local).ok_or_else(|| {
                    Error(format!(
                        "{local:?} in {input:?} is not a local port: use a number from 1 to 65535, \
                         or leave it out (`:{remote}`) to let eks pick a free one."
                    ))
                })?;
                (Local::Port(number), remote)
            }
        };
        Ok(Self {
            local,
            remote: remote_port(remote, input)?,
        })
    }
}

fn local_number(text: &str) -> Option<u16> {
    text.parse::<u16>().ok().filter(|number| *number > 0)
}

/// A remote port: a number from 1 to 65535, or a name as Kubernetes allows
/// one (`IANA_SVC_NAME`: up to fifteen lowercase letters, digits, and
/// hyphens, with at least one letter).
fn remote_port(text: &str, whole: &str) -> Result<Remote, Error> {
    if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) {
        return text
            .parse::<u16>()
            .ok()
            .filter(|number| *number > 0)
            .map(Remote::Number)
            .ok_or_else(|| {
                Error(format!(
                    "{text:?} in {whole:?} is not a port: ports run from 1 to 65535."
                ))
            });
    }
    let is_name = !text.is_empty()
        && text.len() <= 15
        && text
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && text.bytes().any(|byte| byte.is_ascii_lowercase())
        && !text.starts_with('-')
        && !text.ends_with('-');
    if is_name {
        return Ok(Remote::Name(text.to_owned()));
    }
    if text.is_empty() {
        return Err(Error(format!(
            "{whole:?} has no remote port after the colon: write it as `LOCAL:REMOTE`, e.g. `8080:80`."
        )));
    }
    Err(Error(format!(
        "{text:?} in {whole:?} is neither a port number nor a port name: a name is up to 15 \
         lowercase letters, digits, and hyphens, e.g. `http` or `grpc-web`."
    )))
}

/// One address to listen on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Listen {
    pub address: IpAddr,
    /// Whether failing to bind here fails the forward. Only `localhost`'s
    /// IPv6 half is optional: plenty of machines and containers have no
    /// `::1`, and a person who typed `localhost` asked for whatever loopback
    /// there is, not for both or nothing.
    pub required: bool,
}

/// The addresses `--address` names, in order and without repeats.
///
/// Nothing given is `127.0.0.1` alone, so a forward is never reachable from
/// anywhere but this machine unless someone asked for that. `localhost` is
/// `127.0.0.1` and, where there is one, `::1`.
pub fn addresses(given: &[String]) -> Result<Vec<Listen>, Error> {
    let mut out: Vec<Listen> = Vec::new();
    let mut push = |listen: Listen| {
        if let Some(existing) = out.iter_mut().find(|l| l.address == listen.address) {
            existing.required |= listen.required;
        } else {
            out.push(listen);
        }
    };

    if given.is_empty() {
        push(Listen {
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            required: true,
        });
    }
    for text in given.iter().map(|text| text.trim()) {
        if text.eq_ignore_ascii_case("localhost") {
            push(Listen {
                address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                required: true,
            });
            push(Listen {
                address: IpAddr::V6(Ipv6Addr::LOCALHOST),
                required: false,
            });
            continue;
        }
        let address = text
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .map_err(|_| {
                Error(format!(
                    "{text:?} is not an address to listen on: give an IP address such as \
                     `127.0.0.1`, `0.0.0.0`, or `::1`, or `localhost`."
                ))
            })?;
        push(Listen {
            address,
            required: true,
        });
    }
    Ok(out)
}

/// The URL a person clicks for a forward on `address:port`.
///
/// An unspecified address (`0.0.0.0`, `::`) is not something a browser can
/// open, so it is written as loopback: that is the one address on which a
/// listener bound everywhere is certainly reachable from this machine.
#[must_use]
pub fn url(address: IpAddr, port: u16) -> String {
    let shown = match address {
        IpAddr::V4(v4) if v4.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(v6) if v6.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        other => other,
    };
    match shown {
        IpAddr::V4(v4) => format!("http://{v4}:{port}"),
        IpAddr::V6(v6) => format!("http://[{v6}]:{port}"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn target(input: &str) -> Target {
        Target::parse(input).unwrap()
    }

    fn spec(input: &str) -> Spec {
        Spec::parse(input).unwrap()
    }

    fn refusal(result: Result<impl fmt::Debug, Error>) -> String {
        result.unwrap_err().to_string()
    }

    #[test]
    fn a_bare_name_is_a_pod() {
        assert_eq!(
            target("api"),
            Target {
                kind: Kind::Pod,
                name: "api".to_owned()
            }
        );
    }

    #[test]
    fn kubectl_spellings_of_each_kind_are_accepted() {
        for (input, kind) in [
            ("pod/api", Kind::Pod),
            ("po/api", Kind::Pod),
            ("pods/api", Kind::Pod),
            ("svc/api", Kind::Service),
            ("service/api", Kind::Service),
            ("Services/api", Kind::Service),
            ("deploy/api", Kind::Deployment),
            ("deployment/api", Kind::Deployment),
            ("deployments.apps/api", Kind::Deployment),
        ] {
            assert_eq!(target(input).kind, kind, "{input}");
            assert_eq!(target(input).name, "api", "{input}");
        }
    }

    #[test]
    fn a_target_prints_back_the_way_it_would_be_typed() {
        assert_eq!(target("service/api").to_string(), "svc/api");
        assert_eq!(target("deployment/api").to_string(), "deploy/api");
        assert_eq!(target("pod/api").to_string(), "api");
    }

    #[test]
    fn a_statefulset_is_refused_with_a_pointer_at_its_pods() {
        let text = refusal(Target::parse("sts/db"));
        assert!(text.contains("\"sts\" is not one of those"), "{text}");
        assert!(text.contains("`db-0`"), "{text}");
    }

    #[test]
    fn an_unknown_kind_lists_the_ones_there_are() {
        let text = refusal(Target::parse("cronjob/x"));
        assert!(text.contains("`svc/`"), "{text}");
        assert!(text.contains("`deploy/`"), "{text}");
    }

    #[test]
    fn a_kind_with_no_name_after_the_slash_is_refused() {
        let text = refusal(Target::parse("svc/"));
        assert!(text.contains("names no service"), "{text}");
        assert!(text.contains("`svc/api`"), "{text}");
    }

    #[test]
    fn a_second_slash_is_refused() {
        assert!(refusal(Target::parse("svc/a/b")).contains("more than one slash"));
    }

    #[test]
    fn a_bare_port_listens_on_the_same_number_where_it_can() {
        assert_eq!(
            spec("80"),
            Spec {
                local: Local::Same,
                remote: Remote::Number(80)
            }
        );
    }

    #[test]
    fn local_and_remote_are_separated_by_a_colon() {
        assert_eq!(
            spec("8080:80"),
            Spec {
                local: Local::Port(8080),
                remote: Remote::Number(80)
            }
        );
    }

    #[test]
    fn an_empty_or_zero_local_port_means_any_free_one() {
        assert_eq!(spec(":80").local, Local::Any);
        assert_eq!(spec("0:80").local, Local::Any);
    }

    #[test]
    fn a_remote_port_can_be_a_name() {
        assert_eq!(spec("http").remote, Remote::Name("http".to_owned()));
        assert_eq!(
            spec("9000:grpc-web"),
            Spec {
                local: Local::Port(9000),
                remote: Remote::Name("grpc-web".to_owned())
            }
        );
    }

    #[test]
    fn port_zero_and_ports_past_65535_are_refused() {
        assert!(refusal(Spec::parse("0")).contains("from 1 to 65535"));
        assert!(refusal(Spec::parse("70000")).contains("from 1 to 65535"));
        assert!(refusal(Spec::parse("70000:80")).contains("not a local port"));
    }

    #[test]
    fn a_missing_remote_port_shows_the_shape_to_write() {
        assert!(refusal(Spec::parse("8080:")).contains("`LOCAL:REMOTE`"));
    }

    #[test]
    fn something_that_is_neither_a_number_nor_a_name_says_what_a_name_may_be() {
        for input in [
            "HTTP",
            "my_port",
            "a-very-long-port-name",
            "-http",
            "8080:http:x",
        ] {
            let text = refusal(Spec::parse(input));
            assert!(
                text.contains("neither a port number nor a port name"),
                "{input}: {text}"
            );
        }
    }

    #[test]
    fn a_local_port_that_is_not_a_number_is_refused() {
        let text = refusal(Spec::parse("web:80"));
        assert!(
            text.contains("\"web\" in \"web:80\" is not a local port"),
            "{text}"
        );
    }

    #[test]
    fn no_address_listens_on_ipv4_loopback_only() {
        assert_eq!(
            addresses(&[]).unwrap(),
            vec![Listen {
                address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                required: true
            }]
        );
    }

    #[test]
    fn localhost_is_both_loopbacks_with_ipv6_optional() {
        let listens = addresses(&["localhost".to_owned()]).unwrap();
        assert_eq!(listens.len(), 2);
        assert!(listens[0].required);
        assert_eq!(listens[1].address, IpAddr::V6(Ipv6Addr::LOCALHOST));
        assert!(!listens[1].required);
    }

    #[test]
    fn an_address_named_twice_is_listened_on_once_and_required_if_either_says_so() {
        let listens = addresses(&["localhost".to_owned(), "::1".to_owned()]).unwrap();
        assert_eq!(listens.len(), 2);
        assert!(listens[1].required);
    }

    #[test]
    fn bracketed_ipv6_is_accepted() {
        let listens = addresses(&["[::1]".to_owned()]).unwrap();
        assert_eq!(listens[0].address, IpAddr::V6(Ipv6Addr::LOCALHOST));
    }

    #[test]
    fn a_hostname_is_refused_with_examples() {
        let text = refusal(addresses(&["example.com".to_owned()]));
        assert!(text.contains("`0.0.0.0`"), "{text}");
    }

    #[test]
    fn urls_are_clickable_for_every_kind_of_address() {
        assert_eq!(
            url(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080),
            "http://127.0.0.1:8080"
        );
        assert_eq!(url(IpAddr::V6(Ipv6Addr::LOCALHOST), 80), "http://[::1]:80");
        assert_eq!(
            url(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 80),
            "http://127.0.0.1:80"
        );
        assert_eq!(
            url(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 80),
            "http://[::1]:80"
        );
    }
}
