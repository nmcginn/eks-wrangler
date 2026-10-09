//! `eks logs`: a container's log, from the cluster while the pod is there and
//! from CloudWatch once it is not.
//!
//! The pod and container are chosen by `eks exec`'s rules
//! (`exec::find`, [`pick::container`]). A running pod's log is read from
//! the API server as `kubectl logs` reads it. A prefix that matches no
//! running pod is looked for in the Container Insights group instead
//! ([`insights`]), which still holds the lines of a pod that was deleted or
//! rescheduled. Every line from there carries a `[cloudwatch …]` label, and a
//! note on stderr says which pod it was and where its lines come from, so a
//! reader always knows which of the two they are looking at.
//!
//! Every decision is a pure function in [`insights`], [`pick`], or
//! [`k8s_logs`]; this module is the I/O between them.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow, bail};
use futures_util::{AsyncBufReadExt, StreamExt};
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::jiff::Timestamp;
use kube::Client;
use kube::api::Api;

use crate::aws::LoginMode;
use crate::aws::cli::{Failure, Surface};
use crate::aws::eks::Target;
use crate::aws::insights::{self, Found, Record, Search, Wanted};
use crate::aws::logs::{self, Event, Since, Tail};
use crate::cluster::ClusterView;
use crate::commands::cloudwatch::{self, Aws, passing, quiet_on_closed_pipe};
use crate::commands::credentials::{self, AwsLogin};
use crate::commands::exec::{self, Located, with_context_hint};
use crate::commands::nodes::target_cluster;
use crate::commands::pods::selectors_for;
use crate::k8s;
use crate::k8s::client;
use crate::k8s::page::Budget;
use crate::k8s::pods::logs as k8s_logs;
use crate::k8s::pods::pick;
use crate::kubeconfig::KubeConfig;
use crate::progress::Progress;
use crate::theme::Palette;

/// Everything `eks logs` was asked for.
#[derive(Debug)]
pub struct Request<'a> {
    /// The pod: a full name, or the start of exactly one.
    pub pod: &'a str,
    /// `--container`. Without one, the pod's default container.
    pub container: Option<&'a str>,
    /// `--previous`: the container instance before the current one.
    pub previous: bool,
    /// `--since`. Unset reads every line the kubelet kept, and the last
    /// [`Since::default`] of CloudWatch.
    pub since: Option<Since>,
    /// `--follow`.
    pub follow: bool,
    /// `--namespace`, or the config file's. Without either, the context's.
    pub namespace: Option<&'a str>,
    /// `-l`, unparsed. Narrows the running pods a prefix is matched against.
    pub label_selector: Option<&'a str>,
    /// `--field-selector`, unparsed.
    pub field_selector: Option<&'a str>,
    /// `log_group` from the config file.
    pub log_group: Option<&'a str>,
    pub palette: Palette,
    pub budget: Budget,
    pub login: LoginMode,
    pub progress: Progress,
}

/// Where the command writes: the log to `out`, everything said about it to
/// `notes`, so a pipe carries the container's lines and nothing else.
pub struct Sinks<'a> {
    pub out: &'a mut dyn Write,
    pub notes: &'a mut dyn Write,
}

impl std::fmt::Debug for Sinks<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sinks")
    }
}

impl Sinks<'_> {
    /// One line to stdout. `false` once the reader has gone (`| head`).
    fn line(&mut self, text: &str) -> Result<bool> {
        match writeln!(self.out, "{text}") {
            Ok(()) => Ok(true),
            Err(error) => quiet_on_closed_pipe(error),
        }
    }

    fn flush(&mut self) -> Result<bool> {
        match self.out.flush() {
            Ok(()) => Ok(true),
            Err(error) => quiet_on_closed_pipe(error),
        }
    }

    /// A sentence about the output, on the notes stream. One that cannot be
    /// written is not worth failing over: the lines are what was asked for.
    fn note(&mut self, text: &str) {
        let _ = writeln!(self.notes, "{text}");
    }
}

/// What `--follow` with `--previous` is told: the instance has stopped.
const PREVIOUS_DOES_NOT_FOLLOW: &str = "--follow does nothing with --previous: that instance has stopped, so its log is printed \
     and eks exits.";

