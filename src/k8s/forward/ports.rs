//! Which port in the pod a forward reaches.
//!
//! A person types a number, a name, or nothing at all, against a pod, a
//! deployment, or a service — and a service's ports are not the pod's. Its
//! `port` is what clients of the service dial, and its `targetPort` is where
//! the pod listens, which may itself be a name that only the pod's container
//! spec turns into a number. Every rule for that is here, over objects that
//! have already been fetched, so each one is a fixture.

use k8s_openapi::api::core::v1::{Pod, Service, ServicePort};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use crate::format::{self, Cell};
use crate::k8s::forward::spec::Remote;
use crate::theme::Palette;

/// Why no port could be settled on.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Error(String);

/// A port a container declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declared {
    pub container: String,
    pub name: Option<String>,
    pub number: u16,
    /// `TCP` when the spec leaves it out, as Kubernetes does.
    pub protocol: String,
}

impl Declared {
    fn is_tcp(&self) -> bool {
        self.protocol == "TCP"
    }
}

/// Every port the pod's containers declare, in spec order.
///
/// Sidecars started as init containers with `restartPolicy: Always` run for
/// the pod's whole life and serve like any other container, so their ports
/// count. Ordinary init containers have finished by the time anything could
/// connect, so theirs do not.
#[must_use]
pub fn declared(pod: &Pod) -> Vec<Declared> {
    let Some(spec) = pod.spec.as_ref() else {
        return Vec::new();
    };
    let sidecars = spec
        .init_containers
        .iter()
        .flatten()
        .filter(|container| container.restart_policy.as_deref() == Some("Always"));
    spec.containers
        .iter()
        .chain(sidecars)
        .flat_map(|container| {
            container.ports.iter().flatten().filter_map(|port| {
                Some(Declared {
                    container: container.name.clone(),
                    name: port.name.clone(),
                    number: u16::try_from(port.container_port).ok()?,
                    protocol: port.protocol.clone().unwrap_or_else(|| "TCP".to_owned()),
                })
            })
        })
        .collect()
}

/// The port in `pod` that `remote` means.
///
/// A number is taken as it is, declared or not: a container may listen on a
/// port its spec never mentions, and refusing those would make the forward
/// less useful than `kubectl`'s for no gain. The one number refused is one
/// the pod declares only for UDP, since port-forwarding carries TCP alone and
/// the forward would connect to nothing.
pub fn on_pod(pod: &Pod, remote: &Remote) -> Result<u16, Error> {
    let ports = declared(pod);
    let pod_name = pod_name(pod);
    match remote {
        Remote::Number(number) => {
            let same: Vec<&Declared> = ports.iter().filter(|p| p.number == *number).collect();
            if !same.is_empty() && same.iter().all(|port| !port.is_tcp()) {
                return Err(not_tcp(&format!("pod {pod_name}"), same[0]));
            }
            Ok(*number)
        }
        Remote::Name(name) => named_on_pod(&ports, pod_name, name),
    }
}

fn named_on_pod(ports: &[Declared], pod_name: &str, name: &str) -> Result<u16, Error> {
    let found: Vec<&Declared> = ports
        .iter()
        .filter(|port| port.name.as_deref() == Some(name))
        .collect();
    if let Some(tcp) = found.iter().find(|port| port.is_tcp()) {
        return Ok(tcp.number);
    }
    if let Some(other) = found.first() {
        return Err(not_tcp(&format!("pod {pod_name}"), other));
    }
    let names: Vec<String> = ports
        .iter()
        .filter(|port| port.is_tcp())
        .filter_map(|port| port.name.clone())
        .collect();
    Err(Error(match format::list(&names, "or") {
        Some(names) => format!(
            "pod {pod_name} has no port called {name:?}. Its named ports are {names}; \
             use one of those, or a number."
        ),
        None => format!(
            "pod {pod_name} has no port called {name:?}, and none of its ports has a name. \
             Use the port's number instead."
        ),
    }))
}

