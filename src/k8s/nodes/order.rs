//! How a node listing is ordered.
//!
//! `eks nodes` is ten columns of numbers, and printing them alphabetically
//! makes the reader do the work every one of those columns was added to save:
//! "which node is full", "which node is broken", "which node is new" are all
//! visible in the table and all invisible at a glance, because the row that
//! answers them is wherever the instance ID puts it.
//!
//! [`sort`] is a pure function over [`NodeRow`]s. The direction rules, and what
//! becomes of a row an ordering cannot rank, are [`crate::k8s::order`]'s and are
//! shared with `eks pods`; what lives here is the keys, which are not shared —
//! a node has no restart count, and a pod has no allocatable capacity.
//!
//! # Sorting by a share, not by a figure
//!
//! `--sort cpu` on a *pod* ranks by the number in the column. A pod's usage does
//! have a denominator now — what it asked for — but that denominator is whatever
//! somebody typed into a manifest, so 400% of a 10m request is 40m and nobody's
//! problem. A *node*'s denominator is the machine, every figure in that table is
//! already shown as a share of what the node can give out, and that share is the
//! question: a two-core node at 95% is closer to trouble than a sixty-four-core
//! node burning twenty times as much and sitting at 30%. So the node orderings
//! rank by percentage, and a node whose allocatable the API server has not
//! reported yet — a figure with nothing to divide it by — falls to the tail
//! ahead of the nodes with no figure at all.

use std::cmp::{Ordering, Reverse};

use k8s_openapi::jiff::Timestamp;

use super::{NodeRow, Share};
use crate::k8s::order::{Cause, Direction, Rank, Ratio, compare};
use crate::theme::Severity;

/// The order the rows of a node listing are printed in.
///
/// Derives `clap::ValueEnum` so `--sort` parses straight to the value [`sort`]
/// takes. The names match the columns: `cpu` and `memory` are the `USE`
/// columns, as they are for `eks pods`, and the booked figures are spelled out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Order {
    /// By name — what `kubectl get nodes` prints, and the default.
    #[default]
    Name,
    /// Least healthy first: `NotReady`, then unknown, then cordoned, then ready.
    Status,
    /// Busiest CPU first, as a share of allocatable; unsampled nodes last.
    Cpu,
    /// Busiest memory first, as a share of allocatable; unsampled nodes last.
    Memory,
    /// Most CPU booked by pods first; nodes with no pod total last.
    CpuRequested,
    /// Most memory booked by pods first; nodes with no pod total last.
    MemoryRequested,
    /// Closest to its pod limit first; nodes with no pod count last.
    ///
    /// A share like the rest of the node orders, not a headcount: the node to
    /// look at is the one with two slots left, not the one running the most
    /// pods, and on a cluster of mixed instance types those are rarely the
    /// same node.
    Pods,
    /// Youngest first; nodes with no creation timestamp last.
    Age,
}

/// Sort a listing in place.
pub fn sort(rows: &mut [NodeRow], order: Order, direction: Direction) {
    // The alphabet is the last word under every ordering and in either
    // direction, so a listing with nothing left to separate two rows still
    // cannot shuffle between two runs of one command. Node names are unique in
    // a cluster, so this also makes every ordering here total.
    rows.sort_by(|a, b| rank(a, b, order, direction).then_with(|| a.name.cmp(&b.name)));
}

/// The ordering's own comparison, before the alphabetical tie-break.
fn rank(a: &NodeRow, b: &NodeRow, order: Order, direction: Direction) -> Ordering {
    match order {
        Order::Name => direction.apply(a.name.cmp(&b.name)),
        Order::Status => compare(&alarm(a), &alarm(b), direction),
        Order::Cpu => compare(&busiest(a.cpu_used), &busiest(b.cpu_used), direction),
        Order::Memory => compare(&busiest(a.memory_used), &busiest(b.memory_used), direction),
        Order::CpuRequested => compare(
            &busiest(a.cpu_requested),
            &busiest(b.cpu_requested),
            direction,
        ),
        Order::MemoryRequested => compare(
            &busiest(a.memory_requested),
            &busiest(b.memory_requested),
            direction,
        ),
        Order::Pods => compare(&busiest(a.pods), &busiest(b.pods), direction),
        Order::Age => compare(&youngest(a), &youngest(b), direction),
    }
}