/// Run the command until the log is printed or, with `--follow`, until it is
/// stopped.
pub async fn run(
    config: &KubeConfig,
    paths: &[PathBuf],
    context: Option<&str>,
    request: Request<'_>,
    mut sinks: Sinks<'_>,
) -> Result<()> {
    let view = target_cluster(config, context)
        .map_err(|error| with_context_hint(&error, context.is_some()))?;
    let selectors = selectors_for(request.label_selector, request.field_selector)?;
    let namespace = request
        .namespace
        .map_or_else(|| view.namespace.clone(), ToOwned::to_owned);

    let client = credentials::connect(
        paths,
        &view,
        request.budget,
        request.login,
        &Progress::none(),
    )
    .await?;

    if request.follow && request.previous {
        sinks.note(PREVIOUS_DOES_NOT_FOLLOW);
    }

    match exec::find(
        &client,
        &view,
        &namespace,
        request.pod,
        &selectors,
        request.budget,
    )
    .await?
    {
        Located::Live(pod) => live(&client, &view, &namespace, &pod, &request, &mut sinks).await,
        Located::Gone { elsewhere, .. } => {
            let elsewhere: Vec<String> = elsewhere
                .iter()
                .map(|pod| {
                    format!(
                        "{}/{}",
                        pod.metadata.namespace.as_deref().unwrap_or_default(),
                        pod.metadata.name.as_deref().unwrap_or_default()
                    )
                })
                .collect();
            let gone = Gone {
                namespace: &namespace,
                elsewhere: &elsewhere,
                selected: selectors.label.is_some() || selectors.field.is_some(),
            };
            from_cloudwatch(config, paths, &view, &gone, &request, &mut sinks).await
        }
    }
}

/// A running pod's log, from the API server.
async fn live(
    client: &Client,
    view: &ClusterView,
    namespace: &str,
    pod: &Pod,
    request: &Request<'_>,
    sinks: &mut Sinks<'_>,
) -> Result<()> {
    let name = pod.metadata.name.clone().unwrap_or_default();
    let container = pick::container(pod, request.container)?;
    if request.previous && pick::restarts(pod, &container) == Some(0) {
        bail!(
            "{container} in pod {name} has not restarted, so it has no previous log.\n\
             Drop `--previous` to read the instance that is running."
        );
    }

    let api: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let params =
        k8s_logs::command_params(&container, request.previous, request.follow, request.since);
    let stream = request
        .budget
        .wrap(api.log_stream(&name, &params))
        .await
        .map_err(|error| anyhow!(k8s::explain(&error, &view.label())))?;

    let mut lines = stream.lines();
    while let Some(line) = lines.next().await {
        let line = line.with_context(|| format!("the log stream for {container} broke"))?;
        if !sinks.line(&line)? {
            return Ok(());
        }
        // Each line as it comes: a followed log is read by a person waiting
        // for the next one, and stdout to a pipe is block-buffered.
        if !sinks.flush()? {
            return Ok(());
        }
    }
    Ok(())
}

/// What the live search left behind for CloudWatch.
struct Gone<'a> {
    namespace: &'a str,
    /// Running pods elsewhere that start with the prefix, `namespace/name`.
    elsewhere: &'a [String],
    selected: bool,
}