fn not_tcp(owner: &str, port: &Declared) -> Error {
    let name = port
        .name
        .as_deref()
        .map(|name| format!(" ({name})"))
        .unwrap_or_default();
    Error(format!(
        "port {number}{name} of {owner} is {protocol}, and port-forwarding carries TCP only.",
        number = port.number,
        protocol = port.protocol,
    ))
}

/// The service port `remote` names: by its number (the service's `port`,
/// which is what its clients dial) or by its name.
pub fn service_port<'a>(service: &'a Service, remote: &Remote) -> Result<&'a ServicePort, Error> {
    let ports = service_ports(service);
    let service_name = service.metadata.name.as_deref().unwrap_or_default();
    let found = ports.iter().copied().find(|port| match remote {
        Remote::Number(number) => i32::from(*number) == port.port,
        Remote::Name(name) => port.name.as_deref() == Some(name.as_str()),
    });
    match found {
        Some(port) if protocol(port) != "TCP" => Err(Error(format!(
            "port {number} of service {service_name} is {protocol}, and port-forwarding carries TCP only.",
            number = port.port,
            protocol = protocol(port),
        ))),
        Some(port) => Ok(port),
        None => {
            let listed: Vec<String> = ports.iter().map(|port| describe(port)).collect();
            Err(Error(match format::list(&listed, "and") {
                Some(listed) => format!(
                    "service {service_name} has no port {remote}. Its ports are {listed}.\n\
                     A service is forwarded by its own port numbers, not its pods'; \
                     to reach a pod's port directly, forward to the pod or its deployment."
                ),
                None => format!(
                    "service {service_name} declares no ports at all, so there is nothing to forward through it.\n\
                     Forward to one of its pods instead."
                ),
            }))
        }
    }
}

/// Where in `pod` the service's `port` lands: its `targetPort`, resolved
/// against the pod's containers when it is a name, and the same number as
/// `port` when it is left out.
pub fn target_on_pod(service: &str, port: &ServicePort, pod: &Pod) -> Result<u16, Error> {
    match &port.target_port {
        None => u16::try_from(port.port).map_err(|_| out_of_range(service, port.port)),
        Some(IntOrString::Int(number)) => {
            u16::try_from(*number).map_err(|_| out_of_range(service, *number))
        }
        Some(IntOrString::String(name)) => named_on_pod(&declared(pod), pod_name(pod), name)
            .map_err(|error| {
                Error(format!(
                    "service {service} sends port {port} to the pod's port {name:?}, but: {error}",
                    port = port.port,
                ))
            }),
    }
}

fn out_of_range(service: &str, number: i32) -> Error {
    Error(format!(
        "service {service} names port {number}, which is not a port number; the service's spec needs fixing."
    ))
}

fn service_ports(service: &Service) -> Vec<&ServicePort> {
    service
        .spec
        .as_ref()
        .and_then(|spec| spec.ports.as_ref())
        .map(|ports| ports.iter().collect())
        .unwrap_or_default()
}

fn protocol(port: &ServicePort) -> &str {
    port.protocol.as_deref().unwrap_or("TCP")
}

/// `80 (http)`, or `80` for a port with no name.
fn describe(port: &ServicePort) -> String {
    match port.name.as_deref() {
        Some(name) if !name.is_empty() => format!("{} ({name})", port.port),
        _ => port.port.to_string(),
    }
}

/// A port offered for someone to choose from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offered {
    pub name: Option<String>,
    pub number: u16,
    pub protocol: String,
    /// The container that declares it, or for a service where it lands.
    pub beside: String,
}

impl Offered {
    /// What choosing this one means, as a port spec would say it.
    #[must_use]
    pub fn remote(&self) -> Remote {
        Remote::Number(self.number)
    }
}

/// What to forward when no port was named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Default {
    /// There is exactly one candidate.
    One(Remote),
    /// Several: the person has to say which.
    Choose(Vec<Offered>),
}

