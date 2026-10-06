//! `eks port-forward` — a pod, a service, or a deployment, on localhost.
//!
//! What it decides is in [`crate::k8s::forward`]: which pod, which port, which
//! local port, and what to do when the pod goes away. This module is the I/O
//! around those decisions, in four steps:
//!
//! 1. **Resolve.** Find the target by name or prefix, the pod behind it, and
//!    the port in that pod each spec means, asking which when several could
//!    be meant and there is somebody to ask.
//! 2. **Listen.** Bind each forward's local port on every `--address` and
//!    print one clickable line per forward.
//! 3. **Carry.** Every accepted connection gets its own port-forward stream to
//!    the pod — one WebSocket each — so a browser's six parallel requests are
//!    six independent streams, and one that fails takes no other down.
//! 4. **Watch.** A loop looks at the pod every few seconds, and at once when
//!    a connection finds it gone. A `svc/` or `deploy/` forward moves to
//!    another ready pod; a pod named directly ends the command with a
//!    sentence naming what happened.
//!
//! Ctrl-C drops the whole future (see `main`), which closes every listener
//! and aborts every connection's task.
//!
//! The dashboard's `f` runs steps 2 to 4 for one port of one pod through
//! [`spawn_dashboard`]. Everything the command would print on stderr while it
//! runs goes through a `Report` instead, which for the dashboard is a
//! channel its forwards strip reads.

use std::fmt::Debug;
use std::io::IsTerminal as _;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use futures_util::future;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::authorization::v1::{
    ResourceAttributes, SelfSubjectAccessReview, SelfSubjectAccessReviewSpec,
};
use k8s_openapi::api::core::v1::{Pod, Service};
use k8s_openapi::jiff::Timestamp;
use kube::api::{Api, ListParams, PostParams};
use kube::{Client, ResourceExt};
use serde::de::DeserializeOwned;
use tokio::io::AsyncBufReadExt as _;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, watch};
use tokio::task::JoinSet;

use crate::aws::LoginMode;
use crate::cluster::ClusterView;
use crate::commands::{
    FetchError, StreamHandle, credentials, exec, nodes::target_cluster, pods::selectors_for,
};
use crate::k8s::auth::Store;
use crate::k8s::forward::choose::{self, Found, Move, Verdict};
use crate::k8s::forward::ports::{self, Default as DefaultPort};
use crate::k8s::forward::spec::{self, Kind, Listen, Local, Remote, Spec, Target};
use crate::k8s::forward::{self, Bind, Event, Landing, PodPort, Surface};
use crate::k8s::page::{self, Budget};
use crate::k8s::pods::{self as k8s_pods, Scope, Selectors};
use crate::k8s::{self, Failure};
use crate::kubeconfig::KubeConfig;
use crate::progress::Progress;

/// How often the pod behind a forward is looked at when nothing has gone
/// wrong. Often enough that a rollout is noticed before somebody's next
/// click, rarely enough that a forward left open all day is a few thousand
/// small requests rather than a stream of them.
const CHECK_EVERY: Duration = Duration::from_secs(3);

/// How long a connection that found its pod gone waits for the watch loop to
/// find another before giving up on it.
const RESETTLE: Duration = Duration::from_secs(10);

/// What `eks port-forward` was asked to do, as it came off the command line.
#[derive(Debug, Clone, Default)]
pub struct Request<'a> {
    /// `api`, `svc/api`, `deploy/api`.
    pub target: &'a str,
    /// `[LOCAL:]REMOTE`, each unparsed. Empty asks for the one declared port.
    pub ports: &'a [String],
    /// `--address`, unparsed.
    pub addresses: &'a [String],
    /// `--namespace`, or the config file's. Without either, the context's.
    pub namespace: Option<&'a str>,
    /// `-l`, unparsed. Narrows the pods a bare pod prefix is matched against,
    /// as for `eks exec`; a `svc/` or `deploy/` brings its own selector.
    pub label_selector: Option<&'a str>,
    /// `--field-selector`, unparsed, likewise.
    pub field_selector: Option<&'a str>,
    /// `--timeout`: each request, and never the forward itself.
    pub budget: Budget,
    pub login: LoginMode,
}

