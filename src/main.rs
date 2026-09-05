//! Binary entrypoint.
//!
//! Deliberately thin: parse, set up logging, dispatch, and turn errors into an
//! exit code. Everything worth testing lives in the library.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::Parser;
use ratatui::crossterm::terminal;
use tracing_subscriber::EnvFilter;

use eks::aws::LoginMode;
use eks::cli::{Cli, Command, GlobalArgs};
use eks::commands::{self, contexts, credentials, nodes, pods};
use eks::format::Width;
use eks::k8s::order::Direction;
use eks::k8s::page::Budget;
use eks::k8s::pods::Selectors;
use eks::kubeconfig::KubeConfig;
use eks::theme::{ColourChoice, Palette};
use eks::ui::{self, App, RefreshInterval};

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(&cli.global);

    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // `{:#}` prints the whole anyhow context chain on one line, which is
            // what a CLI user wants; backtraces stay behind RUST_BACKTRACE.
            eprintln!("eks: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let paths = cli.global.kubeconfig_paths()?;
    let config = KubeConfig::load_from(&paths)?;

    match cli.command.unwrap_or(Command::Dashboard) {
        Command::Dashboard => {
            // Validated here, before the terminal takes over, exactly as
            // `pods::list` validates the same flags: a malformed `-l` should be
            // a sentence naming the bad text, not a dashboard that opens and
            // then can never load a node's pods.
            let selectors = pods::selectors_for(
                cli.global.selector.as_deref(),
                cli.global.field_selector.as_deref(),
            )?;
            dashboard(
                &config,
                &paths,
                cli.global.context.as_deref(),
                cli.global.timeout,
                cli.global.refresh,
                &selectors,
                cli.global.login,
            )
        }
        Command::Contexts { quiet } => {
            print_line(&contexts::list(
                &config,
                quiet,
                stdout_palette(cli.global.color),
            ));
            Ok(())
        }
        Command::Nodes {
            sort,
            sort_reverse,
            sort_resource,
            wide,
        } => {
            // The only command so far that needs a runtime; it builds one for
            // itself so the filesystem-only commands stay as cheap as they are.
            let output = commands::block_on(nodes::list(
                &config,
                &paths,
                cli.global.context.as_deref(),
                nodes::Request {
                    order: sort,
                    direction: Direction::reversed(sort_reverse),
                    resource: sort_resource,
                    width: Width::for_terminal(wide, stdout_terminal_cols()),
                    palette: stdout_palette(cli.global.color),
                    budget: cli.global.timeout,
                    login: cli.global.login,
                },
            ))?;
            print_line(&output);
            Ok(())
        }
        Command::Pods {
            all_namespaces,
            sort,
            sort_reverse,
            sort_resource,
            wide,
        } => {
            let output = commands::block_on(pods::list(
                &config,
                &paths,
                cli.global.context.as_deref(),
                pods::Request {
                    namespace: cli.global.namespace.as_deref(),
                    all_namespaces,
                    label_selector: cli.global.selector.as_deref(),
                    field_selector: cli.global.field_selector.as_deref(),
                    order: sort,
                    direction: Direction::reversed(sort_reverse),
                    resource: sort_resource.as_deref(),
                    width: Width::for_terminal(wide, stdout_terminal_cols()),
                    palette: stdout_palette(cli.global.color),
                    budget: cli.global.timeout,
                    login: cli.global.login,
                },
            ))?;
            print_line(&output);
            Ok(())
        }
        Command::Use { name } => {
            print_line(&contexts::switch(&config, &name)?);
            Ok(())
        }
        Command::Current => {
            print_line(&contexts::current(&config)?);
            Ok(())
        }
    }
}