/// A pod that is not running, from Container Insights.
async fn from_cloudwatch(
    config: &KubeConfig,
    paths: &[PathBuf],
    view: &ClusterView,
    gone: &Gone<'_>,
    request: &Request<'_>,
    sinks: &mut Sinks<'_>,
) -> Result<()> {
    let wanted = request.pod;
    let namespace = gone.namespace;
    let looked = || {
        format!(
            "no pod in namespace {namespace} is called {wanted:?} or starts with it, so eks \
             looked in CloudWatch"
        )
    };
    let Some(pattern) = insights::pattern(namespace, wanted) else {
        bail!(
            "no pod in namespace {namespace} is called {wanted:?} or starts with it, and \
             {wanted:?} cannot be the start of a pod's name (lower-case letters, digits, `-`, \
             and `.`), so eks did not look in CloudWatch.\n\
             Run `eks pods -n {namespace}` to see what is there."
        );
    };

    let label = view.label();
    let entry = config
        .resolved_contexts()
        .into_iter()
        .find(|resolved| resolved.name == view.context_name)
        .map(|resolved| resolved.cluster_name)
        .unwrap_or_default();
    let resolved = client::resolve(paths, view).await?;
    let target = Target::of(
        &view.context_name,
        &entry,
        view.region.as_deref(),
        &resolved.auth_info,
    )
    .with_context(looked)?;
    let group = insights::group(request.log_group, &target.cluster);

    let mut aws = Aws {
        target: &target,
        login: AwsLogin::before(&resolved, &label, request.login)?,
        budget: request.budget,
        surface: Surface::Command,
    };

    let since = request.since.unwrap_or_default();
    let start = logs::millis(since.start(Timestamp::now()));
    let read = Read {
        group: &group,
        pattern: &pattern,
        overridden: request.log_group.is_some(),
        label: &label,
    };
    let pages = read
        .window(&mut aws, &request.progress, start)
        .await
        .map_err(|error| error.context(looked()))?;
    let mut tail = Tail::new(start);
    let events = tail.admit(pages.events);
    let records: Vec<Record> = events.iter().filter_map(insights::record).collect();

    let wanted = Wanted {
        pod: request.pod,
        container: request.container,
        previous: request.previous,
    };
    let found = insights::resolve(records, wanted).map_err(|why| {
        anyhow!(insights::explain(
            &why,
            &Search {
                wanted,
                namespace,
                since,
                group: &group,
                elsewhere: gone.elsewhere,
                selected: gone.selected,
            },
            Timestamp::now(),
        ))
    })?;

    sinks.note(&insights::reading_note(
        request.pod,
        namespace,
        &found,
        &group,
        since,
        request.since.is_some(),
    ));
    if pages.stalled {
        sinks.note(cloudwatch::STALLED);
    }
    if !print(sinks, &found.lines, request.palette)? {
        return Ok(());
    }
    if request.follow && !request.previous {
        return follow(&mut aws, &read, &found, tail, request.palette, sinks).await;
    }
    Ok(())
}

/// One read of the group: where, what for, and how to explain its absence.
struct Read<'a> {
    group: &'a str,
    pattern: &'a str,
    /// The group came from `log_group` in the config file.
    overridden: bool,
    label: &'a str,
}

impl Read<'_> {
    /// Every page from `start` to now. A group that does not exist is
    /// Container Insights not being set up, and is said so.
    async fn window(
        &self,
        aws: &mut Aws<'_>,
        progress: &Progress,
        start: i64,
    ) -> Result<cloudwatch::Pages> {
        let target = aws.target;
        let read = cloudwatch::read_pages(
            aws,
            progress,
            "container log lines",
            |token| insights::filter_events(target, self.group, self.pattern, start, token),
            |_: &Event| true,
        )
        .await;
        match read {
            Err(error) if matches!(cloudwatch::failure(&error), Some(Failure::NotFound { .. })) => {
                Err(anyhow!(insights::not_set_up(
                    self.group,
                    self.overridden,
                    target,
                    self.label,
                )))
            }
            other => other,
        }
    }
}

/// Print CloudWatch's lines. `false` once the reader has gone.
fn print(sinks: &mut Sinks<'_>, lines: &[Record], palette: Palette) -> Result<bool> {
    for record in lines {
        if !sinks.line(&insights::line(record, palette))? {
            return Ok(false);
        }
    }
    sinks.flush()
}

/// Poll for lines still on their way: a pod's last few seconds reach
/// CloudWatch after the pod has gone. As `eks control-plane-logs --follow`
/// does: every [`logs::POLL`] from just before the newest line, a failure
/// that passes reported once and retried.
async fn follow(
    aws: &mut Aws<'_>,
    read: &Read<'_>,
    found: &Found,
    mut tail: Tail,
    palette: Palette,
    sinks: &mut Sinks<'_>,
) -> Result<()> {
    sinks.note(&format!(
        "Following {}'s {} lines in CloudWatch; the pod is gone, so only lines still on their \
         way will arrive. Ctrl-C to stop.",
        found.pod, found.container,
    ));
    let mut failing = false;
    loop {
        tokio::time::sleep(logs::POLL).await;
        match read.window(aws, &Progress::none(), tail.start()).await {
            Ok(pages) => {
                if std::mem::take(&mut failing) {
                    sinks.note("Reading again.");
                }
                let lines: Vec<Record> = tail
                    .admit(pages.events)
                    .iter()
                    .filter_map(insights::record)
                    .filter(|record| found.admits(record))
                    .collect();
                if !print(sinks, &lines, palette)? {
                    return Ok(());
                }
            }
            Err(error) if passing(&error) => {
                if !failing {
                    sinks.note(&cloudwatch::retrying(&error));
                    failing = true;
                }
            }
            Err(error) => return Err(error),
        }
    }
}
