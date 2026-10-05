//! `eks exec` — a shell, or any command, in a container.
//!
//! What `kubectl exec -it api-7d9f8c6b5-xk2pq -c app -- /bin/sh` makes a
//! person assemble by hand, `eks exec api` works out: which pod `api` means
//! ([`pick::find`]), which container ([`pick::container`]), whether it can
//! run anything right now ([`pick::running`]), which shell the image has
//! ([`remote::shells`]), and whether this terminal wants a TTY
//! ([`remote::wants_tty`]). Each of those is a pure function with its own
//! tests. This module is the I/O around them: the requests, the terminal's
//! raw mode, its size, and the sentences printed when something stops the
//! session from starting.
//!
//! Every failure before the session starts is an `Err` that `main` prints the
//! usual way. Once it has started, the remote command's own exit code is
//! `eks`'s — so `eks exec api -- test -f /ready` is usable in a script.
//!
//! The dashboard's `x` opens the same session in two halves, because the
//! dashboard must not wait on the network while it holds the terminal.
//! [`spawn_prepare`] runs on a background thread and finds the pod, the
//! container, and a shell that is really in the image. Any refusal comes
//! back as a sentence for the status line, with the dashboard still on
//! screen. Only a [`Plan`] that will start is handed to [`run_prepared`],
//! once `ui::run` has given the terminal back. Both halves use the checks and
//! the wording `eks exec` uses; [`Surface`] changes only the advice, so the
//! dashboard names keys rather than flags.

use std::io::{self, IsTerminal};
use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow, bail};
use futures_util::stream::{self, BoxStream, StreamExt};
use k8s_openapi::api::core::v1::{Node, Pod};
use k8s_openapi::jiff::Timestamp;
use kube::Client;
use kube::api::{Api, AttachParams, TerminalSize};
use ratatui::crossterm::terminal;
use tokio::io::AsyncRead;

use crate::aws::LoginMode;
use crate::cluster::ClusterView;
use crate::commands::{self, FetchError, credentials, nodes::target_cluster, pods::selectors_for};
use crate::k8s::auth::Store;
use crate::k8s::page::{self, Budget};
use crate::k8s::pods::events::{self, EventRow};
use crate::k8s::pods::pick::{self, ContainerError, Match, NotRunning};
use crate::k8s::pods::{self as k8s_pods, Scope, Selectors};
use crate::k8s::remote::{self, Ending, Local, Os, Remote};
use crate::k8s::{self, Failure};
use crate::kubeconfig::KubeConfig;
use crate::progress::Progress;

mod keyboard;

/// What a shell's exit status is when a pipe it was writing to has closed:
/// 128 plus `SIGPIPE`'s number. `eks exec api -- cat big.log | head` ends
/// this way, quietly, as the same pipeline with a local `cat` would.
const BROKEN_PIPE: u8 = 141;

/// What `eks exec` was asked to do, as it came off the command line.
#[derive(Debug, Clone, Default)]
pub struct Request<'a> {
    /// The pod: a full name, or the start of exactly one.
    pub pod: &'a str,
    /// `--container`. Without one, the pod's default container.
    pub container: Option<&'a str>,
    /// What to run, after `--`. Empty asks for a shell.
    pub command: &'a [String],
    /// `--namespace`, or the config file's. Without either, the context's.
    pub namespace: Option<&'a str>,
    /// `-l`, unparsed. Narrows the pods a prefix is matched against.
    pub label_selector: Option<&'a str>,
    /// `--field-selector`, unparsed.
    pub field_selector: Option<&'a str>,
    /// Whether the session runs with a TTY — [`remote::wants_tty`], asked in
    /// `main` where the terminal is.
    pub tty: bool,
    /// `--timeout`: each step up to the session starting. Never the session
    /// itself, which lasts as long as the person in it wants.
    pub budget: Budget,
    /// `--login`, for the credential check before connecting.
    pub login: LoginMode,
}

/// Where a message about a session is read, which decides the advice in it:
/// a flag to pass on the command line, or a key to press in the dashboard.
/// The diagnosis is the same on both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// `eks exec`.
    Command,
    /// `x` in the dashboard.
    Dashboard,
}

/// Run `request` and return the exit code `eks` should end with.
///
/// `context` is `--context`, whose short `-c` is `kubectl exec`'s spelling
/// for the container. A context that does not resolve therefore gets a line
/// pointing at `--container`, since that is the likeliest way to have typed
/// one here.
pub async fn run(
    config: &KubeConfig,
    paths: &[PathBuf],
    context: Option<&str>,
    request: Request<'_>,
) -> Result<u8> {
    let target = target_cluster(config, context)
        .map_err(|error| with_context_hint(&error, context.is_some()))?;
    let label = target.label();
    let selectors = selectors_for(request.label_selector, request.field_selector)?;
    let namespace = request
        .namespace
        .map_or_else(|| target.namespace.clone(), ToOwned::to_owned);

    let client = credentials::connect(
        paths,
        &target,
        request.budget,
        request.login,
        &Progress::none(),
    )
    .await?;

    let pod = locate(
        &client,
        &target,
        &namespace,
        request.pod,
        &selectors,
        request.budget,
    )
    .await?;
    let pod_name = pod.metadata.name.clone().unwrap_or_default();
    let container = pick::container(&pod, request.container)?;

    let now = Timestamp::now();
    if let Err(why) = pick::running(&pod, &container, now) {
        // Asked only now, on the way out: a pod that is stuck is the one
        // whose events are worth a request, and the reader would otherwise
        // have to go and find them.
        let events = events::fetch(client.clone(), &namespace, &pod_name, request.budget)
            .await
            .map(|events| events::from_events(&events, now))
            .map_err(|error| tracing::debug!(%error, "listing pod events failed"))
            .ok();
        bail!(not_running(
            &pod_name,
            &namespace,
            &why,
            events.as_deref(),
            &pick::running_containers(&pod),
            Surface::Command,
        ));
    }

    let api: Api<Pod> = Api::namespaced(client.clone(), &namespace);
    let session = Session {
        api: &api,
        label: &label,
        namespace: &namespace,
        pod: &pod_name,
        container: &container,
        tty: request.tty,
        budget: request.budget,
        watch_signals: true,
        surface: Surface::Command,
    };
    let mut stdio = Stdio {
        stdin: tokio::io::stdin(),
        stdout: tokio::io::stdout(),
        stderr: tokio::io::stderr(),
    };

    if request.command.is_empty() {
        session.shell(&client, &pod, &mut stdio).await
    } else {
        session.command(request.command, &mut stdio).await
    }
}