/// Forward until Ctrl-C, or until a pod named directly goes away.
///
/// Every failure before the first line is printed is an `Err` that `main`
/// prints the usual way, and so is a pod that was named directly and has
/// gone: that is the end of the command, and the reason is its last line.
pub async fn run(
    config: &KubeConfig,
    paths: &[PathBuf],
    context: Option<&str>,
    request: Request<'_>,
) -> Result<()> {
    // Everything typed is read before anything connects.
    let target = Target::parse(request.target)?;
    let mut specs = request
        .ports
        .iter()
        .map(|text| Spec::parse(text))
        .collect::<Result<Vec<_>, _>>()?;
    let listens = spec::addresses(request.addresses)?;
    let selectors = selectors_for(request.label_selector, request.field_selector)?;

    let cluster = target_cluster(config, context)?;
    let label = cluster.label();
    let namespace = request
        .namespace
        .map_or_else(|| cluster.namespace.clone(), ToOwned::to_owned);
    let budget = request.budget;
    let client =
        credentials::connect(paths, &cluster, budget, request.login, &Progress::none()).await?;

    if allowed(&client, &namespace, budget).await == Some(false) {
        bail!(forward::forbidden(&label, &namespace));
    }

    let resolved = resolve(&client, &cluster, &namespace, &target, &selectors, budget).await?;
    if specs.is_empty() {
        specs.push(default_spec(&target, &resolved).await?);
    }
    let pod_name = resolved.pod.name_any();
    let landed = specs
        .iter()
        .map(|spec| land(resolved.service.as_ref(), &spec.remote, &resolved.pod))
        .collect::<Result<Vec<_>, _>>()?;

    let mut forwards = Vec::with_capacity(specs.len());
    for (spec, port) in specs.iter().zip(&landed) {
        let listening = listen(&listens, spec, port.local_default()).await?;
        for listener in &listening.listeners {
            let address = listener.local_addr()?;
            let notes: Vec<String> = listening
                .fallback
                .iter()
                .cloned()
                .chain(forward::exposure(address.ip()))
                .collect();
            println!(
                "{}",
                forward::forwarding(
                    &spec::url(address.ip(), address.port()),
                    &Landing {
                        target: &target,
                        service_port: port.service,
                        pod: &pod_name,
                        pod_port: port.pod,
                    },
                    &notes,
                )
            );
        }
        forwards.push(listening.listeners);
    }
    eprintln!("{}", forward::STOP_HINT);

    let (destination, _) = watch::channel(Some(Destination {
        pod: pod_name,
        ports: landed.iter().map(|port| port.pod).collect(),
    }));
    let shared = Arc::new(Shared {
        api: Api::namespaced(client.clone(), &namespace),
        target: target.clone(),
        label,
        namespace: namespace.clone(),
        budget,
        destination,
        recheck: Notify::new(),
        report: Report::Stderr,
    });

    let watching = match resolved.selector {
        Some(selector) => Watching::Set {
            client,
            selector,
            service: resolved.service.map(Box::new),
            remotes: specs.into_iter().map(|spec| spec.remote).collect(),
        },
        None => Watching::Pod {
            last: Box::new(resolved.pod),
        },
    };

    let accepting = future::join_all(forwards.into_iter().enumerate().flat_map(
        |(index, listeners)| {
            let shared = Arc::clone(&shared);
            listeners
                .into_iter()
                .map(move |listener| accept(Arc::clone(&shared), listener, index))
        },
    ));

    tokio::select! {
        outcome = watch_loop(&shared, watching) => outcome,
        // The accept loops only end if every listener does, which a socket
        // the OS keeps open never does; this arm is here for completeness.
        _ = accepting => Ok(()),
    }
}