/// Whether an ordering has anything at all to rank in these rows.
///
/// The question behind the note under the table: `eks nodes --sort cpu` on a
/// cluster with no metrics-server ranks nothing, and the alphabet decides the
/// whole listing. See [`crate::k8s::order::unranked_note`].
///
/// `any` rather than `all`, matching the rule the usage columns already follow:
/// one node the sampler has not reached is not a listing the ordering failed to
/// order. It is the row a person went looking for that has to be findable, and
/// one ranked row puts it at an end of the table.
#[must_use]
pub fn ranks_any(rows: &[NodeRow], order: Order) -> bool {
    rows.iter().any(|row| ranked(row, order))
}

/// Whether this ordering would put two of these rows in a different
/// arrangement than they are already in.
///
/// The question behind the *advice* half of the note under the table, and a
/// stricter one than [`ranks_any`]: `--sort status` on a cluster where every
/// node is `Ready` ranks every row — every node has a status — so `ranks_any`
/// says yes, and offering it as something to try instead sends the reader to
/// a listing identical to the one that just told them nothing worked.
///
/// Compared against the first row rather than every pair, which is enough
/// because `rank` is a total order: if every row compares equal to the
/// first, they compare equal to each other, and sorting leaves the listing
/// exactly as it was.
#[must_use]
pub fn distinguishes(rows: &[NodeRow], order: Order) -> bool {
    let mut rows = rows.iter();
    let Some(first) = rows.next() else {
        return false;
    };
    rows.any(|row| rank(first, row, order, Direction::Natural) != Ordering::Equal)
}

/// Whether one row carries what an ordering sorts on.
///
/// A second exhaustive match over `Order` beside [`rank`], on purpose: adding an
/// ordering without saying what makes a row rankable under it should fail to
/// compile rather than quietly claim every listing in that order ranked nothing.
fn ranked(row: &NodeRow, order: Order) -> bool {
    match order {
        // Every node has both, `NodeRow::from_node` substituting a placeholder
        // for a node the API server somehow did not name, so these two can never
        // come up empty-handed.
        Order::Name | Order::Status => true,
        Order::Cpu => busiest(row.cpu_used).is_ranked(),
        Order::Memory => busiest(row.memory_used).is_ranked(),
        Order::CpuRequested => busiest(row.cpu_requested).is_ranked(),
        Order::MemoryRequested => busiest(row.memory_requested).is_ranked(),
        Order::Pods => busiest(row.pods).is_ranked(),
        Order::Age => youngest(row).is_ranked(),
    }
}

/// Which of the node table's optional columns this listing could not fill in.
///
/// Both are failures the command has already footnoted above the table — see
/// [`super::requests_unavailable`] and [`super::usage_unavailable`] — and both
/// take a column out of the table that `--sort` can name. Carried as a value so
/// [`cause`] can be a pure function, testable without a cluster to break.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Missing {
    /// The pod listing failed, so `CPU REQ`, `MEM REQ`, and the counted half
    /// of `PODS` are empty.
    pub requests: bool,
    /// There is no live usage in the table, so `CPU USE` and `MEM USE` are
    /// absent — either because the read failed, or because it succeeded and
    /// nothing here had been sampled. Both now put a footnote above the table
    /// (see [`super::usage_unavailable`] and [`super::usage_unsampled`]), which
    /// is what this field is really asking about.
    pub usage: bool,
}

/// Whether a footnote above the table already accounts for an ordering that
/// ranked nothing.
///
/// A third exhaustive match over `Order`, beside `rank` and `ranked`, and
/// for the same reason: an ordering added without saying which of the table's
/// failures could explain it should fail to compile rather than quietly claim
/// nothing above covers it.
///
/// The distinction is not "is this column optional" but "did *this* listing
/// lose it". `--sort cpu` where the `CPU USE` column is present and every figure
/// in it lacks an allocatable to be a share of is [`Cause::Unexplained`]: the
/// ordering ranked nothing, the column is right there in the table, and no
/// footnote above is about it.
#[must_use]
pub fn cause(order: Order, missing: Missing) -> Cause {
    Cause::explained(match order {
        Order::Cpu | Order::Memory => missing.usage,
        // `PODS` belongs here and not with the two above only because of where
        // its number comes from: the count is the pod listing's, so the same
        // failure empties it, even though the limit beside it is the node's own
        // and is still on screen.
        Order::CpuRequested | Order::MemoryRequested | Order::Pods => missing.requests,
        // Nothing the table footnotes can explain these away. A node with no
        // name, no status, or no creation timestamp is the API server being
        // strange, not a column this command failed to fetch.
        Order::Name | Order::Status | Order::Age => false,
    })
}