/// Find the pod `wanted` names in `namespace`.
///
/// A listing, so a prefix can be matched — and, when the role behind the
/// context may `get` pods but not `list` them, a `get` by the full name
/// instead, which is all `kubectl exec` itself would have needed. When
/// nothing in the namespace matches, the same prefix is looked for across the
/// cluster, so the message can name the namespace the pod is actually in.
///
/// `eks port-forward` finds a pod named directly by this same rule.
pub(crate) async fn locate(
    client: &Client,
    target: &ClusterView,
    namespace: &str,
    wanted: &str,
    selectors: &Selectors,
    budget: Budget,
) -> Result<Pod> {
    let label = target.label();
    let scope = Scope::Namespace(namespace.to_owned());
    let listed =
        k8s_pods::fetch_scope(client.clone(), &scope, selectors, budget, &Progress::none()).await;

    let pods = match listed {
        Ok(pods) => pods,
        Err(error) if Failure::of(&error) == Failure::Forbidden => {
            tracing::debug!(%error, "listing pods was refused; trying the name as given");
            let api: Api<Pod> = Api::namespaced(client.clone(), namespace);
            return match budget.wrap(api.get_opt(wanted)).await {
                Ok(Some(pod)) => Ok(pod),
                Ok(None) => Err(anyhow!(unlistable(wanted, namespace))),
                Err(error) => Err(anyhow!(k8s::explain(&error, &label))),
            };
        }
        Err(error) => return Err(anyhow!(k8s::explain(&error, &label))),
    };

    let now = Timestamp::now();
    match pick::find(&pods, wanted) {
        Match::One(pod) => Ok(pod.clone()),
        Match::Several(candidates) => Err(anyhow!(pick::ambiguous(wanted, &candidates, now))),
        Match::None => {
            // Best effort: a role scoped to one namespace cannot list the
            // cluster, and that is no reason to hide the answer it did get.
            let everywhere = k8s_pods::fetch_scope(
                client.clone(),
                &Scope::All,
                selectors,
                budget,
                &Progress::none(),
            )
            .await
            .map_err(|error| tracing::debug!(%error, "searching every namespace failed"))
            .unwrap_or_default();
            let elsewhere: Vec<&Pod> = everywhere
                .iter()
                .filter(|pod| pod.metadata.namespace.as_deref() != Some(namespace))
                .filter(|pod| {
                    pod.metadata
                        .name
                        .as_deref()
                        .is_some_and(|name| !wanted.is_empty() && name.starts_with(wanted))
                })
                .collect();
            Err(anyhow!(pick::not_found(wanted, namespace, &elsewhere, now)))
        }
    }
}

/// The OS the pod's node runs, from its labels — or `None` when the node
/// cannot be read, which a role scoped to a namespace usually cannot.
async fn node_os(client: &Client, pod: &Pod, budget: Budget) -> Option<Os> {
    let node = pod.spec.as_ref()?.node_name.as_deref()?;
    let api: Api<Node> = Api::all(client.clone());
    let node = budget
        .wrap(api.get(node))
        .await
        .map_err(|error| tracing::debug!(%error, "reading the pod's node failed"))
        .ok()?;
    remote::os_of_labels(node.metadata.labels.as_ref())
}

/// What looking for a shell in a container found.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Search {
    /// This shell, one of `os`'s, ran and ended with `code`.
    Ended {
        os: Os,
        shell: Vec<String>,
        code: u8,
    },
    /// None of the shells for this OS is in the image.
    NoShell(Os),
    /// This shell could not be run for a reason other than not existing.
    Broke { shell: Vec<String>, ending: Ending },
}

/// Try `attempt` on each shell the container might have until one runs.
///
/// The pod's own word for its OS first, then Linux's shells, and the node
/// asked only once neither of those exists — a Windows pod that says
/// nothing about it is rare, and asking every time would be a request
/// most sessions never need.
///
/// `eks exec` attempts the real session ([`Interactive`]). The dashboard's
/// [`prepare`] attempts a [`Probe`], which runs the shell without a terminal
/// and returns as soon as it has started.
async fn search(
    client: &Client,
    pod: &Pod,
    budget: Budget,
    attempt: &mut impl Attempt,
) -> Result<Search> {
    let mut os = remote::os_of_pod(pod);
    let mut tried = Vec::new();
    loop {
        let current = os.unwrap_or(Os::Linux);
        tried.push(current);
        for shell in remote::shells(current) {
            match attempt.attempt(&shell).await? {
                Ending::Exited(code) => {
                    return Ok(Search::Ended {
                        os: current,
                        shell,
                        code,
                    });
                }
                Ending::Missing(detail) => {
                    tracing::debug!(%detail, shell = ?shell, "no such shell in the image");
                }
                ending => return Ok(Search::Broke { shell, ending }),
            }
        }

        if os.is_some() {
            break;
        }
        os = node_os(client, pod, budget).await;
        if os.is_none_or(|os| tried.contains(&os)) {
            break;
        }
    }
    Ok(Search::NoShell(os.unwrap_or(Os::Linux)))
}

/// One way of trying a shell, for [`search`].
///
/// A trait rather than an async closure: the dashboard's search runs on a
/// thread [`commands::spawn`] starts, so its future must be `Send`, and the
/// compiler cannot yet prove that of an async closure borrowing the session.
trait Attempt {
    async fn attempt(&mut self, shell: &[String]) -> Result<Ending>;
}

/// The real session, on this process's terminal.
struct Interactive<'a, I> {
    session: &'a Session<'a>,
    stdio: &'a mut Stdio<I>,
}

impl<I: AsyncRead + Unpin> Attempt for Interactive<'_, I> {
    async fn attempt(&mut self, shell: &[String]) -> Result<Ending> {
        self.session.attempt(shell, self.stdio).await
    }
}

/// [`probe`], for each shell in turn.
struct Probe<'a>(&'a Session<'a>);

impl Attempt for Probe<'_> {
    async fn attempt(&mut self, shell: &[String]) -> Result<Ending> {
        probe(self.0, shell).await
    }
}

/// The attach parameters for [`probe`]: output only, no input and no TTY.
///
/// Without stdin the shell's input is at its end as it starts, so it exits at
/// once with nothing to run. Stdout is attached only because the API server
/// refuses a session with no streams at all; whatever reaches it is dropped.
fn probe_params(container: &str) -> AttachParams {
    AttachParams {
        container: Some(container.to_owned()),
        stdin: false,
        stdout: true,
        stderr: false,
        tty: false,
        ..AttachParams::default()
    }
}