/// The port to forward to `pod` when none was named: its one declared TCP
/// port, or the choice between several.
pub fn default_on_pod(pod: &Pod) -> Result<Default, Error> {
    let ports = declared(pod);
    let pod_name = pod_name(pod);
    let mut tcp: Vec<&Declared> = Vec::new();
    for port in ports.iter().filter(|port| port.is_tcp()) {
        // Two containers declaring the same number share one network
        // namespace, so it is one port to forward, not two.
        if !tcp.iter().any(|seen| seen.number == port.number) {
            tcp.push(port);
        }
    }
    match tcp.as_slice() {
        [] if ports.is_empty() => Err(Error(format!(
            "pod {pod_name} declares no ports, so eks cannot tell which one to forward.\n\
             Name the port the app listens on after the target, e.g. `eks port-forward {pod_name} 8080`."
        ))),
        [] => Err(Error(format!(
            "pod {pod_name} declares only {listed}, and port-forwarding carries TCP only.\n\
             If something in it listens on TCP too, name that port, e.g. `eks port-forward {pod_name} 8080`.",
            listed = format::list(
                &ports
                    .iter()
                    .map(|port| format!("{}/{}", port.number, port.protocol))
                    .collect::<Vec<_>>(),
                "and"
            )
            .unwrap_or_default(),
        ))),
        [only] => Ok(Default::One(Remote::Number(only.number))),
        several => Ok(Default::Choose(
            several
                .iter()
                .map(|port| Offered {
                    name: port.name.clone(),
                    number: port.number,
                    protocol: port.protocol.clone(),
                    beside: port.container.clone(),
                })
                .collect(),
        )),
    }
}

/// The port to forward through `service` when none was named: its one TCP
/// port, or the choice between several. `pod` is the one it will land on,
/// so the choice can say where each port goes.
pub fn default_on_service(service: &Service, pod: &Pod) -> Result<Default, Error> {
    let service_name = service.metadata.name.as_deref().unwrap_or_default();
    let tcp: Vec<&ServicePort> = service_ports(service)
        .into_iter()
        .filter(|port| protocol(port) == "TCP")
        .collect();
    match tcp.as_slice() {
        [] => Err(Error(format!(
            "service {service_name} has no TCP ports, and port-forwarding carries TCP only."
        ))),
        [only] => Ok(Default::One(number_of(only, service_name)?)),
        several => Ok(Default::Choose(
            several
                .iter()
                .filter_map(|port| {
                    Some(Offered {
                        name: port.name.clone().filter(|name| !name.is_empty()),
                        number: u16::try_from(port.port).ok()?,
                        protocol: protocol(port).to_owned(),
                        beside: landing(service_name, port, pod),
                    })
                })
                .collect(),
        )),
    }
}

fn number_of(port: &ServicePort, service: &str) -> Result<Remote, Error> {
    u16::try_from(port.port)
        .map(Remote::Number)
        .map_err(|_| out_of_range(service, port.port))
}

/// `pod port 8080`, or what went wrong working that out.
fn landing(service: &str, port: &ServicePort, pod: &Pod) -> String {
    match (target_on_pod(service, port, pod), &port.target_port) {
        (Ok(number), Some(IntOrString::String(name))) => format!("pod port {number} ({name})"),
        (Ok(number), _) => format!("pod port {number}"),
        (Err(_), Some(IntOrString::String(name))) => format!("{name:?}, not declared by the pod"),
        (Err(_), _) => "-".to_owned(),
    }
}

/// The choices as a numbered table, for the person to pick from.
///
/// `beside` heads the last column: `CONTAINER` for a pod's own ports,
/// `GOES TO` for a service's.
#[must_use]
pub fn table(offered: &[Offered], beside: &str) -> String {
    let rows: Vec<Vec<Cell>> = offered
        .iter()
        .enumerate()
        .map(|(index, port)| {
            vec![
                Cell::plain((index + 1).to_string()),
                Cell::plain(port.name.clone().unwrap_or_else(|| "-".to_owned())),
                Cell::plain(port.number.to_string()),
                Cell::plain(port.protocol.clone()),
                Cell::plain(port.beside.clone()),
            ]
        })
        .collect();
    // Plain for the same reason `pick`'s candidate table is: it is printed on
    // stderr, as part of a question or an error.
    format::table(
        &["#", "NAME", "PORT", "PROTOCOL", beside],
        &rows,
        Palette::Plain,
    )
}