fn dashboard(
    config: &KubeConfig,
    paths: &[PathBuf],
    requested_context: Option<&str>,
    budget: Budget,
    refresh: RefreshInterval,
    selectors: &Selectors,
    login: LoginMode,
) -> Result<()> {
    let views = contexts::views(config);
    let mut app = App::new(views);

    if let Some(name) = requested_context {
        // Resolve through the same selector logic as `eks use`, so `--context`
        // accepts short cluster names too.
        let target = contexts::resolve_selector(app.clusters(), name)?;
        let context_name = target.context_name.clone();
        if !app.select_context(&context_name) {
            bail!("context {name:?} disappeared while starting up");
        }
    }

    // The dashboard's one chance to ask a question with an ordinary terminal in
    // front of it. Every fetch after this point runs on a background thread and
    // draws onto an alternate screen, so a login offered from there would be
    // shouting over the pane it was trying to fill; `L` on the failure banner is
    // how a session that dies later gets refreshed instead.
    //
    // Skipped outright under `--login never`, so that flag costs nothing at all
    // — not even the runtime this would otherwise build before first paint.
    if !matches!(login, LoginMode::Never)
        && let Some(cluster) = app.selected_cluster().cloned()
    {
        commands::block_on(credentials::preflight(paths, &cluster, login))?;
    }

    // Every fetch after this one — `r`, the refresh interval, a different
    // cluster selected in the sidebar — goes through this closure too, so
    // `ui::run` never has to know how a `NodesFetch` is built, only how to
    // ask for one.
    let spawn_nodes: ui::NodesFetcher = {
        let config = config.clone();
        let paths = paths.to_vec();
        Box::new(move |context: &str| {
            nodes::spawn_gather(
                config.clone(),
                paths.clone(),
                Some(context.to_owned()),
                budget,
            )
        })
    };

    // The pod-browsing pane's counterpart: called once each time drilling
    // into a node changes which one the detail pane is showing. `selectors`
    // is fixed for the life of the session — set from `-l`/`--field-selector`
    // at startup, the same flags `eks pods` reads — so every node's pods are
    // filtered by the one the user typed rather than the pane growing its own.
    let spawn_pods: ui::PodsFetcher = {
        let config = config.clone();
        let paths = paths.to_vec();
        let selectors = selectors.clone();
        Box::new(move |context: &str, node: &str| {
            pods::spawn_gather_for_node(
                config.clone(),
                paths.clone(),
                Some(context.to_owned()),
                node.to_owned(),
                selectors.clone(),
                budget,
            )
        })
    };

    // One level further in: called once each time drilling into a pod
    // changes which one the detail pane is showing. No selectors to carry
    // through here — a single named `get` is not a listing, so there is
    // nothing for `-l`/`--field-selector` to filter.
    let spawn_containers: ui::ContainersFetcher = {
        let config = config.clone();
        let paths = paths.to_vec();
        Box::new(move |context: &str, namespace: &str, pod: &str| {
            pods::spawn_gather_containers(
                config.clone(),
                paths.clone(),
                Some(context.to_owned()),
                namespace.to_owned(),
                pod.to_owned(),
                budget,
            )
        })
    };

    // The deepest level: called once each time drilling into a container
    // changes which one the detail pane is showing, and again whenever `p`
    // switches it between that container's current log and its previous
    // instance's. `budget` still bounds connecting and opening the log, but
    // not the `follow`ed read after that — see `commands::pods::stream_logs`.
    let spawn_logs: ui::LogsFetcher = {
        let config = config.clone();
        let paths = paths.to_vec();
        Box::new(
            move |context: &str, namespace: &str, pod: &str, container: &str, previous: bool| {
                pods::spawn_stream_logs(
                    config.clone(),
                    paths.clone(),
                    Some(context.to_owned()),
                    pods::LogRequest {
                        namespace: namespace.to_owned(),
                        pod: pod.to_owned(),
                        container: container.to_owned(),
                        previous,
                    },
                    budget,
                )
            },
        )
    };

    // Kicked off before the terminal takes over, so the fetch is already in
    // flight for the first frame — the loading state a bare `eks` shows is
    // real, not simulated. No cluster selected, nothing to fetch: the empty
    // kubeconfig message `draw_detail` prints does not depend on node state.
    let nodes_rx = app
        .selected_cluster()
        .map(|cluster| spawn_nodes(&cluster.context_name));

    let drill = ui::DrillFetchers {
        spawn_pods: &spawn_pods,
        spawn_containers: &spawn_containers,
        spawn_logs: &spawn_logs,
    };

    // What `L` runs. Unlike the four fetchers above it does not spawn a thread:
    // it blocks, because it owns the terminal `ui::run` hands back to it, and a
    // browser login is a thing to wait for rather than to poll.
    //
    // `retry_login`, not `preflight`: the key only appears once a background
    // fetch has already been refused, so the token cache `preflight` would
    // re-read has already been proven stale — consulting it again would leave
    // `L` doing nothing whenever the cache still called the dead session
    // valid. `retry_login` always logs in without asking and reports plainly
    // when there was nothing an Identity Center login could have fixed,
    // rather than the two of those reading as the identical "flash and
    // return" a bare `Ok(())` for both would give the event loop.
    let login: ui::LoginRunner = {
        let config = config.clone();
        let paths = paths.to_vec();
        Box::new(move |context: &str| {
            let views = contexts::views(&config);
            let cluster = contexts::resolve_selector(&views, context)
                .map_err(|error| format!("{error:#}"))?;
            commands::block_on(credentials::retry_login(&paths, cluster))
                .map_err(|error| format!("{error:#}"))
        })
    };

    ui::run(app, nodes_rx, &spawn_nodes, &drill, refresh, &login)
}

/// Print a command's output, skipping the newline for empty results so
/// `eks contexts -q` on an empty config pipes cleanly.
fn print_line(output: &str) {
    if !output.is_empty() {
        println!("{output}");
    }
}

/// Whether this run prints colour, and in what.
///
/// The three impure answers `Palette::choose` needs, gathered in one place:
/// whether stdout is a terminal, and what `NO_COLOR` and `TERM` say. Stdout
/// specifically, and not stderr — colour goes into the table, and the table is
/// what a pipe carries away. The rule itself, including which of the three
/// wins, is `theme::Palette`'s and is tested there without an environment to
/// set up.
///
/// `OsStr` rather than `String`: an environment variable that is not valid
/// UTF-8 is still set, and `NO_COLOR=<invalid>` must turn colour off rather
/// than be dropped as unreadable.
fn stdout_palette(choice: ColourChoice) -> Palette {
    Palette::choose(
        choice,
        std::io::stdout().is_terminal(),
        std::env::var_os("NO_COLOR").as_deref(),
        std::env::var_os("TERM").as_deref(),
    )
}

/// The terminal's column count where stdout is one, `None` otherwise.
///
/// Two questions and two answers, in that order: the terminal-size ioctl on
/// stdout when stdout is a pipe returns the *stderr* terminal's width, which
/// would narrow a piped listing based on the terminal that ran it and break
/// the `eks nodes | grep foo` pipeline every script depends on. So the
/// `IsTerminal` check has to come first, and a failed `size()` — no
/// controlling terminal, an ioctl the platform does not support — also
/// returns `None`: better an unnarrowed listing than a wrong one.
///
/// Impure and small on purpose; the arithmetic that decides which columns
/// drop lives inside each listing — `k8s::nodes` and `k8s::pods::row` — and is
/// tested there without an ioctl in sight.
fn stdout_terminal_cols() -> Option<u16> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    terminal::size().ok().map(|(cols, _rows)| cols)
}

fn init_tracing(global: &GlobalArgs) {
    // RUST_LOG wins when set; otherwise -v decides. Logs go to stderr so they
    // never corrupt piped stdout or the TUI.
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(global.log_filter()));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}