/// Whether `shell` is in the container, by running it with no input.
///
/// A shell that is there exits at once and reports its exit code. One that
/// is not is refused by the runtime as [`Ending::Missing`], exactly as the
/// real session would be. Any output is thrown away.
///
/// A shell still running when the budget runs out has started, so it
/// counts as there: the session that follows will say if it is not.
async fn probe(session: &Session<'_>, shell: &[String]) -> Result<Ending> {
    let mut attached = session
        .budget
        .wrap(session.api.exec(
            session.pod,
            shell.to_vec(),
            &probe_params(session.container),
        ))
        .await
        .map_err(|error| {
            refused(
                &error,
                session.label,
                session.namespace,
                session.pod,
                session.surface,
            )
        })?;
    let status = attached.take_status();
    let (mut input, mut output, mut errors) =
        (tokio::io::empty(), tokio::io::sink(), tokio::io::sink());
    let remote = Remote {
        stdin: None::<tokio::io::Empty>,
        stdout: attached.stdout(),
        stderr: attached.stderr(),
    };
    let relayed = remote::relay(
        remote,
        async move {
            match status {
                Some(status) => status.await,
                None => None,
            }
        },
        Local {
            stdin: &mut input,
            stdout: &mut output,
            stderr: &mut errors,
        },
        stream::empty(),
        |_| {},
    );
    let ending = match session.budget.limit() {
        Some(limit) => tokio::time::timeout(limit, relayed)
            .await
            .unwrap_or(Ok(Ending::Exited(0))),
        None => relayed.await,
    };
    drop(attached);
    ending.context("the probe for a shell failed partway")
}

/// The container `x` was pressed on, as the dashboard knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub namespace: String,
    pub pod: String,
    /// `None` on a pod row, which takes the pod's default container by
    /// [`pick::container`]'s rule, as `eks exec` without `--container` does.
    pub container: Option<String>,
}

/// A session [`spawn_prepare`] has checked will start: a running container, and
/// a shell its image has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub namespace: String,
    pub pod: String,
    pub container: String,
    pub shell: Vec<String>,
    /// The OS `shell` is for, so a refusal names the right shells.
    pub os: Os,
}

/// Check, on a background thread, that a shell can be opened in `target`.
///
/// Everything `eks exec` checks before its session starts, with the same
/// wording, plus one thing it does not need: that a shell is really in the
/// image. `eks exec` learns that by starting one. The dashboard has to know
/// before it gives up the terminal, because a container without a shell
/// should leave the dashboard on screen with a reason under it. So
/// `probe` runs each candidate once without a terminal first.
#[must_use]
pub fn spawn_prepare(
    config: KubeConfig,
    paths: Vec<PathBuf>,
    cluster: String,
    target: Target,
    budget: Budget,
    store: Store,
) -> std::sync::mpsc::Receiver<Result<Plan, FetchError>> {
    commands::spawn(async move {
        prepare(&config, &paths, &cluster, &target, budget, &store)
            .await
            .map_err(|error| FetchError::of(&error))
    })
}

async fn prepare(
    config: &KubeConfig,
    paths: &[PathBuf],
    cluster: &str,
    target: &Target,
    budget: Budget,
    store: &Store,
) -> Result<Plan> {
    let cluster = target_cluster(config, Some(cluster))?;
    let client = k8s::client::connect_kept(paths, &cluster, budget, store).await?;
    plan(&client, &cluster.label(), target, budget).await
}

/// [`spawn_prepare`]'s checks, over a client that is already connected to
/// the cluster `label` names.
pub async fn plan(client: &Client, label: &str, target: &Target, budget: Budget) -> Result<Plan> {
    let namespace = target.namespace.as_str();
    let api: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let pod = match budget.wrap(api.get_opt(&target.pod)).await {
        Ok(Some(pod)) => pod,
        Ok(None) => bail!(gone(&target.pod, Surface::Dashboard)),
        Err(error) => return Err(k8s::client::Error::explained(&error, label).into()),
    };
    let container = pick::container(&pod, target.container.as_deref())
        .map_err(|error| anyhow!(container_choice(&error, Surface::Dashboard)))?;

    let now = Timestamp::now();
    if let Err(why) = pick::running(&pod, &container, now) {
        let events = events::fetch(client.clone(), namespace, &target.pod, budget)
            .await
            .map(|events| events::from_events(&events, now))
            .map_err(|error| tracing::debug!(%error, "listing pod events failed"))
            .ok();
        bail!(not_running(
            &target.pod,
            namespace,
            &why,
            events.as_deref(),
            &pick::running_containers(&pod),
            Surface::Dashboard,
        ));
    }

    let session = Session {
        api: &api,
        label,
        namespace,
        pod: &target.pod,
        container: &container,
        tty: false,
        budget,
        watch_signals: false,
        surface: Surface::Dashboard,
    };
    let found = search(client, &pod, budget, &mut Probe(&session)).await?;
    match found {
        Search::Ended { os, shell, .. } => Ok(Plan {
            namespace: namespace.to_owned(),
            pod: target.pod.clone(),
            container,
            shell,
            os,
        }),
        Search::NoShell(os) => bail!(no_shell(
            &target.pod,
            namespace,
            &container,
            os,
            Surface::Dashboard
        )),
        Search::Broke { shell, ending } => {
            bail!(session_failed(&ending, &shell, &target.pod, &container))
        }
    }
}

/// Run a session [`spawn_prepare`] planned, on the terminal `ui::run` has just
/// handed back, until the shell exits.
///
/// Blocks, as the login behind `L` does: the terminal is the session's
/// until then. How the shell exits is the user's business, and nothing is
/// reported for it. A session that could not start, or broke, comes back as
/// a sentence for the dashboard's status line.
pub fn run_prepared(
    config: &KubeConfig,
    paths: &[PathBuf],
    cluster: &str,
    plan: &Plan,
    budget: Budget,
    store: &Store,
) -> Result<(), FetchError> {
    commands::block_on(session(config, paths, cluster, plan, budget, store))
        .map_err(|error| FetchError::of(&error))
}