/// Which offered port an answer picks: a port number from the table, a
/// port's name, or a row number.
///
/// A port number is looked for before a row number, so that `2` in a table
/// that offers port 2 means the port — it is the reading that is never
/// surprising once forwarded.
#[must_use]
pub fn answer(text: &str, offered: &[Offered]) -> Option<Remote> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(number) = text.parse::<usize>() {
        if let Some(port) = offered
            .iter()
            .find(|port| usize::from(port.number) == number)
        {
            return Some(port.remote());
        }
        return number
            .checked_sub(1)
            .and_then(|index| offered.get(index))
            .map(Offered::remote);
    }
    offered
        .iter()
        .find(|port| port.name.as_deref() == Some(text))
        .map(Offered::remote)
}

/// What to print when several ports are offered and nobody is at a terminal
/// to choose: the choices, and how to name one on the command line.
#[must_use]
pub fn unchosen(owner: &str, typed: &str, offered: &[Offered], beside: &str) -> String {
    let example = offered
        .iter()
        .find_map(|port| port.name.clone())
        .or_else(|| offered.first().map(|port| port.number.to_string()))
        .unwrap_or_default();
    format!(
        "{owner} has {count} ports, so eks needs to be told which to forward:\n\n{table}\n\n\
         Name one after the target, by name or number, e.g. `eks port-forward {typed} {example}`.",
        count = offered.len(),
        table = table(offered, beside),
    )
}

/// The question asked when several ports are offered and somebody is there
/// to answer it.
#[must_use]
pub fn question(owner: &str, offered: &[Offered], beside: &str) -> String {
    format!(
        "{owner} has {count} ports:\n\n{table}\n\nForward which one? (a name, a port, or a row number) ",
        count = offered.len(),
        table = table(offered, beside),
    )
}

/// An answer to [`question`] that matched none of the choices.
#[must_use]
pub fn unanswered(answer: &str, typed: &str) -> String {
    let answer = answer.trim();
    if answer.is_empty() {
        return format!(
            "no port chosen, so nothing was forwarded.\n\
             Name one after the target to skip the question, e.g. `eks port-forward {typed} 8080`."
        );
    }
    format!(
        "{answer:?} is not one of the ports listed, so nothing was forwarded.\n\
         Run it again and pick a name, port, or row number from the table."
    )
}

