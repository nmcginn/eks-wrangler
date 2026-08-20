//! Binary entrypoint.
//!
//! Deliberately thin: parse, set up logging, dispatch, and turn errors into an
//! exit code. Everything worth testing lives in the library.

use std::io::IsTerminal;
use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::Parser;
use ratatui::crossterm::terminal;
use tracing_subscriber::EnvFilter;

use eks::cli::{Cli, Command, GlobalArgs};
use eks::commands::{self, contexts, nodes, pods};
use eks::format::Width;
use eks::k8s::order::Direction;
use eks::kubeconfig::KubeConfig;
use eks::ui::{self, App};

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
        Command::Dashboard => dashboard(&config, cli.global.context.as_deref()),
        Command::Contexts { quiet } => {
            print_line(&contexts::list(&config, quiet));
            Ok(())
        }
        Command::Nodes {
            sort,
            sort_reverse,
            wide,
        } => {
            // The only command so far that needs a runtime; it builds one for
            // itself so the filesystem-only commands stay as cheap as they are.
            let output = commands::block_on(nodes::list(
                &config,
                &paths,
                cli.global.context.as_deref(),
                sort,
                Direction::reversed(sort_reverse),
                Width::for_terminal(wide, stdout_terminal_cols()),
                cli.global.timeout,
            ))?;
            print_line(&output);
            Ok(())
        }
        Command::Pods {
            all_namespaces,
            selector,
            field_selector,
            sort,
            sort_reverse,
            wide,
        } => {
            let output = commands::block_on(pods::list(
                &config,
                &paths,
                cli.global.context.as_deref(),
                pods::Request {
                    namespace: cli.global.namespace.as_deref(),
                    all_namespaces,
                    label_selector: selector.as_deref(),
                    field_selector: field_selector.as_deref(),
                    order: sort,
                    direction: Direction::reversed(sort_reverse),
                    // `Width::Narrow` is treated as `Default` by the pod table
                    // — no drop rule for it exists yet — so the pods listing
                    // is unchanged and does not silently lose columns on a
                    // small terminal. When it grows one, this line will not
                    // need to change.
                    width: Width::for_terminal(wide, stdout_terminal_cols()),
                    budget: cli.global.timeout,
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

fn dashboard(config: &KubeConfig, requested_context: Option<&str>) -> Result<()> {
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

    ui::run(app)
}

/// Print a command's output, skipping the newline for empty results so
/// `eks contexts -q` on an empty config pipes cleanly.
fn print_line(output: &str) {
    if !output.is_empty() {
        println!("{output}");
    }
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
/// drop lives inside the node listing and is tested there without an ioctl
/// in sight.
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