async fn session(
    config: &KubeConfig,
    paths: &[PathBuf],
    cluster: &str,
    plan: &Plan,
    budget: Budget,
    store: &Store,
) -> Result<()> {
    let cluster = target_cluster(config, Some(cluster))?;
    let label = cluster.label();
    // Before connecting, so a slow cluster is not a blank screen.
    println!("{}", banner(plan, &label));

    let client = k8s::client::connect_kept(paths, &cluster, budget, store).await?;
    let api: Api<Pod> = Api::namespaced(client, &plan.namespace);
    let session = Session {
        api: &api,
        label: &label,
        namespace: &plan.namespace,
        pod: &plan.pod,
        container: &plan.container,
        tty: true,
        budget,
        watch_signals: false,
        surface: Surface::Dashboard,
    };
    let mut stdio = Stdio {
        stdin: dashboard_keyboard(),
        stdout: tokio::io::stdout(),
        stderr: tokio::io::stderr(),
    };

    match session.attempt(&plan.shell, &mut stdio).await? {
        Ending::Exited(_) => Ok(()),
        // Found by the probe moments ago: the image changed under the pod,
        // which a restart can do.
        Ending::Missing(_) => bail!(no_shell(
            &plan.pod,
            &plan.namespace,
            &plan.container,
            plan.os,
            Surface::Dashboard
        )),
        other => bail!(session_failed(
            &other,
            &plan.shell,
            &plan.pod,
            &plan.container
        )),
    }
}

/// The line printed above a dashboard session, so the screen says where the
/// shell is and how to get back.
fn banner(plan: &Plan, cluster: &str) -> String {
    format!(
        "Shell in container {} of pod {} ({}, {cluster}). Exit the shell to return to the dashboard.",
        plan.container, plan.pod, plan.namespace
    )
}

/// The dashboard's keyboard: [`keyboard::Keyboard`] where `poll(2)` exists.
#[cfg(unix)]
fn dashboard_keyboard() -> keyboard::Keyboard {
    keyboard::Keyboard::stdin()
}

/// Elsewhere, `tokio`'s stdin, whose last read outlives the session: the
/// first key pressed back in the dashboard is lost to it. No release target
/// is affected; this keeps the crate building where it is not one.
#[cfg(not(unix))]
fn dashboard_keyboard() -> tokio::io::Stdin {
    tokio::io::stdin()
}

/// The process's own standard streams, opened once and shared by every
/// attempt, so input typed while one shell turned out not to exist reaches
/// the next.
///
/// Stdin is a type parameter because the dashboard reads it differently: see
/// [`keyboard`].
struct Stdio<I> {
    stdin: I,
    stdout: tokio::io::Stdout,
    stderr: tokio::io::Stderr,
}

/// Everything one attempt at a session needs besides what it runs.
struct Session<'a> {
    api: &'a Api<Pod>,
    label: &'a str,
    namespace: &'a str,
    pod: &'a str,
    container: &'a str,
    tty: bool,
    budget: Budget,
    /// Whether to end the session cleanly on `SIGTERM` and `SIGHUP` (see
    /// [`terminated`]). Only `eks exec` does. A handler `tokio` installs is
    /// never removed, so in the dashboard it would outlive the session and
    /// leave the dashboard unable to be stopped with `kill`.
    watch_signals: bool,
    surface: Surface,
}

impl Session<'_> {
    /// Run the command the user gave, once.
    async fn command(
        &self,
        argv: &[String],
        stdio: &mut Stdio<impl AsyncRead + Unpin>,
    ) -> Result<u8> {
        match self.attempt(argv, stdio).await? {
            Ending::Exited(code) => Ok(code),
            Ending::Missing(detail) => {
                tracing::debug!(%detail, "the command is not in the image");
                Err(anyhow!(command_missing(argv, self.pod, self.container)))
            }
            other => Err(anyhow!(session_failed(
                &other,
                argv,
                self.pod,
                self.container
            ))),
        }
    }

    /// Find a shell and run it.
    async fn shell(
        &self,
        client: &Client,
        pod: &Pod,
        stdio: &mut Stdio<impl AsyncRead + Unpin>,
    ) -> Result<u8> {
        let found = search(
            client,
            pod,
            self.budget,
            &mut Interactive {
                session: self,
                stdio,
            },
        )
        .await?;
        match found {
            Search::Ended { code, .. } => Ok(code),
            Search::NoShell(os) => bail!(no_shell(
                self.pod,
                self.namespace,
                self.container,
                os,
                self.surface
            )),
            Search::Broke { shell, ending } => {
                bail!(session_failed(&ending, &shell, self.pod, self.container))
            }
        }
    }

    /// Start `argv` in the container and carry the session until it ends.
    ///
    /// The terminal is in raw mode for exactly as long as the session runs,
    /// and back in its own mode before this returns, whichever way it
    /// returns — see [`RawMode`].
    async fn attempt(
        &self,
        argv: &[String],
        stdio: &mut Stdio<impl AsyncRead + Unpin>,
    ) -> Result<Ending> {
        let mut attached = self
            .budget
            .wrap(self.api.exec(
                self.pod,
                argv.to_vec(),
                &remote::params(self.container, self.tty),
            ))
            .await
            .map_err(|error| refused(&error, self.label, self.namespace, self.pod, self.surface))?;

        let raw = if self.tty {
            Some(
                RawMode::enter(terminal::enable_raw_mode, || {
                    let _ = terminal::disable_raw_mode();
                })
                .context("could not put the terminal in raw mode for the session")?,
            )
        } else {
            None
        };

        let sizes = if self.tty {
            terminal_sizes()
        } else {
            stream::empty().boxed()
        };
        let mut resizer = attached.terminal_size();
        let status = attached.take_status();
        let remote = Remote {
            stdin: attached.stdin(),
            stdout: attached.stdout(),
            stderr: attached.stderr(),
        };

        let relayed = remote::relay(
            remote,
            async move {
                match status {
                    Some(status) => status.await,
                    None => None,
                }
            },
            Local {
                stdin: &mut stdio.stdin,
                stdout: &mut stdio.stdout,
                stderr: &mut stdio.stderr,
            },
            sizes,
            |size| {
                if let Some(resizer) = resizer.as_mut() {
                    // Full only if ten resizes are queued unsent, and then
                    // the next one supersedes this anyway.
                    let _ = resizer.try_send(size);
                }
            },
        );

        let outcome = tokio::select! {
            relayed = relayed => relayed,
            code = terminated(self.tty && self.watch_signals) => Ok(Ending::Exited(code)),
        };
        drop(raw);
        // `attached` outlives the relay on purpose: dropping it aborts the
        // task that carries the WebSocket.
        drop(attached);

        match outcome {
            Ok(ending) => Ok(ending),
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                Ok(Ending::Exited(BROKEN_PIPE))
            }
            Err(error) => Err(error).context("the session's input or output failed"),
        }
    }
}