fn pod_name(pod: &Pod) -> &str {
    pod.metadata.name.as_deref().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use k8s_openapi::api::core::v1::{Container, ContainerPort, PodSpec, ServiceSpec};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    use super::*;

    /// `(container, name, number, protocol)` — a `None` protocol is TCP by
    /// default, as in a real spec.
    type Port<'a> = (&'a str, Option<&'a str>, i32, Option<&'a str>);

    fn pod(ports: &[Port<'_>]) -> Pod {
        let mut containers: Vec<Container> = Vec::new();
        for (container, name, number, protocol) in ports {
            let port = ContainerPort {
                name: name.map(ToOwned::to_owned),
                container_port: *number,
                protocol: protocol.map(ToOwned::to_owned),
                ..ContainerPort::default()
            };
            match containers.iter_mut().find(|c| c.name == *container) {
                Some(existing) => existing.ports.get_or_insert_with(Vec::new).push(port),
                None => containers.push(Container {
                    name: (*container).to_owned(),
                    ports: Some(vec![port]),
                    ..Container::default()
                }),
            }
        }
        Pod {
            metadata: ObjectMeta {
                name: Some("api-1".to_owned()),
                ..ObjectMeta::default()
            },
            spec: Some(PodSpec {
                containers,
                ..PodSpec::default()
            }),
            status: None,
        }
    }

    fn bare_pod() -> Pod {
        Pod {
            metadata: ObjectMeta {
                name: Some("api-1".to_owned()),
                ..ObjectMeta::default()
            },
            spec: Some(PodSpec {
                containers: vec![Container {
                    name: "app".to_owned(),
                    ..Container::default()
                }],
                ..PodSpec::default()
            }),
            status: None,
        }
    }

    /// `(name, port, targetPort, protocol)`.
    type Exposed<'a> = (Option<&'a str>, i32, Option<IntOrString>, Option<&'a str>);

    fn service(ports: &[Exposed<'_>]) -> Service {
        Service {
            metadata: ObjectMeta {
                name: Some("api".to_owned()),
                ..ObjectMeta::default()
            },
            spec: Some(ServiceSpec {
                ports: Some(
                    ports
                        .iter()
                        .map(|(name, port, target, protocol)| ServicePort {
                            name: name.map(ToOwned::to_owned),
                            port: *port,
                            target_port: target.clone(),
                            protocol: protocol.map(ToOwned::to_owned),
                            ..ServicePort::default()
                        })
                        .collect(),
                ),
                ..ServiceSpec::default()
            }),
            status: None,
        }
    }

    fn name(text: &str) -> Remote {
        Remote::Name(text.to_owned())
    }

    #[test]
    fn a_number_is_used_even_when_the_pod_does_not_declare_it() {
        assert_eq!(on_pod(&bare_pod(), &Remote::Number(9090)), Ok(9090));
    }

    #[test]
    fn a_named_port_resolves_against_the_container_spec() {
        let pod = pod(&[
            ("app", Some("http"), 8080, None),
            ("app", Some("admin"), 9000, None),
        ]);
        assert_eq!(on_pod(&pod, &name("admin")), Ok(9000));
    }

    #[test]
    fn a_named_port_on_a_second_container_is_found_too() {
        let pod = pod(&[
            ("app", Some("http"), 8080, None),
            ("proxy", Some("envoy"), 15000, None),
        ]);
        assert_eq!(on_pod(&pod, &name("envoy")), Ok(15000));
    }

    #[test]
    fn a_native_sidecar_s_ports_count_but_a_finished_init_container_s_do_not() {
        let mut pod = bare_pod();
        let spec = pod.spec.as_mut().unwrap();
        spec.init_containers = Some(vec![
            Container {
                name: "proxy".to_owned(),
                restart_policy: Some("Always".to_owned()),
                ports: Some(vec![ContainerPort {
                    name: Some("proxy".to_owned()),
                    container_port: 15001,
                    ..ContainerPort::default()
                }]),
                ..Container::default()
            },
            Container {
                name: "migrate".to_owned(),
                ports: Some(vec![ContainerPort {
                    name: Some("migrate".to_owned()),
                    container_port: 5000,
                    ..ContainerPort::default()
                }]),
                ..Container::default()
            },
        ]);
        assert_eq!(on_pod(&pod, &name("proxy")), Ok(15001));
        assert!(on_pod(&pod, &name("migrate")).is_err());
    }

    #[test]
    fn an_unknown_name_lists_the_names_there_are() {
        let pod = pod(&[
            ("app", Some("http"), 8080, None),
            ("app", Some("metrics"), 9090, None),
        ]);
        let text = on_pod(&pod, &name("grpc")).unwrap_err().to_string();
        assert!(text.contains("no port called \"grpc\""), "{text}");
        assert!(text.contains("http or metrics"), "{text}");
    }

    #[test]
    fn an_unknown_name_on_a_pod_with_no_named_ports_says_to_use_a_number() {
        let pod = pod(&[("app", None, 8080, None)]);
        let text = on_pod(&pod, &name("http")).unwrap_err().to_string();
        assert!(text.contains("none of its ports has a name"), "{text}");
    }

    #[test]
    fn a_udp_only_port_is_refused_by_name_and_by_number() {
        let pod = pod(&[("dns", Some("dns"), 53, Some("UDP"))]);
        for remote in [name("dns"), Remote::Number(53)] {
            let text = on_pod(&pod, &remote).unwrap_err().to_string();
            assert!(text.contains("is UDP"), "{text}");
            assert!(text.contains("TCP only"), "{text}");
        }
    }

    #[test]
    fn a_number_declared_for_both_udp_and_tcp_is_forwarded() {
        let pod = pod(&[
            ("dns", Some("dns"), 53, Some("UDP")),
            ("dns", Some("dns-tcp"), 53, Some("TCP")),
        ]);
        assert_eq!(on_pod(&pod, &Remote::Number(53)), Ok(53));
    }

    #[test]
    fn one_declared_port_is_used_when_none_is_named() {
        let pod = pod(&[("app", Some("http"), 8080, None)]);
        assert_eq!(default_on_pod(&pod), Ok(Default::One(Remote::Number(8080))));
    }

    #[test]
    fn several_declared_ports_are_offered_with_their_containers() {
        let pod = pod(&[
            ("app", Some("http"), 8080, None),
            ("app", None, 9090, None),
            ("proxy", Some("admin"), 15000, None),
        ]);
        let Ok(Default::Choose(offered)) = default_on_pod(&pod) else {
            panic!("expected a choice");
        };
        assert_eq!(offered.len(), 3);
        assert_eq!(offered[2].beside, "proxy");
        assert_eq!(offered[1].name, None);
    }

    #[test]
    fn udp_ports_are_left_out_of_the_choice() {
        let pod = pod(&[
            ("dns", Some("dns"), 53, Some("UDP")),
            ("dns", Some("metrics"), 9153, None),
        ]);
        assert_eq!(default_on_pod(&pod), Ok(Default::One(Remote::Number(9153))));
    }

    #[test]
    fn the_same_number_in_two_containers_is_offered_once() {
        let pod = pod(&[
            ("a", Some("http"), 8080, None),
            ("b", Some("http"), 8080, None),
        ]);
        assert_eq!(default_on_pod(&pod), Ok(Default::One(Remote::Number(8080))));
    }

    #[test]
    fn a_pod_with_no_ports_asks_for_one_with_an_example() {
        let text = default_on_pod(&bare_pod()).unwrap_err().to_string();
        assert!(text.contains("declares no ports"), "{text}");
        assert!(text.contains("`eks port-forward api-1 8080`"), "{text}");
    }

    #[test]
    fn a_pod_with_only_udp_ports_says_so() {
        let pod = pod(&[("dns", Some("dns"), 53, Some("UDP"))]);
        let text = default_on_pod(&pod).unwrap_err().to_string();
        assert!(text.contains("declares only 53/UDP"), "{text}");
    }

    #[test]
    fn a_service_port_maps_to_its_numeric_target_port() {
        let svc = service(&[(Some("http"), 80, Some(IntOrString::Int(8080)), None)]);
        let port = service_port(&svc, &Remote::Number(80)).unwrap();
        assert_eq!(target_on_pod("api", port, &bare_pod()), Ok(8080));
    }

    #[test]
    fn a_service_port_without_a_target_port_lands_on_the_same_number() {
        let svc = service(&[(None, 8080, None, None)]);
        let port = service_port(&svc, &Remote::Number(8080)).unwrap();
        assert_eq!(target_on_pod("api", port, &bare_pod()), Ok(8080));
    }

    #[test]
    fn a_named_target_port_resolves_against_the_pod_it_lands_on() {
        let svc = service(&[(
            Some("web"),
            80,
            Some(IntOrString::String("http".to_owned())),
            None,
        )]);
        let pod = pod(&[("app", Some("http"), 3000, None)]);
        let port = service_port(&svc, &name("web")).unwrap();
        assert_eq!(target_on_pod("api", port, &pod), Ok(3000));
    }

    #[test]
    fn a_named_target_port_the_pod_lacks_names_both_ends() {
        let svc = service(&[(
            Some("web"),
            80,
            Some(IntOrString::String("http".to_owned())),
            None,
        )]);
        let port = service_port(&svc, &Remote::Number(80)).unwrap();
        let text = target_on_pod("api", port, &bare_pod())
            .unwrap_err()
            .to_string();
        assert!(
            text.starts_with("service api sends port 80 to the pod's port \"http\""),
            "{text}"
        );
    }

    #[test]
    fn a_service_port_it_does_not_have_lists_the_ones_it_does() {
        let svc = service(&[(Some("http"), 80, None, None), (None, 443, None, None)]);
        let text = service_port(&svc, &Remote::Number(8080))
            .unwrap_err()
            .to_string();
        assert!(text.contains("has no port 8080"), "{text}");
        assert!(text.contains("80 (http) and 443"), "{text}");
        assert!(text.contains("not its pods'"), "{text}");
    }

    #[test]
    fn a_udp_service_port_is_refused() {
        let svc = service(&[(Some("dns"), 53, None, Some("UDP"))]);
        let text = service_port(&svc, &name("dns")).unwrap_err().to_string();
        assert!(text.contains("is UDP"), "{text}");
    }

    #[test]
    fn a_service_with_one_tcp_port_needs_none_named() {
        let svc = service(&[
            (Some("dns"), 53, None, Some("UDP")),
            (Some("dns-tcp"), 53, None, None),
        ]);
        assert_eq!(
            default_on_service(&svc, &bare_pod()),
            Ok(Default::One(Remote::Number(53)))
        );
    }

    #[test]
    fn a_service_s_several_ports_are_offered_with_where_each_lands() {
        let svc = service(&[
            (
                Some("web"),
                80,
                Some(IntOrString::String("http".to_owned())),
                None,
            ),
            (Some("metrics"), 9090, Some(IntOrString::Int(9100)), None),
            (
                Some("grpc"),
                50051,
                Some(IntOrString::String("grpc".to_owned())),
                None,
            ),
        ]);
        let pod = pod(&[("app", Some("http"), 3000, None)]);
        let Ok(Default::Choose(offered)) = default_on_service(&svc, &pod) else {
            panic!("expected a choice");
        };
        assert_eq!(offered[0].beside, "pod port 3000 (http)");
        assert_eq!(offered[1].beside, "pod port 9100");
        assert_eq!(offered[2].beside, "\"grpc\", not declared by the pod");
    }

    fn offered() -> Vec<Offered> {
        vec![
            Offered {
                name: Some("http".to_owned()),
                number: 8080,
                protocol: "TCP".to_owned(),
                beside: "app".to_owned(),
            },
            Offered {
                name: None,
                number: 2,
                protocol: "TCP".to_owned(),
                beside: "app".to_owned(),
            },
            Offered {
                name: Some("admin".to_owned()),
                number: 9000,
                protocol: "TCP".to_owned(),
                beside: "proxy".to_owned(),
            },
        ]
    }

    #[test]
    fn an_answer_can_be_a_name_a_port_or_a_row() {
        let offered = offered();
        assert_eq!(answer("admin\n", &offered), Some(Remote::Number(9000)));
        assert_eq!(answer("8080", &offered), Some(Remote::Number(8080)));
        assert_eq!(answer("3", &offered), Some(Remote::Number(9000)));
    }

    #[test]
    fn a_number_that_is_both_a_port_and_a_row_means_the_port() {
        assert_eq!(answer("2", &offered()), Some(Remote::Number(2)));
    }

    #[test]
    fn an_empty_or_unknown_answer_picks_nothing() {
        let offered = offered();
        for text in ["", "  \n", "0", "4", "grpc"] {
            assert_eq!(answer(text, &offered), None, "{text:?}");
        }
    }

    #[test]
    fn the_choice_is_a_numbered_table_with_name_port_protocol_and_container() {
        let text = table(&offered(), "CONTAINER");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0].split_whitespace().collect::<Vec<_>>(),
            ["#", "NAME", "PORT", "PROTOCOL", "CONTAINER"]
        );
        assert_eq!(
            lines[2].split_whitespace().collect::<Vec<_>>(),
            ["2", "-", "2", "TCP", "app"]
        );
    }

    #[test]
    fn without_a_terminal_the_choice_says_how_to_name_one() {
        let text = unchosen("pod api-1", "api", &offered(), "CONTAINER");
        assert!(text.starts_with("pod api-1 has 3 ports"), "{text}");
        assert!(text.contains("`eks port-forward api http`"), "{text}");
    }

    #[test]
    fn an_empty_answer_says_how_to_skip_the_question() {
        assert!(unanswered("\n", "svc/api").contains("`eks port-forward svc/api 8080`"));
        assert!(unanswered("grpc", "svc/api").contains("\"grpc\" is not one of the ports"));
    }
}