/// Whether this person may forward ports from pods in `namespace`, or `None`
/// when the cluster would not say.
///
/// Asked before listening, so a missing permission is the command's error
/// rather than a message on the first connection somebody makes, minutes
/// later, from a browser tab. A review that itself fails is not a refusal:
/// the forward is tried, and a `403` then is explained the same way.
async fn allowed(client: &Client, namespace: &str, budget: Budget) -> Option<bool> {
    let review = SelfSubjectAccessReview {
        spec: SelfSubjectAccessReviewSpec {
            resource_attributes: Some(ResourceAttributes {
                namespace: Some(namespace.to_owned()),
                verb: Some("create".to_owned()),
                resource: Some("pods".to_owned()),
                subresource: Some("portforward".to_owned()),
                ..ResourceAttributes::default()
            }),
            ..SelfSubjectAccessReviewSpec::default()
        },
        ..SelfSubjectAccessReview::default()
    };
    let api: Api<SelfSubjectAccessReview> = Api::all(client.clone());
    budget
        .wrap(api.create(&PostParams::default(), &review))
        .await
        .map_err(|error| tracing::debug!(%error, "asking whether port-forward is allowed failed"))
        .ok()?
        .status
        .map(|status| status.allowed)
}

/// The target, found: the pod to start on, and for `svc/` and `deploy/`
/// what finds its successors.
struct Resolved {
    pod: Pod,
    service: Option<Service>,
    /// The label selector behind a `svc/` or `deploy/` target.
    selector: Option<String>,
}

async fn resolve(
    client: &Client,
    cluster: &ClusterView,
    namespace: &str,
    target: &Target,
    selectors: &Selectors,
    budget: Budget,
) -> Result<Resolved> {
    let label = cluster.label();
    let now = Timestamp::now();
    match target.kind {
        Kind::Pod => {
            let pod =
                exec::locate(client, cluster, namespace, &target.name, selectors, budget).await?;
            if let Some(why) = choose::unusable(&pod, now, Surface::Command) {
                bail!(why);
            }
            Ok(Resolved {
                pod,
                service: None,
                selector: None,
            })
        }
        Kind::Service => {
            let service: Service = named(client, &label, namespace, target, budget).await?;
            let selector = choose::service_selector(&service).map_err(|why| anyhow!(why))?;
            let pod = ready_pod(client, &label, namespace, target, &selector, budget).await?;
            Ok(Resolved {
                pod,
                service: Some(service),
                selector: Some(selector),
            })
        }
        Kind::Deployment => {
            let deployment: Deployment = named(client, &label, namespace, target, budget).await?;
            let selector = choose::deployment_selector(&deployment).map_err(|why| anyhow!(why))?;
            let pod = ready_pod(client, &label, namespace, target, &selector, budget).await?;
            Ok(Resolved {
                pod,
                service: None,
                selector: Some(selector),
            })
        }
    }
}

/// The service or deployment `target` names, by full name or unique prefix.
///
/// The same bargain as [`exec::locate`] strikes for pods: a listing so a
/// prefix can be matched, a `get` by the full name when listing is refused,
/// and, when nothing matches here, a best-effort look across the cluster so
/// the message can name the namespace it is in.
async fn named<K>(
    client: &Client,
    label: &str,
    namespace: &str,
    target: &Target,
    budget: Budget,
) -> Result<K>
where
    K: kube::Resource<Scope = k8s_openapi::NamespaceResourceScope, DynamicType = ()>
        + Clone
        + DeserializeOwned
        + Debug,
{
    let noun = target.kind.noun();
    let wanted = target.name.as_str();
    let api: Api<K> = Api::namespaced(client.clone(), namespace);
    let items = match budget.wrap(api.list(&ListParams::default())).await {
        Ok(list) => list.items,
        Err(error) if Failure::of(&error) == Failure::Forbidden => {
            tracing::debug!(%error, "listing was refused; trying the name as given");
            return match budget.wrap(api.get_opt(wanted)).await {
                Ok(Some(item)) => Ok(item),
                Ok(None) => Err(anyhow!(
                    "there is no {noun} called {wanted:?} in namespace {namespace}, and your access \
                     does not include listing {noun}s there, so eks cannot match the start of a name.\n\
                     Give the {noun}'s full name, or ask a cluster admin for `list` on {noun}s in {namespace}."
                )),
                Err(error) => Err(anyhow!(k8s::explain(&error, label))),
            };
        }
        Err(error) => return Err(anyhow!(k8s::explain(&error, label))),
    };

    match choose::find(&items, wanted) {
        Found::One(item) => Ok(item.clone()),
        Found::Several(candidates) => {
            let names: Vec<String> = candidates.iter().map(|item| item.name_any()).collect();
            bail!(choose::ambiguous(target.kind, wanted, &names))
        }
        Found::None => {
            let here: Vec<String> = items.iter().map(ResourceExt::name_any).collect();
            let everywhere: Vec<K> = budget
                .wrap(Api::<K>::all(client.clone()).list(&ListParams::default()))
                .await
                .map(|list| list.items)
                .map_err(|error| tracing::debug!(%error, "searching every namespace failed"))
                .unwrap_or_default();
            let mut elsewhere: Vec<String> = everywhere
                .iter()
                .filter(|item| !wanted.is_empty() && item.name_any().starts_with(wanted))
                .filter_map(ResourceExt::namespace)
                .filter(|other| other != namespace)
                .collect();
            elsewhere.sort();
            elsewhere.dedup();
            bail!(choose::not_found(
                target.kind,
                wanted,
                namespace,
                &here,
                &elsewhere
            ))
        }
    }
}