/// Raw mode for as long as this lives.
///
/// Leaving is in `Drop`, so every way out of a session restores the terminal
/// — an error, an early return, the future being dropped by Ctrl-C's race in
/// `main` — not only the ones somebody remembered to write a line for. The
/// two halves are closures so that guarantee is tested without a terminal.
struct RawMode<F: FnMut()> {
    leave: F,
}

impl<F: FnMut()> RawMode<F> {
    fn enter(enter: impl FnOnce() -> io::Result<()>, leave: F) -> io::Result<Self> {
        enter()?;
        Ok(Self { leave })
    }
}

impl<F: FnMut()> Drop for RawMode<F> {
    fn drop(&mut self) {
        (self.leave)();
    }
}

/// The terminal's size now, and again each time it changes.
///
/// On Unix a change is `SIGWINCH`. Elsewhere there is no such signal, so the
/// size is looked at four times a second and sent when it differs.
fn terminal_sizes() -> BoxStream<'static, TerminalSize> {
    let initial = stream::iter(current_size());

    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::window_change()) {
            Ok(changes) => initial
                .chain(stream::unfold(changes, |mut changes| async move {
                    loop {
                        changes.recv().await?;
                        if let Some(size) = current_size() {
                            return Some((size, changes));
                        }
                    }
                }))
                .boxed(),
            Err(error) => {
                tracing::debug!(%error, "window resizes will not be forwarded");
                initial.boxed()
            }
        }
    }

    #[cfg(not(unix))]
    {
        let last = current_size().map(|size| (size.width, size.height));
        initial
            .chain(stream::unfold(last, |mut last| async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    let Some(size) = current_size() else { continue };
                    let now = Some((size.width, size.height));
                    if now != last {
                        last = now;
                        return Some((size, last));
                    }
                }
            }))
            .boxed()
    }
}

fn current_size() -> Option<TerminalSize> {
    let (width, height) = terminal::size().ok()?;
    (width > 0 && height > 0).then_some(TerminalSize { width, height })
}

/// Resolves with `128 + n` when the terminal session is ended from outside —
/// `SIGTERM`, or `SIGHUP` from a closed terminal window — so the session's
/// guard runs and the terminal is put back, instead of the process dying in
/// raw mode. Never resolves without a TTY: there is no mode to restore, and
/// the default disposition is the right one.
async fn terminated(tty: bool) -> u8 {
    #[cfg(unix)]
    if tty {
        use tokio::signal::unix::{SignalKind, signal};
        if let (Ok(mut term), Ok(mut hangup)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        ) {
            return tokio::select! {
                _ = term.recv() => 128 + 15,
                _ = hangup.recv() => 128 + 1,
            };
        }
    }
    #[cfg(not(unix))]
    let _ = tty;
    std::future::pending().await
}

/// Whether to ask for a TTY, decided from this process's real streams.
#[must_use]
pub fn wants_tty() -> bool {
    remote::wants_tty(io::stdin().is_terminal(), io::stdout().is_terminal())
}

// --- What is printed when a session cannot start ---------------------------

/// `-c` is `--context` in `eks`, where `kubectl exec` reads it as the
/// container. A context that does not resolve, typed on this command, was
/// most likely meant as one.
fn with_context_hint(error: &anyhow::Error, context_given: bool) -> anyhow::Error {
    if !context_given {
        return anyhow!("{error:#}");
    }
    anyhow!(
        "{error:#}\n\
         If that was meant as a container: in eks `-c` is `--context`; the container is `--container` (`-C`)."
    )
}

/// The cluster refused to start a session, or never answered.
///
/// Typed rather than a bare sentence so that a refused credential is still
/// one when the dashboard receives it, and `L` is offered for it there as
/// for any other failed request.
fn refused(
    error: &page::Error,
    cluster: &str,
    namespace: &str,
    pod: &str,
    surface: Surface,
) -> k8s::client::Error {
    let failure = Failure::of(error);
    let message = match (failure, upgrade_status(error)) {
        (Failure::Forbidden, _) => format!(
            "{cluster} will not let you run commands in pods in namespace {namespace}: \
             your access is missing the `create` verb on `pods/exec`.\n\
             Ask a cluster admin to grant it (the built-in `edit` and `admin` roles include it). \
             `kubectl auth can-i create pods/exec -n {namespace}` checks whether you have it."
        ),
        (_, Some(404)) => gone(pod, surface),
        _ => k8s::explain(error, cluster),
    };
    k8s::client::Error::Cluster { message, failure }
}

/// The pod went away between being chosen and being connected to.
fn gone(pod: &str, surface: Surface) -> String {
    let next = match surface {
        Surface::Command => "Run the same command again to find its successor.",
        Surface::Dashboard => "Press r to refresh the list, then pick its successor.",
    };
    format!(
        "pod {pod} was gone by the time eks connected to it — it was probably just replaced.\n{next}"
    )
}

/// The HTTP status a refused WebSocket upgrade came back with, if that is
/// what `error` is.
pub(crate) fn upgrade_status(error: &page::Error) -> Option<u16> {
    match error {
        page::Error::Api(kube::Error::UpgradeConnection(
            kube::client::UpgradeConnectionError::ProtocolSwitch(code),
        )) => Some(code.as_u16()),
        _ => None,
    }
}

/// Listing was refused, and the name given is not a full one.
fn unlistable(wanted: &str, namespace: &str) -> String {
    format!(
        "there is no pod called {wanted:?} in namespace {namespace}, and your access does not \
         include listing pods there, so eks cannot match the start of a name.\n\
         Give the pod's full name, or ask a cluster admin for `list` on pods in {namespace}."
    )
}

/// How many events to print under a pod that cannot run anything.
const EVENTS_SHOWN: usize = 5;

