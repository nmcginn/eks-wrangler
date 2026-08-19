//! The command-line surface.
//!
//! Keeping the parser separate from the commands themselves means argument
//! handling can be tested without touching a cluster.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::k8s::nodes::Order as NodeOrder;
use crate::k8s::pods::Order as PodOrder;

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
    Nodes {
        /// Order the listing. Every order but `name` puts the most interesting
        /// row first: the least healthy node, the fullest, the youngest.
        #[arg(long, value_name = "ORDER", default_value = "name")]
        sort: NodeOrder,

        /// Reverse the order. Nodes there is nothing to rank — no live usage,
        /// or no reported capacity — stay at the end either way.
        #[arg(long)]
        sort_reverse: bool,
    },

    /// List the pods of a namespace, or of every namespace.
    #[command(visible_alias = "po")]
    Pods {
        /// List pods in every namespace, adding a NAMESPACE column.
        #[arg(long, short = 'A')]
        all_namespaces: bool,

        /// Select by label, e.g. `-l app=api,tier notin (canary)`.
        #[arg(long, short = 'l', value_name = "SELECTOR")]
        selector: Option<String>,

        /// Select by field, e.g. `--field-selector status.phase!=Running`.
        #[arg(long, value_name = "SELECTOR")]
        field_selector: Option<String>,

        /// Order the listing. Every order but `name` puts the most
        /// interesting row first: the newest restart, the youngest pod, the
        /// largest usage figure.
        #[arg(long, value_name = "ORDER", default_value = "name")]
        sort: PodOrder,

        /// Reverse the order. Pods there is nothing to rank — never restarted,
        /// or never sampled — stay at the end either way.
        #[arg(long)]
        sort_reverse: bool,
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
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

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
            Some(Command::Nodes { .. })
        ));

        let cli = parse(&["eks", "nodes", "--context", "prod"]);
        assert!(matches!(cli.command, Some(Command::Nodes { .. })));
        assert_eq!(cli.global.context.as_deref(), Some("prod"));
    }

    #[test]
    fn nodes_defaults_to_the_alphabetical_order() {
        // The listing people already have must not move under them because a
        // flag was added; `--sort` is opt-in for nodes exactly as it is for
        // pods.
        let Some(Command::Nodes { sort, sort_reverse }) = parse(&["eks", "nodes"]).command else {
            panic!("expected a Nodes command");
        };

        assert_eq!(sort, NodeOrder::Name);
        assert_eq!(sort, NodeOrder::default());
        assert!(!sort_reverse);
    }

    #[test]
    fn nodes_takes_every_ordering_by_name() {
        for (flag, expected) in [
            ("name", NodeOrder::Name),
            ("status", NodeOrder::Status),
            ("cpu", NodeOrder::Cpu),
            ("memory", NodeOrder::Memory),
            ("cpu-requested", NodeOrder::CpuRequested),
            ("memory-requested", NodeOrder::MemoryRequested),
            ("age", NodeOrder::Age),
        ] {
            let Some(Command::Nodes { sort, .. }) =
                parse(&["eks", "nodes", "--sort", flag]).command
            else {
                panic!("expected a Nodes command");
            };

            assert_eq!(sort, expected, "--sort {flag}");
        }
    }

    #[test]
    fn nodes_takes_a_reversal_with_or_without_an_ordering() {
        for args in [
            vec!["eks", "nodes", "--sort-reverse"],
            vec!["eks", "nodes", "--sort", "cpu", "--sort-reverse"],
        ] {
            let Some(Command::Nodes { sort_reverse, .. }) = parse(&args).command else {
                panic!("expected a Nodes command");
            };

            assert!(sort_reverse, "{args:?}");
        }
    }

    #[test]
    fn the_two_listings_do_not_share_an_ordering_vocabulary() {
        // `restarts` is a pod ordering and means nothing for a node; a node
        // rejecting it should say so rather than sorting by something else.
        let error = Cli::try_parse_from(["eks", "nodes", "--sort", "restarts"])
            .unwrap_err()
            .to_string();

        assert!(error.contains("restarts"), "{error}");
        assert!(error.contains("status"), "{error}");
        assert!(error.contains("cpu-requested"), "{error}");
    }

    #[test]
    fn pods_accepts_label_and_field_selectors() {
        let cli = parse(&[
            "eks",
            "pods",
            "-l",
            "app=api",
            "--field-selector",
            "status.phase!=Running",
            "-A",
        ]);

        let Some(Command::Pods {
            all_namespaces,
            selector,
            field_selector,
            ..
        }) = cli.command
        else {
            panic!("expected a Pods command");
        };

        assert!(all_namespaces);
        assert_eq!(selector.as_deref(), Some("app=api"));
        assert_eq!(field_selector.as_deref(), Some("status.phase!=Running"));
    }

    #[test]
    fn pods_defaults_to_the_alphabetical_order() {
        // The listing people already know must not change under them because a
        // flag was added; `--sort` is opt-in.
        let Some(Command::Pods { sort, .. }) = parse(&["eks", "pods"]).command else {
            panic!("expected a Pods command");
        };

        assert_eq!(sort, PodOrder::Name);
        assert_eq!(sort, PodOrder::default());
    }

    #[test]
    fn pods_takes_every_ordering_by_name() {
        for (flag, expected) in [
            ("name", PodOrder::Name),
            ("restarts", PodOrder::Restarts),
            ("age", PodOrder::Age),
            ("cpu", PodOrder::Cpu),
            ("memory", PodOrder::Memory),
        ] {
            let Some(Command::Pods { sort, .. }) = parse(&["eks", "pods", "--sort", flag]).command
            else {
                panic!("expected a Pods command");
            };

            assert_eq!(sort, expected, "--sort {flag}");
        }
    }

    #[test]
    fn an_unknown_sort_order_is_rejected_with_the_ones_that_exist() {
        // clap lists the accepted values itself, which is the whole reason the
        // flag is a value enum rather than a free string parsed later.
        //
        // `node` rather than `age`, which this test used before `age` became a
        // real ordering. The assertion is about the rejection listing what is
        // accepted, so any word the flag does not take will do.
        let error = Cli::try_parse_from(["eks", "pods", "--sort", "node"])
            .unwrap_err()
            .to_string();

        assert!(error.contains("node"), "{error}");
        assert!(error.contains("restarts"), "{error}");
        assert!(error.contains("memory"), "{error}");
    }

    #[test]
    fn the_listing_runs_the_natural_way_round_unless_asked() {
        // Another flag that must not move the listing people already have.
        let Some(Command::Pods { sort_reverse, .. }) = parse(&["eks", "pods"]).command else {
            panic!("expected a Pods command");
        };

        assert!(!sort_reverse);
    }

    #[test]
    fn pods_takes_a_reversal_with_or_without_an_ordering() {
        // `--sort-reverse` on its own reverses the default, which is a sensible
        // thing to type and not an error.
        for args in [
            vec!["eks", "pods", "--sort-reverse"],
            vec!["eks", "pods", "--sort", "cpu", "--sort-reverse"],
        ] {
            let Some(Command::Pods { sort_reverse, .. }) = parse(&args).command else {
                panic!("expected a Pods command");
            };

            assert!(sort_reverse, "{args:?}");
        }
    }

    #[test]
    fn pods_selectors_default_to_absent() {
        let Some(Command::Pods {
            selector,
            field_selector,
            ..
        }) = parse(&["eks", "pods"]).command
        else {
            panic!("expected a Pods command");
        };

        assert!(selector.is_none());
        assert!(field_selector.is_none());
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