/// The pods `selector` matches in `namespace`, finished ones included so a
/// message can show them.
async fn pods_behind(
    client: &Client,
    label: &str,
    namespace: &str,
    selector: &str,
    budget: Budget,
) -> Result<Vec<Pod>> {
    let selectors = Selectors {
        label: Some(selector.to_owned()),
        field: None,
    };
    k8s_pods::fetch_scope(
        client.clone(),
        &Scope::Namespace(namespace.to_owned()),
        &selectors,
        budget,
        &Progress::none(),
    )
    .await
    .map_err(|error| anyhow!(k8s::explain(&error, label)))
}

async fn ready_pod(
    client: &Client,
    label: &str,
    namespace: &str,
    target: &Target,
    selector: &str,
    budget: Budget,
) -> Result<Pod> {
    let pods = pods_behind(client, label, namespace, selector, budget).await?;
    let now = Timestamp::now();
    match choose::choose(&pods, None, now) {
        Some(pod) => Ok(pod.clone()),
        None => bail!(choose::none_ready(target, selector, &pods, now)),
    }
}

/// Where one spec lands: the service port asked for, if any, and the port in
/// the pod.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Port {
    service: Option<u16>,
    pod: u16,
}

impl Port {
    /// The number a forward listens on when no local port was given: the
    /// one that was asked for, which for a service is its own port — what
    /// its clients dial, and what its docs will say.
    fn local_default(self) -> u16 {
        self.service.unwrap_or(self.pod)
    }
}

fn land(service: Option<&Service>, remote: &Remote, pod: &Pod) -> Result<Port, ports::Error> {
    let Some(service) = service else {
        return Ok(Port {
            service: None,
            pod: ports::on_pod(pod, remote)?,
        });
    };
    let port = ports::service_port(service, remote)?;
    Ok(Port {
        service: u16::try_from(port.port).ok(),
        pod: ports::target_on_pod(&service.name_any(), port, pod)?,
    })
}

/// The spec to use when none was typed: the one port there is, or the one
/// the person picks when there are several.
async fn default_spec(target: &Target, resolved: &Resolved) -> Result<Spec> {
    let (default, owner, beside) = match &resolved.service {
        Some(service) => (
            ports::default_on_service(service, &resolved.pod)?,
            format!("service {}", service.name_any()),
            "GOES TO",
        ),
        None => (
            ports::default_on_pod(&resolved.pod)?,
            format!("pod {}", resolved.pod.name_any()),
            "CONTAINER",
        ),
    };
    let offered = match default {
        DefaultPort::One(remote) => {
            return Ok(Spec {
                local: Local::Same,
                remote,
            });
        }
        DefaultPort::Choose(offered) => offered,
    };

    let typed = target.to_string();
    if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
        bail!(ports::unchosen(&owner, &typed, &offered, beside));
    }
    eprint!("{}", ports::question(&owner, &offered, beside));
    // Read through `tokio` rather than `std`, so the read is something the
    // Ctrl-C race in `main` can drop: a blocking read on this thread would
    // keep the signal waiting until somebody pressed Enter.
    let mut answer = String::new();
    tokio::io::BufReader::new(tokio::io::stdin())
        .read_line(&mut answer)
        .await?;
    match ports::answer(&answer, &offered) {
        Some(remote) => Ok(Spec {
            local: Local::Same,
            remote,
        }),
        None => bail!(ports::unanswered(&answer, &typed)),
    }
}