/// Where a row sits in status order: the node you would look at first, first.
///
/// Every node has a status, so nothing is unranked here. The order within it is
/// a judgement: a node whose kubelet has stopped reporting (`Unknown`) is a
/// problem someone has not noticed yet, while a cordoned one is a node somebody
/// took out of service on purpose. Both are worth seeing above a healthy node,
/// and the accident belongs above the intention.
fn alarm(row: &NodeRow) -> Rank<u8> {
    Rank::By(match row.severity {
        Severity::Critical => 0,
        Severity::Unknown => 1,
        Severity::Warn => 2,
        Severity::Ok => 3,
    })
}

/// Where a row sits in age order: youngest first, undatable nodes last.
///
/// Youngest rather than oldest for the same reason as `eks pods`: the question
/// behind the `AGE` column during an incident is "what changed", and what
/// changed is the node that joined ten minutes ago. `--sort-reverse` asks the
/// other question, which is which node has been up longest.
fn youngest(row: &NodeRow) -> Rank<Reverse<Timestamp>> {
    match row.created_at {
        Some(at) => Rank::By(Reverse(at)),
        None => Rank::Unranked(0),
    }
}

/// Where a row sits in a utilisation order: fullest first, then a figure with
/// no allocatable behind it, then no figure at all.
///
/// The two tail tiers are different failures and neither is a zero. A node with
/// a measurement but no allocatable is one the API server has not finished
/// describing; a node with no measurement is one metrics-server has not reached,
/// or a cluster that has no metrics-server at all. Ranking either as `0%` would
/// put it among the genuinely idle nodes, which is a claim about the machine
/// rather than about what we failed to read.
fn busiest(share: Share) -> Rank<Reverse<Ratio>> {
    match (share.ratio(), share.amount) {
        (Some(ratio), _) => Rank::By(Reverse(Ratio(ratio))),
        (None, Some(_)) => Rank::Unranked(0),
        (None, None) => Rank::Unranked(1),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use k8s_openapi::jiff::SignedDuration;

    use super::*;
    use crate::k8s::nodes::Capacity;
    use crate::k8s::quantity::Quantity;

    /// Every ordering, for the tests that must hold of all of them.
    const ORDERS: [Order; 8] = [
        Order::Name,
        Order::Status,
        Order::Cpu,
        Order::Memory,
        Order::CpuRequested,
        Order::MemoryRequested,
        Order::Pods,
        Order::Age,
    ];

    const DIRECTIONS: [Direction; 2] = [Direction::Natural, Direction::Reversed];

    fn now() -> Timestamp {
        "2026-08-19T12:00:00Z".parse().unwrap()
    }

    fn minutes_ago(mins: i64) -> Timestamp {
        now() - SignedDuration::from_mins(mins)
    }

    fn quantity(text: &str) -> Quantity {
        Quantity::parse(text).unwrap()
    }

    /// A healthy, unmeasured node.
    ///
    /// Built directly rather than through `NodeRow::from_node`: each ordering
    /// reads one or two fields, and arranging a `Node` to produce them would
    /// mean writing conditions and capacity maps these tests are not about.
    fn row(name: &str) -> NodeRow {
        NodeRow {
            name: name.to_owned(),
            status: "Ready".to_owned(),
            severity: Severity::Ok,
            version: "v1.30.2-eks-1552ad0".to_owned(),
            cpu: Capacity::default(),
            memory: Capacity::default(),
            cpu_requested: Share::default(),
            memory_requested: Share::default(),
            cpu_used: Share::default(),
            memory_used: Share::default(),
            usage_stale: false,
            pods: Share::default(),
            age: "3h".to_owned(),
            created_at: Some(minutes_ago(180)),
            internal_ip: "10.0.1.9".to_owned(),
            external_ip: "-".to_owned(),
            os_image: "Amazon Linux 2023.9.20260714".to_owned(),
            kernel_version: "6.1.148-172.265.amzn2023.x86_64".to_owned(),
            container_runtime: "containerd://1.7.28".to_owned(),
            devices: std::collections::BTreeMap::new(),
            ephemeral_storage: Capacity::default(),
            hugepages: std::collections::BTreeMap::new(),
        }
    }

    /// A node whose CPU is measured, or not, against an allocatable that may
    /// itself be missing.
    fn burning(name: &str, used: Option<&str>, allocatable: Option<&str>) -> NodeRow {
        NodeRow {
            cpu_used: Share {
                amount: used.map(quantity),
                allocatable: allocatable.map(quantity),
            },
            ..row(name)
        }
    }

    fn booked(name: &str, requested: Option<&str>) -> NodeRow {
        NodeRow {
            cpu_requested: Share {
                amount: requested.map(quantity),
                allocatable: Some(quantity("4")),
            },
            ..row(name)
        }
    }

    /// A node with `pods` pods on it out of a limit of `limit`, or with a pod
    /// count nobody could read.
    fn crowded(name: &str, pods: Option<u32>, limit: &str) -> NodeRow {
        NodeRow {
            pods: Share {
                amount: pods.map(Quantity::from_count),
                allocatable: Some(quantity(limit)),
            },
            ..row(name)
        }
    }

    fn aged(name: &str, minutes: Option<i64>) -> NodeRow {
        NodeRow {
            created_at: minutes.map(minutes_ago),
            ..row(name)
        }
    }

    fn unhealthy(name: &str, severity: Severity) -> NodeRow {
        NodeRow {
            severity,
            ..row(name)
        }
    }

    fn names(rows: &[NodeRow]) -> Vec<&str> {
        rows.iter().map(|row| row.name.as_str()).collect()
    }

    /// Sort a copy and report the names, so a test can compare two arrangements
    /// of the same rows without threading mutation through the assertion.
    fn sorted(rows: &[NodeRow], order: Order, direction: Direction) -> Vec<String> {
        let mut rows = rows.to_vec();
        sort(&mut rows, order, direction);
        rows.iter().map(|row| row.name.clone()).collect()
    }

    #[test]
    fn a_cluster_with_no_metrics_ranks_nothing_under_cpu() {
        // The case the note under the table exists for: no metrics-server, so
        // no `CPU USE` column, and `--sort cpu` leaves the alphabet in charge.
        let rows = [burning("a", None, Some("4")), burning("b", None, Some("4"))];

        assert!(!ranks_any(&rows, Order::Cpu));
    }

    #[test]
    fn one_sampled_node_is_enough_for_cpu_to_have_ranked_something() {
        // A half-scraped cluster is not a listing the ordering failed to order:
        // the busiest node it knows about is at the top, where it belongs.
        let rows = [
            burning("a", None, Some("4")),
            burning("b", Some("3"), Some("4")),
        ];

        assert!(ranks_any(&rows, Order::Cpu));
    }

    #[test]
    fn a_figure_with_no_allocatable_behind_it_ranks_nothing() {
        // Node orderings rank by share, so a reading with nothing to divide it
        // by is unranked — a different tail tier from no reading at all, but
        // the same answer to "did this ordering order anything".
        let rows = [burning("a", Some("3"), None)];

        assert!(!ranks_any(&rows, Order::Cpu));
    }

    #[test]
    fn a_listing_with_no_pod_totals_ranks_nothing_under_the_booked_orderings() {
        // The pod listing failed, so `CPU REQ` and `MEM REQ` are empty columns.
        let rows = [booked("a", None), booked("b", None)];

        assert!(!ranks_any(&rows, Order::CpuRequested));
        assert!(ranks_any(&[booked("a", Some("1"))], Order::CpuRequested));
    }

    #[test]
    fn nodes_with_no_creation_timestamp_rank_nothing_under_age() {
        assert!(!ranks_any(&[aged("a", None)], Order::Age));
        assert!(ranks_any(
            &[aged("a", None), aged("b", Some(5))],
            Order::Age
        ));
    }

    #[test]
    fn every_node_ranks_by_name_and_by_status() {
        // Both keys are always present — a node the API server did not name
        // gets a placeholder — so these two orderings can never come up empty.
        let rows = [row("a"), unhealthy("b", Severity::Unknown)];

        assert!(ranks_any(&rows, Order::Name));
        assert!(ranks_any(&rows, Order::Status));
    }

    #[test]
    fn a_cluster_where_every_node_is_ready_distinguishes_nothing_under_status() {
        // The case this function exists for: `--sort status` ranks every row —
        // every node has one — but on a cluster with nothing to report, sorting
        // by it reorders nothing at all, and suggesting it as the fix for a
        // failed ordering would send the reader to an identical listing.
        let rows = [row("a"), row("b"), row("c")];

        assert!(ranks_any(&rows, Order::Status));
        assert!(!distinguishes(&rows, Order::Status));
    }

    #[test]
    fn one_unhealthy_node_is_enough_to_distinguish_status() {
        let rows = [row("a"), unhealthy("b", Severity::Critical)];

        assert!(distinguishes(&rows, Order::Status));
    }

    #[test]
    fn a_single_row_listing_distinguishes_nothing_under_any_ordering() {
        // Sorting one row is a no-op whatever the key, so nothing here can ever
        // put it anywhere it was not already — unlike `ranks_any`, which only
        // asks whether the row has a key at all.
        let rows = [row("only")];

        for order in ORDERS {
            assert!(!distinguishes(&rows, order), "{order:?}");
        }
    }

    #[test]
    fn an_empty_listing_distinguishes_nothing_under_any_ordering() {
        for order in ORDERS {
            assert!(!distinguishes(&[], order), "{order:?}");
        }
    }

    #[test]
    fn rows_tied_on_a_figure_distinguish_nothing_even_though_both_ranked() {
        // Two nodes at exactly the same share of allocatable: `ranks_any` says
        // yes for both, and sorting between them changes nothing either way.
        let rows = [
            burning("a", Some("2"), Some("4")),
            burning("b", Some("1"), Some("2")),
        ];

        assert!(ranks_any(&rows, Order::Cpu));
        assert!(!distinguishes(&rows, Order::Cpu));
    }

    #[test]
    fn the_metrics_footnote_explains_the_usage_orderings_and_nothing_else() {
        // `--sort cpu` with no metrics-server: the note above the table already
        // names the cause and links to the fix, so the sort note points at it
        // instead of writing the same paragraph again a line later.
        let missing = Missing {
            requests: false,
            usage: true,
        };

        assert_eq!(cause(Order::Cpu, missing), Cause::Explained);
        assert_eq!(cause(Order::Memory, missing), Cause::Explained);
        assert_eq!(cause(Order::CpuRequested, missing), Cause::Unexplained);
        assert_eq!(cause(Order::MemoryRequested, missing), Cause::Unexplained);
    }

    #[test]
    fn a_failed_pod_listing_explains_the_request_orderings_and_nothing_else() {
        // The other footnote, over the other pair of columns: a read-only role
        // bound to one namespace cannot list pods cluster-wide, and `CPU REQ`
        // and `MEM REQ` go empty rather than the command failing.
        let missing = Missing {
            requests: true,
            usage: false,
        };

        assert_eq!(cause(Order::CpuRequested, missing), Cause::Explained);
        assert_eq!(cause(Order::MemoryRequested, missing), Cause::Explained);
        assert_eq!(cause(Order::Cpu, missing), Cause::Unexplained);
        assert_eq!(cause(Order::Memory, missing), Cause::Unexplained);
    }

    #[test]
    fn a_column_that_is_simply_empty_is_never_blamed_on_a_footnote() {
        // Nothing above the table said a word — which, now that a listing with
        // no usage at all earns a footnote of its own, is the listing whose
        // usage columns are *present* and cannot be ranked anyway: a node
        // metrics-server sampled whose allocatable the API server has not
        // reported leaves `--sort cpu` with no share to rank and nothing above
        // to point at.
        for order in ORDERS {
            assert_eq!(
                cause(order, Missing::default()),
                Cause::Unexplained,
                "{order:?}"
            );
        }
    }

    #[test]
    fn no_footnote_ever_explains_the_orderings_that_are_not_about_a_column() {
        // Even with both fetches failed. A node with no name, no status, or no
        // creation timestamp is the API server being strange, and sending the
        // user to read the metrics footnote about it would waste their time.
        let missing = Missing {
            requests: true,
            usage: true,
        };

        for order in [Order::Name, Order::Status, Order::Age] {
            assert_eq!(cause(order, missing), Cause::Unexplained, "{order:?}");
        }
    }

    #[test]
    fn an_empty_listing_ranks_nothing_under_any_ordering() {
        // True but never printed: `render` drops every note when there are no
        // rows, because "this cluster reports no nodes" is the whole answer.
        for order in ORDERS {
            assert!(!ranks_any(&[], order), "{order:?}");
        }
    }

    #[test]
    fn the_default_order_is_by_name() {
        // The listing people already have, unchanged: `--sort` is opt-in from
        // here, and the default must stay byte-for-byte what it was.
        assert_eq!(Order::default(), Order::Name);

        let mut rows = vec![
            row("ip-10-0-3-4.ec2.internal"),
            row("ip-10-0-1-9.ec2.internal"),
            row("ip-10-0-2-7.ec2.internal"),
        ];

        sort(&mut rows, Order::Name, Direction::Natural);

        assert_eq!(
            names(&rows),
            [
                "ip-10-0-1-9.ec2.internal",
                "ip-10-0-2-7.ec2.internal",
                "ip-10-0-3-4.ec2.internal"
            ]
        );
    }

    #[test]
    fn name_order_reverses_all_the_way_because_every_node_has_a_name() {
        let rows = vec![row("a"), row("b"), row("c")];

        assert_eq!(
            sorted(&rows, Order::Name, Direction::Reversed),
            ["c", "b", "a"]
        );
    }

    #[test]
    fn status_order_puts_the_node_that_needs_looking_at_first() {
        // The whole point of the ordering: a hundred-node cluster with one
        // NotReady in it should open on that node.
        let rows = vec![
            unhealthy("healthy", Severity::Ok),
            unhealthy("cordoned", Severity::Warn),
            unhealthy("registering", Severity::Unknown),
            unhealthy("broken", Severity::Critical),
        ];

        assert_eq!(
            sorted(&rows, Order::Status, Direction::Natural),
            ["broken", "registering", "cordoned", "healthy"]
        );
    }

    #[test]
    fn status_order_reverses_completely() {
        // Nothing is unranked under `status` — every node has one — so unlike
        // the usage orderings this one turns over entirely.
        let rows = vec![
            unhealthy("healthy", Severity::Ok),
            unhealthy("broken", Severity::Critical),
        ];

        assert_eq!(
            sorted(&rows, Order::Status, Direction::Reversed),
            ["healthy", "broken"]
        );
    }

    #[test]
    fn usage_order_ranks_by_share_rather_than_by_the_raw_figure() {
        // The reason node usage sorts differently from pod usage: the big node
        // is burning eight times the CPU and has plenty of room, and the small
        // one is nearly full. "Fullest" is the useful reading of a node.
        let rows = vec![
            burning("large", Some("8"), Some("64")),
            burning("small", Some("1900m"), Some("2")),
        ];

        assert_eq!(
            sorted(&rows, Order::Cpu, Direction::Natural),
            ["small", "large"]
        );
    }

    #[test]
    fn a_node_with_no_live_usage_sorts_last_in_either_direction() {
        // The rule `k8s::order` exists to state once, asserted here on the case
        // that motivated it: a cluster where metrics-server has not reached
        // every node yet must not open on the nodes it has not reached.
        let rows = vec![
            burning("unsampled", None, Some("4")),
            burning("busy", Some("3800m"), Some("4")),
            burning("idle", Some("100m"), Some("4")),
        ];

        for direction in DIRECTIONS {
            let sorted = sorted(&rows, Order::Cpu, direction);
            assert_eq!(
                sorted.last().map(String::as_str),
                Some("unsampled"),
                "{direction:?}"
            );
        }
    }

    #[test]
    fn reversing_a_usage_order_asks_which_node_is_idle_not_which_is_unmeasured() {
        let rows = vec![
            burning("unsampled", None, Some("4")),
            burning("busy", Some("3800m"), Some("4")),
            burning("idle", Some("100m"), Some("4")),
        ];

        assert_eq!(
            sorted(&rows, Order::Cpu, Direction::Reversed),
            ["idle", "busy", "unsampled"]
        );
    }

    #[test]
    fn a_measured_node_with_no_allocatable_sorts_ahead_of_an_unmeasured_one() {
        // Two different failures, and the one where we at least have a figure
        // is the one worth seeing first. A node still registering reports no
        // allocatable; a node metrics-server has never sampled reports nothing.
        let rows = vec![
            burning("no-figure", None, Some("4")),
            burning("no-allocatable", Some("500m"), None),
        ];

        for direction in DIRECTIONS {
            assert_eq!(
                sorted(&rows, Order::Cpu, direction),
                ["no-allocatable", "no-figure"],
                "{direction:?}"
            );
        }
    }

    #[test]
    fn a_node_reporting_zero_allocatable_is_unranked_rather_than_infinite() {
        // `ratio_of` refuses to divide by zero, which happens for real while a
        // node registers. The row has to land somewhere sane rather than at the
        // top of the listing as an infinity.
        let rows = vec![
            burning("registering", Some("500m"), Some("0")),
            burning("normal", Some("100m"), Some("4")),
        ];

        assert_eq!(
            sorted(&rows, Order::Cpu, Direction::Natural),
            ["normal", "registering"]
        );
    }

    #[test]
    fn memory_order_is_the_cpu_order_over_the_other_column() {
        let rows = vec![
            NodeRow {
                memory_used: Share {
                    amount: Some(quantity("1Gi")),
                    allocatable: Some(quantity("16Gi")),
                },
                ..row("roomy")
            },
            NodeRow {
                memory_used: Share {
                    amount: Some(quantity("7Gi")),
                    allocatable: Some(quantity("8Gi")),
                },
                ..row("tight")
            },
        ];

        assert_eq!(
            sorted(&rows, Order::Memory, Direction::Natural),
            ["tight", "roomy"]
        );
    }

    #[test]
    fn a_node_with_nothing_on_it_is_a_real_zero_under_the_request_orders() {
        // Unlike an unsampled node, a node running no pods has a genuine total
        // of zero, and belongs at the idle end of the ranking rather than in
        // the tail with the nodes whose pods could not be listed at all.
        let rows = vec![
            booked("empty", Some("0")),
            booked("unknown", None),
            booked("full", Some("3500m")),
        ];

        assert_eq!(
            sorted(&rows, Order::CpuRequested, Direction::Natural),
            ["full", "empty", "unknown"]
        );
        assert_eq!(
            sorted(&rows, Order::CpuRequested, Direction::Reversed),
            ["empty", "full", "unknown"]
        );
    }

    #[test]
    fn memory_request_order_reads_the_memory_column() {
        let rows = vec![
            NodeRow {
                memory_requested: Share {
                    amount: Some(quantity("1Gi")),
                    allocatable: Some(quantity("8Gi")),
                },
                ..row("light")
            },
            NodeRow {
                memory_requested: Share {
                    amount: Some(quantity("6Gi")),
                    allocatable: Some(quantity("8Gi")),
                },
                ..row("heavy")
            },
        ];

        assert_eq!(
            sorted(&rows, Order::MemoryRequested, Direction::Natural),
            ["heavy", "light"]
        );
    }

    #[test]
    fn age_order_puts_the_newest_node_first() {
        // The node that joined during the incident is the one worth seeing.
        let rows = vec![
            aged("old", Some(10_000)),
            aged("new", Some(4)),
            aged("middling", Some(600)),
        ];

        assert_eq!(
            sorted(&rows, Order::Age, Direction::Natural),
            ["new", "middling", "old"]
        );
        assert_eq!(
            sorted(&rows, Order::Age, Direction::Reversed),
            ["old", "middling", "new"]
        );
    }

    #[test]
    fn a_node_with_no_creation_timestamp_sorts_last_either_way() {
        let rows = vec![
            aged("undated", None),
            aged("new", Some(1)),
            aged("old", Some(9_000)),
        ];

        for direction in DIRECTIONS {
            let sorted = sorted(&rows, Order::Age, direction);
            assert_eq!(
                sorted.last().map(String::as_str),
                Some("undated"),
                "{direction:?}"
            );
        }
    }

    #[test]
    fn age_ranks_on_the_instant_rather_than_on_the_rendered_string() {
        // Both nodes render as `3d`; they are nearly a day apart. Sorting on
        // the column text would call them equal and fall through to the name.
        let rows = vec![
            NodeRow {
                age: "3d".to_owned(),
                created_at: Some(minutes_ago(60 * 24 * 3)),
                ..row("younger")
            },
            NodeRow {
                age: "3d".to_owned(),
                created_at: Some(minutes_ago(60 * 24 * 3 + 60 * 20)),
                ..row("older")
            },
        ];

        assert_eq!(
            sorted(&rows, Order::Age, Direction::Natural),
            ["younger", "older"]
        );
    }

    #[test]
    fn the_alphabet_breaks_every_tie_in_every_direction() {
        // Node names are unique in a cluster, so this makes every ordering
        // total: one listing cannot render two ways between two runs.
        let rows = vec![row("charlie"), row("alpha"), row("bravo")];

        for order in ORDERS {
            for direction in DIRECTIONS {
                let once = sorted(&rows, order, direction);
                let mut reversed_input = rows.clone();
                reversed_input.reverse();
                let twice = sorted(&reversed_input, order, direction);

                assert_eq!(once, twice, "{order:?} {direction:?}");
            }
        }
    }

    #[test]
    fn every_ordering_survives_an_empty_and_a_single_row_listing() {
        for order in ORDERS {
            for direction in DIRECTIONS {
                let mut none: Vec<NodeRow> = Vec::new();
                sort(&mut none, order, direction);
                assert!(none.is_empty(), "{order:?} {direction:?}");

                let mut one = vec![row("only")];
                sort(&mut one, order, direction);
                assert_eq!(names(&one), ["only"], "{order:?} {direction:?}");
            }
        }
    }

    #[test]
    fn a_listing_with_nothing_measured_at_all_falls_back_to_the_alphabet() {
        // A cluster with no metrics-server sorted by `cpu`: every row is
        // unranked, so the tie-break is the whole ordering. It must still be
        // stable and readable rather than arbitrary.
        let rows = vec![row("charlie"), row("alpha"), row("bravo")];

        for direction in DIRECTIONS {
            assert_eq!(
                sorted(&rows, Order::Cpu, direction),
                ["alpha", "bravo", "charlie"],
                "{direction:?}"
            );
        }
    }

    #[test]
    fn every_ordering_names_itself_under_the_table() {
        // The note is `k8s::order`'s, but the names it prints are this enum's,
        // and they are what the user typed after `--sort`. Asserting it here
        // means renaming a variant cannot quietly change what the note says
        // without a test noticing.
        for direction in DIRECTIONS {
            for order in ORDERS {
                let note = crate::k8s::order::note(order, direction);

                if order == Order::default() && direction == Direction::Natural {
                    assert_eq!(note, None, "{order:?} {direction:?}");
                    continue;
                }

                let note = note.expect("a reordered listing should say so");
                assert!(note.starts_with("Sorted by "), "{note}");
                assert_eq!(
                    note.contains("reversed"),
                    direction == Direction::Reversed,
                    "{note}"
                );
            }
        }
    }

    #[test]
    fn a_multi_word_ordering_is_named_the_way_the_flag_spells_it() {
        // `cpu-requested`, not `CpuRequested` — the note has to echo a value
        // `--sort` would actually accept.
        assert_eq!(
            crate::k8s::order::note(Order::CpuRequested, Direction::Natural).as_deref(),
            Some("Sorted by cpu-requested.")
        );
    }
    #[test]
    fn sorting_by_pods_ranks_the_share_of_the_limit_rather_than_the_headcount() {
        // The node to go and look at is the one with two slots left, not the
        // one running the most pods. On a cluster of mixed instance types
        // those are different nodes, and only one of them is a problem.
        let rows = [
            crowded("big", Some(80), "234"),
            crowded("small", Some(50), "58"),
        ];

        // 86% beats 34%, though 50 is the smaller headcount.
        assert_eq!(
            sorted(&rows, Order::Pods, Direction::Natural),
            ["small", "big"]
        );
    }

    #[test]
    fn a_node_whose_pods_could_not_be_counted_sorts_into_the_tail() {
        // A failed pod listing is not an empty node, and must not rank as one:
        // sorting it among the genuinely idle nodes would be a claim about the
        // machine rather than about what we failed to read.
        let rows = [
            crowded("unknown", None, "58"),
            crowded("idle", Some(0), "58"),
            crowded("full", Some(58), "58"),
        ];

        assert_eq!(
            sorted(&rows, Order::Pods, Direction::Natural),
            ["full", "idle", "unknown"]
        );
        // And reversing puts the busiest last without dragging the tail up
        // with it.
        assert_eq!(
            sorted(&rows, Order::Pods, Direction::Reversed),
            ["idle", "full", "unknown"]
        );
    }

    #[test]
    fn a_listing_with_no_pod_counts_at_all_ranks_nothing_under_pods() {
        let rows = [crowded("a", None, "58"), crowded("b", None, "58")];

        assert!(!ranks_any(&rows, Order::Pods));
        // And one counted node is enough to put it at an end of the table.
        let rows = [crowded("a", None, "58"), crowded("b", Some(3), "58")];
        assert!(ranks_any(&rows, Order::Pods));
    }

    #[test]
    fn the_failed_pod_listing_explains_an_unrankable_pod_ordering() {
        // The same footnote that explains the empty `CPU REQ` explains this:
        // the count is the pod listing's, so one failure takes all three.
        assert_eq!(
            cause(
                Order::Pods,
                Missing {
                    requests: true,
                    usage: false
                }
            ),
            Cause::Explained
        );
        // The limit beside it is the node's own, so nothing about the metrics
        // endpoint can account for this ordering.
        assert_eq!(
            cause(
                Order::Pods,
                Missing {
                    requests: false,
                    usage: true
                }
            ),
            Cause::Unexplained
        );
    }
}