/// The pod, or the container, is not running, and what to look at next.
///
/// `events` is the pod's recent events, most recent first, when they could
/// be read: they are usually the whole answer to "why is it Pending", and
/// printing them saves a second command. `None` — the listing failed — and
/// an empty list both fall back to naming the command that shows them.
fn not_running(
    pod: &str,
    namespace: &str,
    why: &NotRunning,
    events: Option<&[EventRow]>,
    running: &[String],
    surface: Surface,
) -> String {
    let head = match why {
        NotRunning::Pod { phase, status } => {
            let status = status
                .as_deref()
                .map(|status| format!(" ({status})"))
                .unwrap_or_default();
            format!(
                "pod {pod} is {phase}{status}, so there is nothing running to open a session in."
            )
        }
        NotRunning::Container { container, reason } => {
            let reason = reason
                .as_deref()
                .map(|reason| format!(" ({reason})"))
                .unwrap_or_default();
            format!("container {container} in pod {pod} is not running{reason}.")
        }
    };

    let mut lines = vec![head];
    if let NotRunning::Container { container, .. } = why {
        lines.push(match surface {
            Surface::Command => format!(
                "`kubectl logs {pod} -n {namespace} -c {container} --previous` shows how its last run ended."
            ),
            Surface::Dashboard => {
                "Open its log and press p to see how its last run ended.".to_owned()
            }
        });
        if let Some(others) = crate::format::list(running, "or") {
            lines.push(match surface {
                Surface::Command => {
                    format!("Its running containers are {others}; pass one to `--container`.")
                }
                Surface::Dashboard => format!(
                    "Its running containers are {others}; press x on one of those in the pod's containers instead."
                ),
            });
        }
    }

    match events {
        Some(events) if !events.is_empty() => {
            lines.push("Recent events, newest first:".to_owned());
            lines.extend(events.iter().take(EVENTS_SHOWN).map(event_line));
        }
        _ => lines.push(match surface {
            Surface::Command => {
                format!("`kubectl describe pod {pod} -n {namespace}` shows its events.")
            }
            Surface::Dashboard => "The pod's containers pane lists its events.".to_owned(),
        }),
    }
    lines.join("\n")
}

/// One event as an indented line: when, how often, what.
fn event_line(event: &EventRow) -> String {
    let age = event.last_seen_age.as_deref().unwrap_or("-");
    let kind = if event.warning { "Warning" } else { "Normal" };
    let times = if event.count > 1 {
        format!(" (x{})", event.count)
    } else {
        String::new()
    };
    format!(
        "  {age:>4} ago  {kind:<7}  {reason}: {message}{times}",
        reason = event.reason,
        message = event.message,
    )
}

/// No shell in the image.
///
/// The dashboard's version spells out the namespace and container in the
/// `eks exec` line, which the command line already had from whatever the
/// user typed, so the line can be pasted into a shell as it is.
fn no_shell(pod: &str, namespace: &str, container: &str, os: Os, surface: Surface) -> String {
    let instead = match surface {
        Surface::Command => format!("eks exec {pod} -- <command>"),
        Surface::Dashboard => format!("eks exec {pod} -n {namespace} -C {container} -- <command>"),
    };
    match os {
        Os::Linux => format!(
            "container {container} in pod {pod} has no shell: neither /bin/bash nor /bin/sh is in its image, \
             as is usual for a distroless image.\n\
             eks cannot start a debug container yet. kubectl can, with a shell that shares the container's processes:\n\
             \x20 kubectl debug -it {pod} -n {namespace} --image=busybox --target={container}\n\
             Or run a command the image does have: `{instead}`."
        ),
        Os::Windows => format!(
            "container {container} in pod {pod} has no shell: cmd.exe is not in its image.\n\
             Run a command the image does have: `{instead}`."
        ),
    }
}

/// Why no container could be chosen, as the dashboard says it.
///
/// [`pick::ContainerError`]'s own wording names `--container`, which is
/// right for `eks exec`. In the dashboard the way to name a container is to
/// highlight it.
fn container_choice(error: &ContainerError, surface: Surface) -> String {
    match (error, surface) {
        (_, Surface::Command) | (ContainerError::Empty { .. }, _) => error.to_string(),
        (
            ContainerError::NotFound {
                pod,
                wanted,
                available,
            },
            Surface::Dashboard,
        ) => format!(
            "pod {pod} no longer has a container called {wanted}; its containers are {available}.\n\
             Press r to refresh the list, then pick one of those."
        ),
        (
            ContainerError::Unchosen {
                pod,
                count,
                available,
            },
            Surface::Dashboard,
        ) => format!(
            "pod {pod} has {count} containers and does not say which is the default: {available}.\n\
             Press enter to open its containers, then x on the one you want."
        ),
    }
}

/// The command asked for is not in the image.
fn command_missing(argv: &[String], pod: &str, container: &str) -> String {
    let program = argv.first().map(String::as_str).unwrap_or_default();
    format!(
        "{program} is not in container {container} of pod {pod}.\n\
         Check the spelling, or give its full path — the image may not include it at all."
    )
}