/// One forward's listeners, one per address, all on the same port.
struct Listening {
    listeners: Vec<TcpListener>,
    /// Why the port is not the one that was preferred, when it is not.
    fallback: Option<String>,
}

async fn listen(listens: &[Listen], spec: &Spec, remote: u16) -> Result<Listening> {
    let remote_text = spec.remote.to_string();
    let mut listeners = Vec::with_capacity(listens.len());
    let mut fallback = None;
    let mut port = None;

    for listen in listens {
        let address = listen.address;
        let bound = match (port, forward::bind(spec.local, remote)) {
            // Every address after the first takes the port the first got,
            // so one forward is one port wherever it is reached.
            (Some(port), _) | (None, Bind::Exactly(port)) => TcpListener::bind((address, port))
                .await
                .map_err(|error| (port, error)),
            (None, Bind::Prefer(preferred)) => {
                match TcpListener::bind((address, preferred)).await {
                    Ok(listener) => Ok(listener),
                    Err(error) => {
                        tracing::debug!(%error, preferred, "falling back to any free port");
                        fallback = Some(forward::unavailable(preferred, &error));
                        TcpListener::bind((address, 0))
                            .await
                            .map_err(|error| (0, error))
                    }
                }
            }
            (None, Bind::Any) => TcpListener::bind((address, 0))
                .await
                .map_err(|error| (0, error)),
        };
        match bound {
            Ok(listener) => {
                port = Some(listener.local_addr()?.port());
                listeners.push(listener);
            }
            Err((tried, error)) if listen.required => {
                bail!(forward::bind_failed(address, tried, &remote_text, &error))
            }
            Err((_, error)) => {
                tracing::debug!(%error, %address, "an optional address could not be listened on");
            }
        }
    }
    Ok(Listening {
        listeners,
        fallback,
    })
}

/// Where connections go right now: the pod, and the port in it for each
/// forward, in the order the forwards were given.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Destination {
    pod: String,
    ports: Vec<u16>,
}

/// What every connection and the watch loop share.
struct Shared {
    api: Api<Pod>,
    target: Target,
    label: String,
    namespace: String,
    budget: Budget,
    /// `None` while a `svc/` or `deploy/` forward waits for a ready pod.
    destination: watch::Sender<Option<Destination>>,
    /// Woken by a connection that found its pod gone, so the watch loop looks
    /// now rather than at its next tick.
    recheck: Notify,
    /// Where the lines about connections and the pod go.
    report: Report,
}

/// Where a running forward's commentary goes.
///
/// `eks port-forward` owns its terminal and writes a line to stderr for each
/// thing worth knowing. A forward the dashboard started must never write to
/// the terminal — it would land on top of the frame — so the same moments go
/// down a channel as [`Event`]s, with the connection count the strip shows
/// beside them.
#[derive(Debug, Clone)]
enum Report {
    Stderr,
    Dashboard(mpsc::Sender<Event>),
}

impl Report {
    /// One line about something that went wrong and did not end the forward.
    fn say(&self, line: &str) {
        match self {
            Self::Stderr => eprintln!("{line}"),
            Self::Dashboard(events) => {
                // A dashboard that has stopped listening has also dropped the
                // handle that ends this forward; nothing is lost.
                let _ = events.send(Event::Problem(line.to_owned()));
            }
        }
    }

    /// Something only the dashboard counts: a connection opening or closing.
    fn count(&self, event: Event) {
        if let Self::Dashboard(events) = self {
            let _ = events.send(event);
        }
    }

    fn surface(&self) -> Surface {
        match self {
            Self::Stderr => Surface::Command,
            Self::Dashboard(_) => Surface::Dashboard,
        }
    }
}

