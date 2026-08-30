//! The command-line surface.
//!
//! Keeping the parser separate from the commands themselves means argument
//! handling can be tested without touching a cluster.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::aws::LoginMode;
use crate::k8s::nodes::Order as NodeOrder;
use crate::k8s::page::Budget;
use crate::k8s::pods::Order as PodOrder;
use crate::theme::ColourChoice;
use crate::ui::RefreshInterval;

/// Explore and interact with AWS EKS clusters.
#[derive(Debug, Parser)]
// Running `eks` bare drops you straight into the dashboard, which
// `Option<Command>` is what provides; the subcommands are for scripting and
// quick one-shot answers. `args_conflicts_with_subcommands` used to be set here
// as well, and it bought nothing: every argument this parser has at the top
// level is a *global* one, meant to be legal beside a subcommand, and the
// setting made `eks --context prod nodes` an error while `eks nodes --context
// prod` worked. Ordering that arbitrary reads as a bug rather than as a rule.
#[command(name = "eks", version, about, long_about = None, propagate_version = true)]
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

    /// Select pods by label, e.g. `-l app=api,tier notin (canary)`. Used by
    /// `eks pods` and the dashboard's pod-drilldown pane, so a selector filters
    /// the same pods whichever surface reads it; other commands accept the flag
    /// without acting on it, the same as `--namespace`.
    #[arg(long, short = 'l', global = true, value_name = "SELECTOR")]
    pub selector: Option<String>,

    /// Select pods by field, e.g. `--field-selector status.phase!=Running`.
    #[arg(long, global = true, value_name = "SELECTOR")]
    pub field_selector: Option<String>,

    /// Path to a kubeconfig file (overrides KUBECONFIG).
    #[arg(long, global = true, value_name = "PATH", env = "KUBECONFIG")]
    pub kubeconfig: Option<PathBuf>,

    /// How long to wait for any one step of talking to the cluster: the
    /// kubeconfig's credential helper, and then each request. `0` waits for as
    /// long as it takes. A listing too large for one response is read in
    /// pages, and this is the limit on each page rather than on the command.
    #[arg(long, global = true, value_name = "DURATION", default_value_t = Budget::default())]
    pub timeout: Budget,

    /// When to colour the listing: `auto` (a terminal that wants it), `always`,
    /// or `never`. `auto` also honours `NO_COLOR`.
    #[arg(
        long = "color",
        visible_alias = "colour",
        global = true,
        value_name = "WHEN",
        default_value = "auto"
    )]
    pub color: ColourChoice,

    /// When to log in to AWS IAM Identity Center for you: `auto` (offer, when
    /// there is a terminal to ask at), `always` (log in without asking), or
    /// `never` (be told what to run instead). Only ever offered for a context
    /// whose AWS profile uses Identity Center and whose session has run out.
    #[arg(long, global = true, value_name = "WHEN", default_value = "auto")]
    pub login: LoginMode,

    /// How often the dashboard's panes refresh themselves in the background,
    /// on top of pressing `r` to refresh on demand. `0` turns automatic
    /// refresh off. The CLI listings ignore this flag: they print once and
    /// exit.
    #[arg(long, global = true, value_name = "DURATION", default_value_t = RefreshInterval::default())]
    pub refresh: RefreshInterval,

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

        /// Add the addresses, AMI, kernel, and container runtime, as
        /// `kubectl get nodes -o wide` does.
        #[arg(long)]
        wide: bool,
    },

    /// List the pods of a namespace, or of every namespace.
    #[command(visible_alias = "po")]
    Pods {
        /// List pods in every namespace, adding a NAMESPACE column.
        #[arg(long, short = 'A')]
        all_namespaces: bool,

        /// Order the listing. Every order but `name` puts the most
        /// interesting row first: the newest restart, the youngest pod, the
        /// largest usage figure, or the pod furthest over its own request.
        #[arg(long, value_name = "ORDER", default_value = "name")]
        sort: PodOrder,

        /// Reverse the order. Pods there is nothing to rank — never restarted,
        /// or never sampled — stay at the end either way.
        #[arg(long)]
        sort_reverse: bool,

        /// Add the pod IP, the nominated node, and the readiness gates, as
        /// `kubectl get pods -o wide` does.
        #[arg(long)]
        wide: bool,
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
        let Some(Command::Nodes {
            sort, sort_reverse, ..
        }) = parse(&["eks", "nodes"]).command
        else {
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
        // The selectors are global flags now — shared with the dashboard's
        // pod-drilldown pane — but they still parse in `eks pods` exactly as
        // before.
        let cli = parse(&[
            "eks",
            "pods",
            "-l",
            "app=api",
            "--field-selector",
            "status.phase!=Running",
            "-A",
        ]);

        let Some(Command::Pods { all_namespaces, .. }) = cli.command else {
            panic!("expected a Pods command");
        };

        assert!(all_namespaces);
        assert_eq!(cli.global.selector.as_deref(), Some("app=api"));
        assert_eq!(
            cli.global.field_selector.as_deref(),
            Some("status.phase!=Running")
        );
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
            ("cpu-share", PodOrder::CpuShare),
            ("memory-share", PodOrder::MemoryShare),
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
        let cli = parse(&["eks", "pods"]);

        assert!(cli.global.selector.is_none());
        assert!(cli.global.field_selector.is_none());
    }

    #[test]
    fn a_selector_parses_before_the_subcommand_too() {
        // Global flags must work on either side — the same rule every other
        // global flag follows, and the dashboard has no subcommand at all to
        // put one after.
        let cli = parse(&["eks", "-l", "app=api", "dashboard"]);
        assert_eq!(cli.global.selector.as_deref(), Some("app=api"));

        let cli = parse(&["eks", "-l", "app=api"]);
        assert_eq!(cli.global.selector.as_deref(), Some("app=api"));
        assert!(cli.command.is_none());
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
    fn every_global_flag_parses_on_either_side_of_the_subcommand() {
        // `eks --kubeconfig x contexts` used to be rejected while
        // `eks contexts --kubeconfig x` worked, which reads as a bug rather than
        // as a rule — nobody can see where the line is.
        let both_ways = |flag: &[&str]| {
            let mut before = vec!["eks"];
            before.extend_from_slice(flag);
            before.push("nodes");

            let mut after = vec!["eks", "nodes"];
            after.extend_from_slice(flag);

            (parse(&before), parse(&after))
        };

        let (before, after) = both_ways(&["--kubeconfig", "/tmp/kubeconfig"]);
        assert_eq!(before.global.kubeconfig, after.global.kubeconfig);
        assert_eq!(
            before.global.kubeconfig.as_deref(),
            Some(std::path::Path::new("/tmp/kubeconfig"))
        );

        let (before, after) = both_ways(&["--context", "prod"]);
        assert_eq!(before.global.context, after.global.context);
        assert_eq!(before.global.context.as_deref(), Some("prod"));

        let (before, after) = both_ways(&["-n", "payments"]);
        assert_eq!(before.global.namespace, after.global.namespace);
        assert_eq!(before.global.namespace.as_deref(), Some("payments"));

        let (before, after) = both_ways(&["-l", "app=api"]);
        assert_eq!(before.global.selector, after.global.selector);
        assert_eq!(before.global.selector.as_deref(), Some("app=api"));

        let (before, after) = both_ways(&["--field-selector", "status.phase!=Running"]);
        assert_eq!(before.global.field_selector, after.global.field_selector);
        assert_eq!(
            before.global.field_selector.as_deref(),
            Some("status.phase!=Running")
        );

        let (before, after) = both_ways(&["--timeout", "5s"]);
        assert_eq!(before.global.timeout, after.global.timeout);
        assert_eq!(
            before.global.timeout,
            Budget::of(std::time::Duration::from_secs(5))
        );

        let (before, after) = both_ways(&["-vv"]);
        assert_eq!(before.global.verbose, after.global.verbose);
        assert_eq!(before.global.verbose, 2);

        let (before, after) = both_ways(&["--login", "never"]);
        assert_eq!(before.global.login, after.global.login);
        assert_eq!(before.global.login, LoginMode::Never);
    }

    #[test]
    fn login_offers_to_sign_you_in_unless_told_otherwise() {
        // The default has to be `auto` rather than `always`: a browser opening
        // without a yes is the one behaviour nobody asked for.
        assert_eq!(parse(&["eks", "nodes"]).global.login, LoginMode::Auto);
        assert_eq!(parse(&["eks"]).global.login, LoginMode::Auto);
    }

    #[test]
    fn every_spelling_of_login_parses() {
        for (flag, expected) in [
            ("auto", LoginMode::Auto),
            ("always", LoginMode::Always),
            ("never", LoginMode::Never),
        ] {
            let cli = parse(&["eks", "nodes", "--login", flag]);
            assert_eq!(cli.global.login, expected, "--login {flag}");
        }
    }

    #[test]
    fn a_login_value_that_is_not_one_of_the_three_is_rejected_with_the_three_listed() {
        // The same bargain `--color`, `--sort`, and `--timeout` make: a bad
        // value is a sentence naming the good ones, before anything connects.
        let error = Cli::try_parse_from(["eks", "nodes", "--login", "maybe"])
            .expect_err("`--login maybe` should be rejected")
            .to_string();

        assert!(error.contains("auto"), "{error}");
        assert!(error.contains("always"), "{error}");
        assert!(error.contains("never"), "{error}");
    }

    #[test]
    fn a_global_flag_before_a_subcommand_leaves_the_subcommand_intact() {
        // The failure mode of getting this wrong is subtler than a rejection:
        // the flag parses and the subcommand quietly becomes the dashboard.
        let cli = parse(&["eks", "--context", "prod", "pods", "-A"]);

        let Some(Command::Pods { all_namespaces, .. }) = cli.command else {
            panic!("expected a Pods command");
        };
        assert!(all_namespaces);
        assert_eq!(cli.global.context.as_deref(), Some("prod"));
    }

    #[test]
    fn a_bare_invocation_still_takes_the_global_flags() {
        // `eks --context prod` is the dashboard, pointed at a cluster — the
        // form the old conflict setting existed to protect.
        let cli = parse(&["eks", "--context", "prod", "--timeout", "0"]);

        assert!(cli.command.is_none());
        assert_eq!(cli.global.context.as_deref(), Some("prod"));
        assert_eq!(cli.global.timeout, Budget::unlimited());
    }

    #[test]
    fn requests_wait_thirty_seconds_unless_told_otherwise() {
        // Documented in the README and in `--help`; asserted here so the two
        // cannot drift apart silently.
        assert_eq!(parse(&["eks", "nodes"]).global.timeout, Budget::default());
        assert_eq!(Budget::default().to_string(), "30s");
    }

    #[test]
    fn the_dashboard_refreshes_every_fifteen_seconds_unless_told_otherwise() {
        assert_eq!(parse(&["eks"]).global.refresh, RefreshInterval::default());
        assert_eq!(RefreshInterval::default().to_string(), "15s");
    }

    #[test]
    fn refresh_is_a_global_flag_that_parses_on_either_side_of_a_subcommand() {
        let before = parse(&["eks", "--refresh", "5s", "nodes"]);
        let after = parse(&["eks", "nodes", "--refresh", "5s"]);

        assert_eq!(before.global.refresh, after.global.refresh);
        assert_eq!(
            before.global.refresh,
            RefreshInterval::every(std::time::Duration::from_secs(5))
        );
    }

    #[test]
    fn refresh_zero_turns_automatic_refresh_off() {
        assert_eq!(
            parse(&["eks", "--refresh", "0"]).global.refresh,
            RefreshInterval::never()
        );
    }

    #[test]
    fn a_refresh_interval_that_is_not_a_duration_is_rejected_with_examples() {
        // Same grammar as `--timeout`, so the same bargain: reject before
        // anything starts, with the accepted spellings in the message.
        let error = Cli::try_parse_from(["eks", "--refresh", "soon"])
            .unwrap_err()
            .to_string();

        assert!(error.contains("soon"), "{error}");
        assert!(error.contains("30s"), "{error}");
    }

    #[test]
    fn a_timeout_that_is_not_a_duration_is_rejected_with_examples() {
        // Before anything connects, and with the accepted spellings in the
        // message — the same bargain `--sort` makes.
        let error = Cli::try_parse_from(["eks", "nodes", "--timeout", "soon"])
            .unwrap_err()
            .to_string();

        assert!(error.contains("soon"), "{error}");
        assert!(error.contains("30s"), "{error}");
        assert!(error.contains("500ms"), "{error}");
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

    #[test]
    fn both_listings_default_to_the_narrow_table() {
        // The table people already have must not grow columns because a flag
        // was added, so `--wide` is opt-in on both.
        let Some(Command::Nodes { wide, .. }) = parse(&["eks", "nodes"]).command else {
            panic!("expected a Nodes command");
        };
        assert!(!wide);

        let Some(Command::Pods { wide, .. }) = parse(&["eks", "pods"]).command else {
            panic!("expected a Pods command");
        };
        assert!(!wide);
    }

    #[test]
    fn both_listings_take_wide() {
        // One flag, spelled the same on both tables: a `--wide` that worked on
        // one listing and was an unknown argument on its twin would read as a
        // bug rather than as a decision.
        let Some(Command::Nodes { wide, .. }) = parse(&["eks", "nodes", "--wide"]).command else {
            panic!("expected a Nodes command");
        };
        assert!(wide);

        let Some(Command::Pods { wide, .. }) = parse(&["eks", "pods", "--wide"]).command else {
            panic!("expected a Pods command");
        };
        assert!(wide);
    }

    #[test]
    fn wide_composes_with_the_other_listing_flags() {
        // Nothing about `--wide` is exclusive with a scope, a selector, or an
        // ordering — they answer different questions about the same listing.
        let cli = parse(&[
            "eks",
            "pods",
            "-A",
            "-l",
            "app=api",
            "--sort",
            "cpu",
            "--sort-reverse",
            "--wide",
        ]);
        let Some(Command::Pods {
            all_namespaces,
            sort,
            sort_reverse,
            wide,
        }) = cli.command
        else {
            panic!("expected a Pods command");
        };

        assert!(all_namespaces);
        assert_eq!(cli.global.selector.as_deref(), Some("app=api"));
        assert_eq!(sort, PodOrder::Cpu);
        assert!(sort_reverse);
        assert!(wide);
    }

    #[test]
    fn colour_defaults_to_looking_at_the_terminal() {
        // The listing people already have must not gain escape sequences
        // because a flag was added; `auto` is the same rule they had before it
        // existed, which is "a terminal gets colour and a pipe does not".
        assert_eq!(parse(&["eks", "nodes"]).global.color, ColourChoice::Auto);
        assert_eq!(ColourChoice::default(), ColourChoice::Auto);
    }

    #[test]
    fn colour_takes_each_of_its_three_answers() {
        for (flag, expected) in [
            ("auto", ColourChoice::Auto),
            ("always", ColourChoice::Always),
            ("never", ColourChoice::Never),
        ] {
            let cli = parse(&["eks", "nodes", "--color", flag]);
            assert_eq!(cli.global.color, expected, "--color {flag}");
        }
    }

    #[test]
    fn colour_is_spelled_both_ways() {
        // Every sentence this project writes says "colour" and every other CLI
        // in the terminal says "color". A user who types the one the README
        // uses should not be told it does not exist.
        assert_eq!(
            parse(&["eks", "nodes", "--colour", "never"]).global.color,
            ColourChoice::Never
        );
    }

    #[test]
    fn an_unknown_colour_setting_is_rejected_with_the_ones_that_exist() {
        // The same bargain `--sort` and `--timeout` make: a bad value is
        // rejected before anything connects, with the accepted spellings in
        // the message.
        let error = Cli::try_parse_from(["eks", "nodes", "--color", "sometimes"])
            .unwrap_err()
            .to_string();

        assert!(error.contains("sometimes"), "{error}");
        assert!(error.contains("auto"), "{error}");
        assert!(error.contains("always"), "{error}");
        assert!(error.contains("never"), "{error}");
    }

    #[test]
    fn colour_is_global_like_every_other_flag() {
        // It describes the output stream rather than one listing, so it parses
        // on either side of the subcommand and on the bare dashboard form.
        assert_eq!(
            parse(&["eks", "--color", "never", "pods"]).global.color,
            ColourChoice::Never
        );
        assert_eq!(
            parse(&["eks", "pods", "--color", "never"]).global.color,
            ColourChoice::Never
        );
        assert_eq!(
            parse(&["eks", "--color", "always"]).global.color,
            ColourChoice::Always
        );
    }
}