/// The session ended without a usable exit code.
fn session_failed(ending: &Ending, argv: &[String], pod: &str, container: &str) -> String {
    let command = argv.join(" ");
    match ending {
        Ending::Lost => format!(
            "the connection to pod {pod} closed before `{command}` reported how it ended.\n\
             The API server or the network went away mid-session; the command may still be running in {container}."
        ),
        Ending::Failed(message) => {
            format!("could not run `{command}` in container {container} of pod {pod}: {message}")
        }
        // Not reached: callers handle these first. Worded anyway, rather
        // than panicking in a library.
        Ending::Missing(_) => command_missing(argv, pod, container),
        Ending::Exited(code) => format!("`{command}` exited with code {code}."),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::cell::Cell;

    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    fn upgrade(code: u16) -> page::Error {
        page::Error::from(kube::Error::UpgradeConnection(
            kube::client::UpgradeConnectionError::ProtocolSwitch(
                http::StatusCode::from_u16(code).unwrap(),
            ),
        ))
    }

    // --- raw mode ---

    #[test]
    fn raw_mode_is_left_when_the_guard_goes_whatever_happened_in_between() {
        let left = Cell::new(0);
        let session = || -> Result<()> {
            let _raw = RawMode::enter(|| Ok(()), || left.set(left.get() + 1))?;
            bail!("the session failed partway")
        };

        assert!(session().is_err());
        assert_eq!(
            left.get(),
            1,
            "an error inside the session left raw mode on"
        );
    }

    #[test]
    fn raw_mode_that_could_not_be_entered_is_not_left() {
        let left = Cell::new(0);
        let entered = RawMode::enter(
            || Err(io::Error::other("not a terminal")),
            || left.set(left.get() + 1),
        );

        assert!(entered.is_err());
        assert_eq!(left.get(), 0);
    }

    // --- refusals ---

    #[test]
    fn a_forbidden_session_names_the_missing_verb_and_resource() {
        let message = refused(
            &upgrade(403),
            "prod (us-east-1)",
            "shop",
            "api-1",
            Surface::Command,
        )
        .to_string();
        assert!(message.contains("prod (us-east-1)"), "{message}");
        assert!(
            message.contains("`create` verb on `pods/exec`"),
            "{message}"
        );
        assert!(message.contains("namespace shop"), "{message}");
        assert!(
            message.contains("kubectl auth can-i create pods/exec -n shop"),
            "{message}"
        );
    }

    #[test]
    fn a_pod_gone_before_the_session_says_to_run_again() {
        let message = refused(&upgrade(404), "prod", "shop", "api-1", Surface::Command).to_string();
        assert!(message.starts_with("pod api-1 was gone"), "{message}");
        assert!(message.contains("again"), "{message}");
    }

    #[test]
    fn a_refused_credential_gets_the_usual_login_advice() {
        let message = refused(&upgrade(401), "prod", "shop", "api-1", Surface::Command).to_string();
        assert_eq!(message, k8s::explain(&upgrade(401), "prod"));
        assert!(message.contains("credentials"), "{message}");
    }

    #[test]
    fn a_slow_cluster_gets_the_usual_timeout_advice() {
        let error = page::Error::TimedOut {
            limit: std::time::Duration::from_secs(10),
        };
        let message = refused(&error, "prod", "shop", "api-1", Surface::Command).to_string();
        assert!(message.contains("--timeout"), "{message}");
    }

    #[test]
    fn a_context_that_did_not_resolve_mentions_kubectls_meaning_of_dash_c() {
        let error = anyhow!("no context matches \"app\"");
        let hinted = with_context_hint(&error, true).to_string();
        assert!(hinted.starts_with("no context matches \"app\""));
        assert!(hinted.contains("`--container` (`-C`)"), "{hinted}");

        let plain = with_context_hint(&error, false).to_string();
        assert_eq!(plain, "no context matches \"app\"");
    }

    #[test]
    fn a_name_that_cannot_be_listed_asks_for_the_full_one() {
        let message = unlistable("api", "shop");
        assert!(message.contains("no pod called \"api\" in namespace shop"));
        assert!(message.contains("full name"), "{message}");
    }

    // --- not running ---

    fn event(reason: &str, message: &str, warning: bool, count: i64, age: &str) -> EventRow {
        EventRow {
            reason: reason.to_owned(),
            message: message.to_owned(),
            warning,
            count,
            last_seen_age: Some(age.to_owned()),
            last_seen: None,
        }
    }

    #[test]
    fn a_pending_pod_prints_its_phase_status_and_recent_events() {
        let why = NotRunning::Pod {
            phase: "Pending".to_owned(),
            status: Some("ImagePullBackOff".to_owned()),
        };
        let events = [
            event("Failed", "Error: ImagePullBackOff", true, 12, "30s"),
            event(
                "Scheduled",
                "Successfully assigned shop/api-1",
                false,
                1,
                "5m",
            ),
        ];

        let message = not_running("api-1", "shop", &why, Some(&events), &[], Surface::Command);

        assert_eq!(
            message,
            "pod api-1 is Pending (ImagePullBackOff), so there is nothing running to open a session in.\n\
             Recent events, newest first:\n\
             \x20  30s ago  Warning  Failed: Error: ImagePullBackOff (x12)\n\
             \x20   5m ago  Normal   Scheduled: Successfully assigned shop/api-1"
        );
    }

    #[test]
    fn only_the_most_recent_events_are_printed() {
        let why = NotRunning::Pod {
            phase: "Pending".to_owned(),
            status: None,
        };
        let events: Vec<EventRow> = (0..9)
            .map(|n| event("BackOff", &format!("attempt {n}"), true, 1, "1m"))
            .collect();

        let message = not_running("api-1", "shop", &why, Some(&events), &[], Surface::Command);

        assert_eq!(
            message
                .lines()
                .filter(|line| line.contains("attempt"))
                .count(),
            EVENTS_SHOWN
        );
        assert!(message.contains("attempt 0") && !message.contains("attempt 5"));
    }

    #[test]
    fn without_events_to_print_it_names_the_command_that_shows_them() {
        let why = NotRunning::Pod {
            phase: "Succeeded".to_owned(),
            status: Some("Completed".to_owned()),
        };
        for events in [None, Some(&[][..])] {
            let message = not_running("job-1", "batch", &why, events, &[], Surface::Command);
            assert!(
                message.starts_with("pod job-1 is Succeeded (Completed)"),
                "{message}"
            );
            assert!(
                message.ends_with("`kubectl describe pod job-1 -n batch` shows its events."),
                "{message}"
            );
        }
    }

    #[test]
    fn a_crashing_container_points_at_its_last_log_and_its_running_siblings() {
        let why = NotRunning::Container {
            container: "app".to_owned(),
            reason: Some("CrashLoopBackOff".to_owned()),
        };

        let message = not_running(
            "api-1",
            "shop",
            &why,
            None,
            &["sidecar".to_owned(), "proxy".to_owned()],
            Surface::Command,
        );

        assert!(
            message.starts_with("container app in pod api-1 is not running (CrashLoopBackOff)."),
            "{message}"
        );
        assert!(
            message.contains("`kubectl logs api-1 -n shop -c app --previous`"),
            "{message}"
        );
        assert!(
            message.contains(
                "Its running containers are sidecar or proxy; pass one to `--container`."
            ),
            "{message}"
        );
    }

    #[test]
    fn a_crashing_only_container_suggests_no_siblings() {
        let why = NotRunning::Container {
            container: "app".to_owned(),
            reason: None,
        };
        let message = not_running("api-1", "shop", &why, None, &[], Surface::Command);
        assert!(message.starts_with("container app in pod api-1 is not running."));
        assert!(!message.contains("running containers"), "{message}");
    }

    // --- no shell, missing command, failed session ---

    #[test]
    fn a_distroless_image_points_at_an_ephemeral_debug_container() {
        let message = no_shell("api-1", "shop", "app", Os::Linux, Surface::Command);
        assert!(
            message.contains("neither /bin/bash nor /bin/sh"),
            "{message}"
        );
        assert!(message.contains("distroless"), "{message}");
        assert!(
            message.contains("kubectl debug -it api-1 -n shop --image=busybox --target=app"),
            "{message}"
        );
        assert!(
            message.contains("eks cannot start a debug container yet"),
            "{message}"
        );
    }

    #[test]
    fn a_windows_image_without_cmd_says_so_without_suggesting_busybox() {
        let message = no_shell("iis-1", "web", "iis", Os::Windows, Surface::Command);
        assert!(message.contains("cmd.exe is not in its image"), "{message}");
        assert!(!message.contains("busybox"), "{message}");
    }

    #[test]
    fn a_missing_command_is_named_with_where_it_was_looked_for() {
        let message = command_missing(&argv(&["htop", "-d", "5"]), "api-1", "app");
        assert!(message.starts_with("htop is not in container app of pod api-1."));
        assert!(message.contains("full path"), "{message}");
    }

    #[test]
    fn a_lost_connection_says_the_command_may_still_be_running() {
        let message = session_failed(&Ending::Lost, &argv(&["sleep", "60"]), "api-1", "app");
        assert!(message.contains("`sleep 60`"), "{message}");
        assert!(message.contains("may still be running in app"), "{message}");
    }

    #[test]
    fn any_other_failure_carries_the_api_servers_words() {
        let message = session_failed(
            &Ending::Failed("container not running (abc)".to_owned()),
            &argv(&["/bin/sh"]),
            "api-1",
            "app",
        );
        assert_eq!(
            message,
            "could not run `/bin/sh` in container app of pod api-1: container not running (abc)"
        );
    }

    // --- the dashboard's wording ---

    #[test]
    fn the_probe_attaches_no_input_and_no_terminal() {
        let params = probe_params("app");
        assert_eq!(params.container.as_deref(), Some("app"));
        assert!(!params.stdin, "a shell with input attached waits for it");
        assert!(!params.tty);
        // The API server refuses a session with no streams at all.
        assert!(params.stdout);
    }

    #[test]
    fn the_banner_says_where_the_shell_is_and_how_to_get_back() {
        let plan = Plan {
            namespace: "shop".to_owned(),
            pod: "api-1".to_owned(),
            container: "app".to_owned(),
            shell: argv(&["/bin/sh"]),
            os: Os::Linux,
        };
        assert_eq!(
            banner(&plan, "prod (us-east-1)"),
            "Shell in container app of pod api-1 (shop, prod (us-east-1)). \
             Exit the shell to return to the dashboard."
        );
    }

    #[test]
    fn in_the_dashboard_a_crashing_container_points_at_keys_not_kubectl() {
        let why = NotRunning::Container {
            container: "app".to_owned(),
            reason: Some("CrashLoopBackOff".to_owned()),
        };

        let message = not_running(
            "api-1",
            "shop",
            &why,
            None,
            &["sidecar".to_owned()],
            Surface::Dashboard,
        );

        assert_eq!(
            message,
            "container app in pod api-1 is not running (CrashLoopBackOff).\n\
             Open its log and press p to see how its last run ended.\n\
             Its running containers are sidecar; press x on one of those in the pod's containers instead.\n\
             The pod's containers pane lists its events."
        );
    }

    #[test]
    fn in_the_dashboard_a_pending_pod_still_prints_its_events() {
        let why = NotRunning::Pod {
            phase: "Pending".to_owned(),
            status: None,
        };
        let events = [event("FailedScheduling", "0/3 nodes", true, 1, "1m")];

        let message = not_running(
            "api-1",
            "shop",
            &why,
            Some(&events),
            &[],
            Surface::Dashboard,
        );

        assert!(message.contains("FailedScheduling: 0/3 nodes"), "{message}");
        assert!(!message.contains("kubectl"), "{message}");
    }

    #[test]
    fn in_the_dashboard_no_shell_gives_an_eks_exec_line_that_pastes_as_it_is() {
        let message = no_shell("api-1", "shop", "app", Os::Linux, Surface::Dashboard);
        assert!(
            message.contains("`eks exec api-1 -n shop -C app -- <command>`"),
            "{message}"
        );
        assert!(
            message.contains("kubectl debug -it api-1 -n shop --image=busybox --target=app"),
            "{message}"
        );

        let windows = no_shell("iis-1", "web", "iis", Os::Windows, Surface::Dashboard);
        assert!(
            windows.contains("`eks exec iis-1 -n web -C iis -- <command>`"),
            "{windows}"
        );
    }

    #[test]
    fn in_the_dashboard_a_pod_gone_before_the_session_says_to_refresh() {
        let message =
            refused(&upgrade(404), "prod", "shop", "api-1", Surface::Dashboard).to_string();
        assert!(message.starts_with("pod api-1 was gone"), "{message}");
        assert!(message.contains("Press r to refresh"), "{message}");
        assert!(!message.contains("command again"), "{message}");
    }

    #[test]
    fn a_refused_session_keeps_its_classification_for_the_login_offer() {
        let refusal = refused(&upgrade(401), "prod", "shop", "api-1", Surface::Dashboard);
        assert!(
            matches!(
                refusal,
                k8s::client::Error::Cluster {
                    failure: Failure::Credentials,
                    ..
                }
            ),
            "{refusal:?}"
        );
    }

    #[test]
    fn in_the_dashboard_a_pod_without_a_default_container_says_to_pick_one() {
        let error = ContainerError::Unchosen {
            pod: "api-1".to_owned(),
            count: 2,
            available: "app or proxy".to_owned(),
        };

        let message = container_choice(&error, Surface::Dashboard);

        assert_eq!(
            message,
            "pod api-1 has 2 containers and does not say which is the default: app or proxy.\n\
             Press enter to open its containers, then x on the one you want."
        );
        assert_eq!(
            container_choice(&error, Surface::Command),
            error.to_string()
        );
    }

    #[test]
    fn in_the_dashboard_a_container_that_has_gone_says_to_refresh() {
        let error = ContainerError::NotFound {
            pod: "api-1".to_owned(),
            wanted: "debugger-x7".to_owned(),
            available: "app".to_owned(),
        };

        let message = container_choice(&error, Surface::Dashboard);

        assert!(
            message.starts_with("pod api-1 no longer has a container called debugger-x7"),
            "{message}"
        );
        assert!(message.contains("Press r to refresh"), "{message}");
        assert!(!message.contains("--container"), "{message}");
    }

    #[test]
    fn a_pod_with_no_containers_reads_the_same_on_both_surfaces() {
        let error = ContainerError::Empty {
            pod: "api-1".to_owned(),
        };
        assert_eq!(
            container_choice(&error, Surface::Dashboard),
            container_choice(&error, Surface::Command)
        );
    }
}