/// Accept connections on one listener until the command ends.
///
/// The connections' tasks live in a [`JoinSet`] owned here, so when the
/// command's future is dropped — Ctrl-C — every one of them is aborted with
/// it rather than left running behind a closed listener.
async fn accept(shared: Arc<Shared>, listener: TcpListener, index: usize) {
    let mut connections = JoinSet::new();
    loop {
        while connections.try_join_next().is_some() {}
        match listener.accept().await {
            Ok((socket, _)) => {
                let shared = Arc::clone(&shared);
                connections.spawn(async move {
                    shared.report.count(Event::Opened);
                    connection(&shared, socket, index).await;
                    shared.report.count(Event::Closed);
                });
            }
            Err(error) => {
                // Out of file descriptors, usually. Pausing lets some close.
                tracing::debug!(%error, "accepting a connection failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// How one connection's stream to the pod failed.
enum Broke {
    /// The pod was not there, and nothing was sent: the connection can be
    /// tried again on another pod without the client noticing.
    Gone,
    /// Anything else, as a sentence for stderr.
    Said(String),
}

/// Carry one local connection to the pod, and say why if that fails.
///
/// A connection that finds its pod gone wakes the watch loop and waits for it
/// to settle on another pod, then tries once more. During a rollout that
/// turns what would be a dropped request into a slightly slow one.
async fn connection(shared: &Shared, mut socket: TcpStream, index: usize) {
    let mut tried_again = false;
    loop {
        let current = shared.destination.borrow().clone();
        let Some(destination) = current else {
            shared
                .report
                .say(&forward::refused_while_waiting(&shared.target));
            return;
        };
        let Some(&port) = destination.ports.get(index) else {
            return;
        };
        match carry(shared, &destination.pod, port, &mut socket).await {
            Ok(()) => return,
            Err(Broke::Gone) if !tried_again => {
                tried_again = true;
                let mut changes = shared.destination.subscribe();
                changes.mark_unchanged();
                shared.recheck.notify_one();
                let _ = tokio::time::timeout(RESETTLE, changes.changed()).await;
            }
            Err(Broke::Gone) => {
                shared.report.say(&format!(
                    "a connection was dropped: pod {} was gone and no other pod was ready in time.",
                    destination.pod
                ));
                return;
            }
            Err(Broke::Said(message)) => {
                shared.report.say(&message);
                return;
            }
        }
    }
}

async fn carry(shared: &Shared, pod: &str, port: u16, socket: &mut TcpStream) -> Result<(), Broke> {
    let mut forwarder = shared
        .budget
        .wrap(shared.api.portforward(pod, &[port]))
        .await
        .map_err(|error| refused(shared, &error))?;
    let Some(mut upstream) = forwarder.take_stream(port) else {
        return Err(Broke::Said(format!(
            "the forward to pod {pod} port {port} opened without a stream for that port."
        )));
    };
    let errors = forwarder.take_error(port);
    let errors = async move {
        match errors {
            Some(errors) => errors.await,
            None => None,
        }
    };
    tokio::pin!(errors);

    let outcome = tokio::select! {
        // The error is looked at first: a kubelet that cannot reach the port
        // sends its reason and closes the stream in the same breath, and the
        // reason is the part worth printing.
        biased;
        Some(message) = &mut errors => Err(message),
        copied = tokio::io::copy_bidirectional(socket, &mut upstream) => {
            if let Err(error) = copied {
                // Usually the client hanging up mid-response, which is its
                // business; anything else shows up as the pod's error below.
                tracing::debug!(%error, "a forwarded connection ended early");
            }
            // The reason may land just after the stream closes. It resolves
            // to `None` as soon as the forward finishes cleanly, so this only
            // waits when there is something to wait for.
            match tokio::time::timeout(Duration::from_secs(1), &mut errors).await {
                Ok(Some(message)) => Err(message),
                _ => Ok(()),
            }
        }
    };
    drop(upstream);
    // Let the forward's own task send its close; it is cut off rather than
    // waited on if the cluster does not answer it.
    let _ = tokio::time::timeout(Duration::from_secs(1), forwarder.join()).await;

    outcome.map_err(|message| {
        Broke::Said(forward::connection_failed(
            &shared.target,
            pod,
            port,
            &message,
            shared.report.surface(),
        ))
    })
}

/// Why the cluster would not open a forward.
fn refused(shared: &Shared, error: &page::Error) -> Broke {
    match (Failure::of(error), exec::upgrade_status(error)) {
        (_, Some(404)) => Broke::Gone,
        (Failure::Forbidden, _) | (_, Some(403)) => {
            Broke::Said(forward::forbidden(&shared.label, &shared.namespace))
        }
        _ => Broke::Said(k8s::explain(error, &shared.label)),
    }
}

/// What the watch loop keeps an eye on.
enum Watching {
    /// A pod named directly, as it was last seen.
    Pod { last: Box<Pod> },
    /// The pods behind a service or deployment.
    Set {
        client: Client,
        selector: String,
        /// Boxed only to keep the two variants a similar size.
        service: Option<Box<Service>>,
        /// Each forward's remote, as typed, to resolve again against
        /// whichever pod is moved to: a named port may be a different number
        /// in the next revision.
        remotes: Vec<Remote>,
    },
}

/// Look at the pod every [`CHECK_EVERY`], and whenever a connection asks,
/// until a pod named directly is gone.
async fn watch_loop(shared: &Shared, mut watching: Watching) -> Result<()> {
    let mut said = Said::new(&shared.report);

    loop {
        tokio::select! {
            () = tokio::time::sleep(CHECK_EVERY) => {}
            () = shared.recheck.notified() => {}
        }
        let now = Timestamp::now();
        match &mut watching {
            Watching::Pod { last } => {
                let name = last.name_any();
                match shared.budget.wrap(shared.api.get_opt(&name)).await {
                    Ok(seen) => {
                        match choose::after_pod(seen.as_ref(), last, now, shared.report.surface()) {
                            Verdict::Keep => {}
                            Verdict::Warn(line) => said.once(line),
                            Verdict::Stop(why) => bail!(why),
                        }
                        if let Some(seen) = seen {
                            **last = seen;
                        }
                    }
                    Err(error) => said.once(unchecked(&name, &error, &shared.label)),
                }
            }
            Watching::Set {
                client,
                selector,
                service,
                remotes,
            } => {
                let pods = match pods_behind(
                    client,
                    &shared.label,
                    &shared.namespace,
                    selector,
                    shared.budget,
                )
                .await
                {
                    Ok(pods) => pods,
                    Err(error) => {
                        said.once(format!(
                            "could not look at the pods behind {}: {error:#}\nForwarding carries on to the pod it had.",
                            shared.target
                        ));
                        continue;
                    }
                };
                let current = shared
                    .destination
                    .borrow()
                    .as_ref()
                    .map(|destination| destination.pod.clone());
                match choose::after_set(&shared.target, current.as_deref(), &pods, now) {
                    Move::Keep => {}
                    Move::Wait(why) => {
                        shared.destination.send_replace(None);
                        said.once(choose::waiting(&why));
                    }
                    Move::Switch { to, why } => {
                        let Some(pod) = pods.iter().find(|pod| pod.name_any() == to) else {
                            continue;
                        };
                        let landed = remotes
                            .iter()
                            .map(|remote| {
                                land(service.as_deref(), remote, pod).map(|port| port.pod)
                            })
                            .collect::<Result<Vec<_>, _>>();
                        match landed {
                            Ok(ports) => {
                                shared.destination.send_replace(Some(Destination {
                                    pod: to.clone(),
                                    ports,
                                }));
                                said.always(&choose::switched(&shared.target, &why, &to));
                            }
                            Err(error) => {
                                shared.destination.send_replace(None);
                                said.once(format!(
                                    "{why}, and pod {to} is ready but cannot be forwarded to: {error}"
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The watch loop's lines, each state reported once however many looks find
/// it unchanged.
struct Said<'a> {
    report: &'a Report,
    last: Option<String>,
}

impl<'a> Said<'a> {
    fn new(report: &'a Report) -> Self {
        Self { report, last: None }
    }

    /// Report `line` unless it is what was reported last.
    fn once(&mut self, line: String) {
        if self.last.as_ref() != Some(&line) {
            self.report.say(&line);
            self.last = Some(line);
        }
    }

    /// Report `line`, an event rather than a state, and forget what was
    /// reported before it, so the state that follows is reported afresh.
    fn always(&mut self, line: &str) {
        self.report.say(line);
        self.last = None;
    }
}

/// A look at a pod named directly failed. The forward carries on: one failed
/// request is not evidence the pod has gone.
fn unchecked(pod: &str, error: &page::Error, label: &str) -> String {
    format!(
        "could not check on pod {pod}: {}\nForwarding carries on.",
        k8s::explain(error, label)
    )
}

/// Forward one port of one pod for the dashboard, until the returned handle
/// is dropped.
///
/// The same steps as [`run`] after it has resolved a pod by name: the
/// permission check, a listener on loopback that prefers the pod's own port
/// number, one stream per connection, and the watch loop. What differs is
/// where it all goes. Nothing is printed; every line `run` would write on
/// stderr arrives as an [`Event`], and the reason the forward stopped —
/// whether it never started or its pod went away — is its last one,
/// [`Event::Ended`]. Credentials come from the dashboard's shared `store`,
/// so the helper never prompts on the dashboard's terminal (decision 120).
///
/// Dropping the handle drops the whole future: the listener closes and every
/// connection's task is aborted with it, as Ctrl-C does for the command.
#[must_use]
pub fn spawn_dashboard(
    config: KubeConfig,
    paths: Vec<PathBuf>,
    context: String,
    target: PodPort,
    budget: Budget,
    store: Store,
) -> (mpsc::Receiver<Event>, StreamHandle) {
    super::spawn_stream(move |events, stop| async move {
        let report = Report::Dashboard(events.clone());
        let served = tokio::select! {
            _ = stop => return,
            served = serve(&config, &paths, &context, &target, budget, &store, report) => served,
        };
        let error = served.err().unwrap_or_else(|| {
            // The accept loop never returns while its listener is open, so
            // this is the same "for completeness" arm `run` has.
            anyhow!(
                "the forward to pod {} stopped accepting connections.",
                target.pod
            )
        });
        let FetchError {
            message,
            credentials,
        } = FetchError::of(&error);
        let _ = events.send(Event::Ended {
            message,
            credentials,
        });
    })
}

/// [`spawn_dashboard`]'s future, apart so its early returns can use `?`.
async fn serve(
    config: &KubeConfig,
    paths: &[PathBuf],
    context: &str,
    target: &PodPort,
    budget: Budget,
    store: &Store,
    report: Report,
) -> Result<()> {
    let PodPort {
        namespace,
        pod: pod_name,
        port,
    } = target;
    let cluster = target_cluster(config, Some(context))?;
    let label = cluster.label();
    let client = k8s::client::connect_kept(paths, &cluster, budget, store).await?;

    if allowed(&client, namespace, budget).await == Some(false) {
        bail!(forward::forbidden(&label, namespace));
    }

    let api: Api<Pod> = Api::namespaced(client, namespace);
    let pod = budget
        .wrap(api.get_opt(pod_name))
        .await
        .map_err(|error| anyhow!(k8s::explain(&error, &label)))?
        .ok_or_else(|| anyhow!(forward::pod_gone(pod_name, namespace)))?;
    if let Some(why) = choose::unusable(&pod, Timestamp::now(), Surface::Dashboard) {
        bail!(why);
    }
    let remote = Remote::Number(*port);
    let pod_port = ports::on_pod(&pod, &remote)?;
    let spec = Spec {
        local: Local::Same,
        remote,
    };
    let listening = listen(&spec::addresses(&[])?, &spec, pod_port).await?;
    let Some(listener) = listening.listeners.into_iter().next() else {
        bail!("eks could not listen on any local address for pod {pod_name} port {port}.");
    };
    let address = listener.local_addr()?;
    report.count(Event::Listening {
        url: spec::url(address.ip(), address.port()),
        notes: listening.fallback.into_iter().collect(),
    });

    let (destination, _) = watch::channel(Some(Destination {
        pod: pod_name.clone(),
        ports: vec![pod_port],
    }));
    let shared = Arc::new(Shared {
        api,
        target: Target {
            kind: Kind::Pod,
            name: pod_name.clone(),
        },
        label,
        namespace: namespace.clone(),
        budget,
        destination,
        recheck: Notify::new(),
        report,
    });
    tokio::select! {
        outcome = watch_loop(&shared, Watching::Pod { last: Box::new(pod) }) => outcome,
        () = accept(Arc::clone(&shared), listener, 0) => Ok(()),
    }
}
