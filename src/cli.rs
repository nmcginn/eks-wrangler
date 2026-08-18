//! The command-line surface.
//!
//! Keeping the parser separate from the commands themselves means argument
//! handling can be tested without touching a cluster.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Explore and interact with AWS EKS clusters.
#[derive(Debug, Parser)]
#[command(
    name = "eks",
    version,
    about,
    long_about = None,
    propagate_version = true,
    // Running `eks` bare should drop you straight into the dashboard; the
    // subcommands are for scripting and quick one-shot answers.
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[command(flatten)]
    pub global: GlobalArgs,
}

/// Options that apply to every subcommand.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct GlobalArgs {
    /// Use this kubeconfig context instead of the current one.
    #[arg(long, short = 'c', global = true, value_name = "NAME")]
    pub context: Option<String>,

    /// Namespace to scope resources to.
    #[arg(long, short = 'n', global = true, value_name = "NAMESPACE")]
    pub namespace: Option<String>,

    /// Path to a kubeconfig file (overrides KUBECONFIG).
    #[arg(long, global = true, value_name = "PATH", env = "KUBECONFIG")]
    pub kubeconfig: Option<PathBuf>,

    /// Increase log verbosity. Repeat for more detail.
    #[arg(long, short = 'v', global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

impl GlobalArgs {
    /// Tracing filter implied by the `-v` count.
    ///
    /// `kube` logs every failed request at ERROR, and we translate those same
    /// failures into a sentence that says what to do about it. Printing both
    /// means the user reads the unhelpful one first, so the quiet default
    /// silences `kube`'s copy; `-v` brings it back for debugging.
    #[must_use]
    pub fn log_filter(&self) -> &'static str {
        match self.verbose {
            0 => "warn,kube_client=off",
            1 => "info",
            2 => "debug",
            _ => "trace",
        }
    }

    /// Kubeconfig files to read, honouring `--kubeconfig` before the
    /// environment's own search path.
    pub fn kubeconfig_paths(&self) -> Result<Vec<PathBuf>, crate::kubeconfig::Error> {
        match &self.kubeconfig {
            // KUBECONFIG may itself be a path list; clap hands it to us whole.
            Some(path) => Ok(std::env::split_paths(path)
                .filter(|p| !p.as_os_str().is_empty())
                .collect()),
            None => crate::kubeconfig::search_paths(),
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open the interactive dashboard (the default when run with no arguments).
    #[command(visible_alias = "dash")]
    Dashboard,

    /// List the clusters available in your kubeconfig.
    #[command(visible_aliases = ["ctx", "clusters"])]
    Contexts {
        /// Print only context names, one per line, for scripting.
        #[arg(long, short = 'q')]
        quiet: bool,
    },

    /// List the nodes of a cluster.
    #[command(visible_alias = "no")]
    Nodes,

    /// List the pods of a namespace, or of every namespace.
    #[command(visible_alias = "po")]
    Pods {
        /// List pods in every namespace, adding a NAMESPACE column.
        #[arg(long, short = 'A')]
        all_namespaces: bool,
    },

    /// Switch the active cluster.
    Use {
        /// Context name, as shown by `eks contexts`.
        name: String,
    },

    /// Print the active cluster.
    Current,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap()
    }

    #[test]
    fn cli_definition_is_valid() {
        // Catches conflicting flags and malformed arg definitions at test time
        // rather than on the user's first run.
        Cli::command().debug_assert();
    }

    #[test]
    fn bare_invocation_has_no_subcommand() {
        assert!(parse(&["eks"]).command.is_none());
    }

    #[test]
    fn contexts_has_short_aliases() {
        for alias in ["contexts", "ctx", "clusters"] {
            assert!(
                matches!(
                    parse(&["eks", alias]).command,
                    Some(Command::Contexts { .. })
                ),
                "alias {alias} did not resolve to Contexts"
            );
        }
    }

    #[test]
    fn nodes_accepts_a_context_and_its_short_alias() {
        assert!(matches!(
            parse(&["eks", "no"]).command,
            Some(Command::Nodes)
        ));

        let cli = parse(&["eks", "nodes", "--context", "prod"]);
        assert!(matches!(cli.command, Some(Command::Nodes)));
        assert_eq!(cli.global.context.as_deref(), Some("prod"));
    }

    #[test]
    fn use_requires_a_context_name() {
        assert!(Cli::try_parse_from(["eks", "use"]).is_err());
        let cli = parse(&["eks", "use", "staging"]);
        assert!(matches!(cli.command, Some(Command::Use { name }) if name == "staging"));
    }

    #[test]
    fn global_flags_are_accepted_after_the_subcommand() {
        let cli = parse(&["eks", "contexts", "--namespace", "payments", "-vv"]);
        assert_eq!(cli.global.namespace.as_deref(), Some("payments"));
        assert_eq!(cli.global.verbose, 2);
    }

    #[test]
    fn the_quiet_default_hides_kubes_own_error_logging() {
        // Our message for a failed request is the one worth reading; `kube`'s
        // raw ERROR line above it is noise until someone asks for detail.
        let quiet = GlobalArgs::default();
        assert!(quiet.log_filter().contains("kube_client=off"));

        let verbose = GlobalArgs {
            verbose: 1,
            ..GlobalArgs::default()
        };
        assert!(!verbose.log_filter().contains("kube_client=off"));
    }

    #[test]
    fn verbosity_maps_to_log_filters() {
        let filter = |v| {
            GlobalArgs {
                verbose: v,
                ..GlobalArgs::default()
            }
            .log_filter()
        };

        assert_eq!(filter(0), "warn,kube_client=off");
        assert_eq!(filter(1), "info");
        assert_eq!(filter(2), "debug");
        assert_eq!(filter(9), "trace");
    }

    #[test]
    fn explicit_kubeconfig_flag_supports_a_path_list() {
        let joined = std::env::join_paths(["/a/config", "/b/config"]).unwrap();
        let args = GlobalArgs {
            kubeconfig: Some(PathBuf::from(joined)),
            ..GlobalArgs::default()
        };

        assert_eq!(
            args.kubeconfig_paths().unwrap(),
            vec![PathBuf::from("/a/config"), PathBuf::from("/b/config")]
        );
    }
}
