//! How a pod listing is ordered.
//!
//! The default is alphabetical, which is the right answer for reading a
//! namespace and the wrong one for every column that carries a number. "What is
//! crashing *now*", "what is burning a core", "what just rolled out" are sorts,
//! not scans: in a hundred-row listing the pod that answers them sits wherever
//! its name puts it, indistinguishable at a glance from the ninety-nine others.
//!
//! [`sort`] is a pure function over the rows — no clock, no cluster, because
//! [`PodRow`] already carries the instant each restart finished, the instant the
//! pod was created, and the usage metrics-server last sampled. Every ordering
//! here is total: two rows only ever compare equal if they are the same pod in
//! the same namespace, so one listing always renders the same way twice.
//!
//! Which way round an ordering runs, and what happens to the rows it cannot
//! rank, are [`crate::k8s::order`]'s rules rather than this module's — `eks
//! nodes` follows the same ones. What lives here is the keys.

use std::cmp::{Ordering, Reverse};

use k8s_openapi::jiff::Timestamp;

use super::PodRow;
use crate::k8s::order::{Cause, Direction, Rank, compare};
use crate::k8s::quantity::Quantity;

/// The order the rows of a pod listing are printed in.
///
/// Derives `clap::ValueEnum` so `--sort` parses straight to the value [`sort`]
/// takes. A separate CLI-facing copy would be one more table to keep in step,
/// and what it would be translating is a presentation choice already.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Order {
    /// Namespace, then name — what `kubectl get pods` prints, and the default.
    #[default]
    Name,
    /// Most recently restarted first; pods that have never restarted last.
    Restarts,
    /// Youngest first; pods with no creation timestamp last.
    Age,
    /// Most CPU first; pods with no sample last.
    Cpu,
    /// Most memory first; pods with no sample last.
    Memory,
}

/// Sort a listing in place.
pub fn sort(rows: &mut [PodRow], order: Order, direction: Direction) {
    // The alphabet is the last word under every ordering and in either
    // direction, so a listing with nothing left to separate two rows still
    // cannot shuffle between two runs of one command.
    rows.sort_by(|a, b| rank(a, b, order, direction).then_with(|| by_name(a, b)));
}

/// The ordering's own comparison, before the alphabetical tie-break.
fn rank(a: &PodRow, b: &PodRow, order: Order, direction: Direction) -> Ordering {
    match order {
        Order::Name => direction.apply(name_key(a).cmp(&name_key(b))),
        Order::Restarts => compare(&recency(a), &recency(b), direction)
            // The count is only the tie-break, never the key. Sorting by how
            // *many* times a pod has restarted answers a different question —
            // it puts a pod that failed two hundred times last week above one
            // that started crashing a minute ago, which is precisely backwards
            // during an incident.
            .then_with(|| direction.apply(Reverse(a.restarts).cmp(&Reverse(b.restarts)))),
        Order::Age => compare(&youngest(a), &youngest(b), direction),
        Order::Cpu => compare(&largest(a.cpu_used), &largest(b.cpu_used), direction),
        Order::Memory => compare(&largest(a.memory_used), &largest(b.memory_used), direction),
    }
}

/// Whether an ordering has anything at all to rank in these rows.
///
/// The question behind the note under the table: `eks pods --sort cpu` where
/// metrics-server has sampled nothing ranks nothing, and the alphabet decides
/// the whole listing. See [`crate::k8s::order::unranked_note`].
///
/// `any` rather than `all`: one unsampled pod is not a listing the ordering
/// failed to order, and one ranked row puts the pod a person went looking for at
/// an end of the table.
#[must_use]
pub fn ranks_any(rows: &[PodRow], order: Order) -> bool {
    rows.iter().any(|row| ranked(row, order))
}

/// Whether this ordering would put two of these rows in a different
/// arrangement than they are already in.
///
/// The question behind the *advice* half of the note under the table, and a
/// stricter one than [`ranks_any`]. See [`crate::k8s::nodes::distinguishes`],
/// the same function over a node listing, for the case this exists to catch:
/// no pod ordering here has a `status`-shaped column that is always present
/// and often uniform, so this is mostly the safety net for a namespace where
/// every sampled pod happens to be using the exact same amount of CPU.
///
/// Compared against the first row rather than every pair, which is enough
/// because `rank` is a total order: if every row compares equal to the
/// first, they compare equal to each other, and sorting leaves the listing
/// exactly as it was.
#[must_use]
pub fn distinguishes(rows: &[PodRow], order: Order) -> bool {
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
fn ranked(row: &PodRow, order: Order) -> bool {
    match order {
        // Every pod has a namespace and a name.
        Order::Name => true,
        // Not `recency(row).is_ranked()` alone. Under this ordering the restart
        // count is a key as well as a tie-break, so a pod that has restarted
        // without the kubelet recording a `finishedAt` is one this ordering
        // still lifted clear of the healthy rows. Calling that "nothing to sort
        // by" would say so over a listing with a crashing pod near the top.
        Order::Restarts => recency(row).is_ranked() || row.restarts > 0,
        Order::Age => youngest(row).is_ranked(),
        Order::Cpu => largest(row.cpu_used).is_ranked(),
        Order::Memory => largest(row.memory_used).is_ranked(),
    }
}

/// Which of the pod table's optional columns this listing could not fill in.
///
/// One field rather than [`super::super::nodes::order::Missing`]'s two, because
/// the pod table has one optional pair: everything else in it comes from the pod
/// listing that the command fails outright without. Still a struct, so the two
/// listings read the same way at the two call sites and so a second optional
/// column here is a field rather than a changed signature.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Missing {
    /// There is no live usage in the table, so `CPU` and `MEMORY` are absent —
    /// either because the read failed, or because it succeeded and nothing here
    /// had been sampled. Both put a footnote above the table: see
    /// [`super::usage_unavailable`] and [`super::usage_unsampled`].
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
/// `restarts` is the case the roadmap entry behind this was written about: a
/// namespace where nothing has ever crashed ranks nothing under it, and there
/// is no failure above the table to point at, because there was no failure —
/// the pods are simply healthy. [`Cause::Unexplained`] is the honest answer, and
/// the advice the note carries is then the whole of what it has to offer.
#[must_use]
pub fn cause(order: Order, missing: Missing) -> Cause {
    Cause::explained(match order {
        Order::Cpu | Order::Memory => missing.usage,
        Order::Name | Order::Restarts | Order::Age => false,
    })
}

/// Namespace, then name.
///
/// Namespace leads because that is what the `NAMESPACE` column implies about
/// the listing it heads, and it is the order `kubectl get pods -A` uses.
fn by_name(a: &PodRow, b: &PodRow) -> Ordering {
    name_key(a).cmp(&name_key(b))
}

fn name_key(row: &PodRow) -> (&str, &str) {
    (&row.namespace, &row.name)
}

/// Where a row sits in restart order: dated newest-first, then undated, then
/// never restarted.
fn recency(row: &PodRow) -> Rank<Reverse<Timestamp>> {
    match row.last_restart {
        Some(at) => Rank::By(Reverse(at)),
        // Defensive about the pairing rather than trusting it: `restarts` and
        // `last_restart` are set together in `PodRow::from_pod`, and a count
        // with no date is the honest reading if that ever comes apart.
        None if row.restarts > 0 => Rank::Unranked(0),
        None => Rank::Unranked(1),
    }
}

/// Where a row sits in age order: youngest first, undatable pods last.
///
/// Youngest rather than oldest because the question behind the `AGE` column
/// during an incident is "what changed", and what changed is the pod that
/// started two minutes ago. `--sort-reverse` is the other reading.
fn youngest(row: &PodRow) -> Rank<Reverse<Timestamp>> {
    match row.created_at {
        Some(at) => Rank::By(Reverse(at)),
        None => Rank::Unranked(0),
    }
}

/// Where a row sits in usage order: largest first, unsampled pods last.
///
/// A pod metrics-server has not sampled is unranked rather than zero. Reading
/// an absent figure as `0` would put it among the genuinely idle pods, which is
/// a claim about the pod rather than about the scraper.
fn largest(used: Option<Quantity>) -> Rank<Reverse<Quantity>> {
    match used {
        Some(quantity) => Rank::By(Reverse(quantity)),
        None => Rank::Unranked(0),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use k8s_openapi::jiff::SignedDuration;

    use super::*;
    use crate::theme::Severity;

    /// Every ordering, for the tests that must hold of all of them.
    const ORDERS: [Order; 5] = [
        Order::Name,
        Order::Restarts,
        Order::Age,
        Order::Cpu,
        Order::Memory,
    ];

    const DIRECTIONS: [Direction; 2] = [Direction::Natural, Direction::Reversed];

    fn now() -> Timestamp {
        "2026-08-18T12:00:00Z".parse().unwrap()
    }

    fn minutes_ago(mins: i64) -> Timestamp {
        now() - SignedDuration::from_mins(mins)
    }

    /// A row, named, with a restart count and when the newest one finished.
    ///
    /// Built directly rather than through `PodRow::from_pod`: the ordering is a
    /// function of a handful of fields, and going via a `Pod` would mean
    /// arranging container statuses to say something these tests are not about.
    fn row(name: &str, restarts: i32, restarted_minutes_ago: Option<i64>) -> PodRow {
        let last_restart = restarted_minutes_ago.map(minutes_ago);

        PodRow {
            namespace: "payments".to_owned(),
            name: name.to_owned(),
            ready: "1/1".to_owned(),
            status: "Running".to_owned(),
            severity: Severity::Ok,
            restarts,
            restart_age: last_restart
                .map(|at| crate::format::human_duration(now().duration_since(at))),
            last_restart,
            age: "3h".to_owned(),
            created_at: Some(minutes_ago(180)),
            cpu_used: None,
            memory_used: None,
            // Nothing here sorts on the request, and a pod that asked for
            // nothing is a real pod; the orderings are over what is measured.
            cpu_requested: Quantity::default(),
            memory_requested: Quantity::default(),
            node: "ip-10-0-1-9.ec2.internal".to_owned(),
            ip: "10.0.1.42".to_owned(),
            nominated_node: "-".to_owned(),
            readiness_gates: None,
        }
    }

    fn in_namespace(namespace: &str, name: &str) -> PodRow {
        PodRow {
            namespace: namespace.to_owned(),
            ..row(name, 0, None)
        }
    }

    /// A row created `minutes` ago, or with no creation timestamp at all.
    fn aged(name: &str, minutes: Option<i64>) -> PodRow {
        PodRow {
            created_at: minutes.map(minutes_ago),
            ..row(name, 0, None)
        }
    }

    /// A row metrics-server has sampled, or has not. `None` is the pod it has
    /// no figure for, which is not the same thing as a pod using nothing.
    fn using(name: &str, cpu: Option<&str>, memory: Option<&str>) -> PodRow {
        PodRow {
            cpu_used: cpu.map(|text| Quantity::parse(text).unwrap()),
            memory_used: memory.map(|text| Quantity::parse(text).unwrap()),
            ..row(name, 0, None)
        }
    }

    fn names(rows: &[PodRow]) -> Vec<&str> {
        rows.iter().map(|row| row.name.as_str()).collect()
    }

    /// Sort a copy and report the names, so a test can compare two arrangements
    /// of the same rows without threading mutation through the assertion.
    fn sorted(rows: &[PodRow], order: Order, direction: Direction) -> Vec<String> {
        let mut rows = rows.to_vec();
        sort(&mut rows, order, direction);
        rows.iter().map(|row| row.name.clone()).collect()
    }

    #[test]
    fn a_listing_where_nothing_has_ever_restarted_ranks_nothing_under_restarts() {
        // The good-news case, and still worth saying: `--sort restarts` on a
        // healthy namespace prints an alphabetical table, and without a note it
        // looks exactly like a flag that was ignored.
        let rows = [row("api", 0, None), row("worker", 0, None)];

        assert!(!ranks_any(&rows, Order::Restarts));
    }

    #[test]
    fn a_restart_with_no_finishing_time_is_still_something_to_sort_by() {
        // Unranked under `recency` — there is no moment to rank it against the
        // dated restarts — but the count is real, and the ordering has already
        // lifted this pod clear of the healthy rows. Claiming there was nothing
        // to sort by would say so over a listing with a crashing pod near the
        // top of it.
        let rows = [row("api", 3, None), row("worker", 0, None)];

        assert!(ranks_any(&rows, Order::Restarts));
        assert_eq!(
            sorted(&rows, Order::Restarts, Direction::Natural),
            ["api", "worker"]
        );
    }

    #[test]
    fn one_restarted_pod_is_enough_for_restarts_to_have_ranked_something() {
        let rows = [row("api", 9, Some(5)), row("worker", 0, None)];

        assert!(ranks_any(&rows, Order::Restarts));
    }

    #[test]
    fn an_unsampled_listing_ranks_nothing_under_the_usage_orderings() {
        // No metrics-server, so no `CPU`/`MEMORY` columns to sort by.
        let rows = [using("api", None, None), using("worker", None, None)];

        assert!(!ranks_any(&rows, Order::Cpu));
        assert!(!ranks_any(&rows, Order::Memory));
    }

    #[test]
    fn one_sampled_pod_is_enough_for_the_usage_orderings_to_have_ranked() {
        let rows = [
            using("api", Some("250m"), None),
            using("worker", None, None),
        ];

        assert!(ranks_any(&rows, Order::Cpu));
        // Sampled for CPU and not for memory is a real shape: the two columns
        // are asked about separately, and so are the two orderings.
        assert!(!ranks_any(&rows, Order::Memory));
    }

    #[test]
    fn pods_with_no_creation_timestamp_rank_nothing_under_age() {
        assert!(!ranks_any(&[aged("api", None)], Order::Age));
        assert!(ranks_any(
            &[aged("api", None), aged("worker", Some(5))],
            Order::Age
        ));
    }

    #[test]
    fn every_pod_ranks_by_name() {
        assert!(ranks_any(&[row("api", 0, None)], Order::Name));
    }

    #[test]
    fn a_single_row_listing_distinguishes_nothing_under_any_ordering() {
        // Sorting one row is a no-op whatever the key, so nothing here can ever
        // put it anywhere it was not already — unlike `ranks_any`, which only
        // asks whether the row has a key at all.
        let rows = [row("only", 3, Some(5))];

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
        // Two pods using the exact same amount of CPU: `ranks_any` says yes for
        // both, and sorting between them changes nothing either way.
        let rows = [
            using("api", Some("250m"), None),
            using("worker", Some("250m"), None),
        ];

        assert!(ranks_any(&rows, Order::Cpu));
        assert!(!distinguishes(&rows, Order::Cpu));
    }

    #[test]
    fn pods_at_the_same_recency_but_different_counts_still_distinguish() {
        // A tie on the dated part of `restarts` falls through to the count,
        // which `rank` — and so `distinguishes` — already treats as the
        // tie-break; two pods that crashed at the same instant a different
        // number of times are still worth separating.
        let rows = [row("quiet", 1, Some(5)), row("noisy", 40, Some(5))];

        assert!(distinguishes(&rows, Order::Restarts));
    }

    #[test]
    fn the_metrics_footnote_explains_the_usage_orderings_and_nothing_else() {
        // `--sort cpu` with no metrics-server: the note above the table already
        // names the cause and links to the fix, so the sort note points at it
        // instead of writing the same paragraph again a line later.
        let missing = Missing { usage: true };

        assert_eq!(cause(Order::Cpu, missing), Cause::Explained);
        assert_eq!(cause(Order::Memory, missing), Cause::Explained);
    }

    #[test]
    fn a_namespace_where_nothing_has_crashed_has_no_footnote_to_point_at() {
        // The case the note was written for. Nothing failed — the pods are
        // healthy — so there is nothing above the table, and `restarts` must
        // account for itself.
        for missing in [Missing::default(), Missing { usage: true }] {
            assert_eq!(cause(Order::Restarts, missing), Cause::Unexplained);
        }
    }

    #[test]
    fn a_column_that_is_simply_empty_is_never_blamed_on_a_footnote() {
        // Nothing above the table said a word — which, now that a listing with
        // no usage at all earns a footnote of its own, is the listing whose
        // usage columns are *present* and half filled in: metrics-server
        // reporting memory for these pods and no cpu it could read leaves
        // `--sort cpu` with nothing to rank and nothing to point at.
        for order in ORDERS {
            assert_eq!(
                cause(order, Missing::default()),
                Cause::Unexplained,
                "{order:?}"
            );
        }
    }

    #[test]
    fn an_empty_listing_ranks_nothing_under_any_ordering() {
        // True but never printed: `render` drops every note when there are no
        // rows, because "nothing matched" is the whole answer.
        for order in ORDERS {
            assert!(!ranks_any(&[], order), "{order:?}");
        }
    }

    #[test]
    fn the_default_order_is_namespace_then_name() {
        // The order the existing listing has, unchanged: this is what `--sort`
        // is opt-in *from*.
        let mut rows = vec![
            in_namespace("storefront", "api"),
            in_namespace("kube-system", "coredns"),
            in_namespace("kube-system", "aws-node"),
        ];

        sort(&mut rows, Order::Name, Direction::Natural);

        assert_eq!(names(&rows), ["aws-node", "coredns", "api"]);
    }

    #[test]
    fn name_order_run_naturally_is_the_default() {
        // The direction's own defaulting is `k8s::order`'s test; this is about
        // which ordering a pod listing opens in.
        assert_eq!(Order::default(), Order::Name);
        assert_eq!(Direction::default(), Direction::Natural);
    }

    #[test]
    fn restart_order_puts_the_most_recent_crash_first() {
        // The whole point: the pod that restarted eight seconds ago is at the
        // top, not wherever the alphabet left it.
        let mut rows = vec![
            row("api", 3, Some(90)),
            row("ledger", 1, Some(2)),
            row("reconcile", 9, Some(20)),
        ];

        sort(&mut rows, Order::Restarts, Direction::Natural);

        assert_eq!(names(&rows), ["ledger", "reconcile", "api"]);
    }

    #[test]
    fn a_pod_that_has_never_restarted_sorts_last() {
        // A healthy pod at the top would defeat the sort — the listing would
        // open on exactly the rows nobody is looking for.
        let mut rows = vec![
            row("healthy", 0, None),
            row("crashing", 1, Some(1)),
            row("also-healthy", 0, None),
        ];

        sort(&mut rows, Order::Restarts, Direction::Natural);

        assert_eq!(names(&rows), ["crashing", "also-healthy", "healthy"]);
    }

    #[test]
    fn every_healthy_pod_still_lands_in_alphabetical_order() {
        // With nothing to rank them by, the fallback is the order they would
        // have had anyway, so the tail of the listing stays readable.
        let mut rows = vec![
            in_namespace("storefront", "checkout"),
            in_namespace("kube-system", "coredns"),
            in_namespace("kube-system", "aws-node"),
        ];

        sort(&mut rows, Order::Restarts, Direction::Natural);

        assert_eq!(names(&rows), ["aws-node", "coredns", "checkout"]);
    }

    #[test]
    fn a_restart_with_no_recorded_time_sorts_between_the_dated_and_the_never() {
        // The count is real, so burying it among the healthy pods would hide a
        // genuine crash; but there is no moment to rank it by, so it cannot be
        // claimed to be more recent than one that has a date.
        let mut rows = vec![
            row("healthy", 0, None),
            row("undated", 4, None),
            row("dated", 1, Some(300)),
        ];

        sort(&mut rows, Order::Restarts, Direction::Natural);

        assert_eq!(names(&rows), ["dated", "undated", "healthy"]);
    }

    #[test]
    fn a_tie_on_recency_is_broken_by_how_many_times() {
        // Same instant — two containers of one deployment killed by the same
        // node problem — so the count is the only thing left to say which is
        // worse.
        let mut rows = vec![
            row("quiet", 1, Some(5)),
            row("noisy", 40, Some(5)),
            row("middling", 7, Some(5)),
        ];

        sort(&mut rows, Order::Restarts, Direction::Natural);

        assert_eq!(names(&rows), ["noisy", "middling", "quiet"]);
    }

    #[test]
    fn rows_that_tie_on_everything_fall_back_to_the_alphabet() {
        // Two undated restarts with the same count have nothing left to
        // separate them; without this the listing could shuffle between runs.
        let mut rows = vec![row("zebra", 2, None), row("aardvark", 2, None)];

        sort(&mut rows, Order::Restarts, Direction::Natural);

        assert_eq!(names(&rows), ["aardvark", "zebra"]);
    }

    #[test]
    fn a_restart_in_the_future_does_not_break_the_ordering() {
        // Clock skew between the kubelet and the API server can date a restart
        // a few seconds ahead of `now`. It is still just the newest one.
        let mut rows = vec![row("past", 1, Some(10)), row("skewed", 1, Some(-1))];

        sort(&mut rows, Order::Restarts, Direction::Natural);

        assert_eq!(names(&rows), ["skewed", "past"]);
    }

    #[test]
    fn sorting_by_restarts_does_not_reorder_by_count_alone() {
        // The count is the tie-break, never the key. A pod that failed two
        // hundred times last week is history; the one that restarted a minute
        // ago is the incident.
        let mut rows = vec![
            row("last-week", 200, Some(10_000)),
            row("just-now", 1, Some(1)),
        ];

        sort(&mut rows, Order::Restarts, Direction::Natural);

        assert_eq!(names(&rows), ["just-now", "last-week"]);
    }

    // --- age ---

    #[test]
    fn age_order_puts_the_youngest_pod_first() {
        // Deliberately the opposite way round from `kubectl --sort-by=
        // .metadata.creationTimestamp`: during an incident the question the AGE
        // column answers is "what changed", and what changed is the new pod.
        let mut rows = vec![
            aged("old", Some(10_000)),
            aged("newest", Some(2)),
            aged("middling", Some(300)),
        ];

        sort(&mut rows, Order::Age, Direction::Natural);

        assert_eq!(names(&rows), ["newest", "middling", "old"]);
    }

    #[test]
    fn a_pod_with_no_creation_timestamp_sorts_last_by_age() {
        // The same field that leaves the AGE cell reading `-`. There is nothing
        // to rank it by, so it belongs with the other unrankable rows.
        let mut rows = vec![
            aged("undated", None),
            aged("old", Some(10_000)),
            aged("young", Some(1)),
        ];

        sort(&mut rows, Order::Age, Direction::Natural);

        assert_eq!(names(&rows), ["young", "old", "undated"]);
    }

    #[test]
    fn reversing_age_puts_the_longest_running_pod_first() {
        // The other reading of the same column: which of these has been up
        // since before the incident started.
        let mut rows = vec![
            aged("undated", None),
            aged("old", Some(10_000)),
            aged("young", Some(1)),
        ];

        sort(&mut rows, Order::Age, Direction::Reversed);

        assert_eq!(names(&rows), ["old", "young", "undated"]);
    }

    #[test]
    fn pods_created_in_the_same_instant_fall_back_to_the_alphabet() {
        // A Deployment's pods are created within milliseconds of each other and
        // can share a timestamp exactly, since `creationTimestamp` has only
        // second resolution.
        let mut rows = vec![aged("zebra", Some(5)), aged("aardvark", Some(5))];

        sort(&mut rows, Order::Age, Direction::Natural);

        assert_eq!(names(&rows), ["aardvark", "zebra"]);
    }

    // --- cpu and memory ---

    #[test]
    fn cpu_order_puts_the_biggest_consumer_first() {
        let mut rows = vec![
            using("idle", Some("2m"), None),
            using("hungry", Some("1500m"), None),
            using("steady", Some("250m"), None),
        ];

        sort(&mut rows, Order::Cpu, Direction::Natural);

        assert_eq!(names(&rows), ["hungry", "steady", "idle"]);
    }

    #[test]
    fn memory_order_puts_the_biggest_consumer_first() {
        // Ranked on the parsed quantity rather than the rendered cell: `900Mi`
        // and `1Gi` do not compare as strings, and the larger one is the
        // shorter one.
        let mut rows = vec![
            using("small", None, Some("64Mi")),
            using("large", None, Some("1Gi")),
            using("medium", None, Some("900Mi")),
        ];

        sort(&mut rows, Order::Memory, Direction::Natural);

        assert_eq!(names(&rows), ["large", "medium", "small"]);
    }

    #[test]
    fn cpu_and_memory_rank_a_listing_independently() {
        // A pod can be the top of one column and the bottom of the other; the
        // two orderings must not read each other's field.
        let rows = vec![
            using("compute", Some("2"), Some("32Mi")),
            using("cache", Some("5m"), Some("8Gi")),
        ];

        assert_eq!(
            sorted(&rows, Order::Cpu, Direction::Natural),
            ["compute", "cache"]
        );
        assert_eq!(
            sorted(&rows, Order::Memory, Direction::Natural),
            ["cache", "compute"]
        );
    }

    #[test]
    fn a_pod_with_no_sample_sorts_last_rather_than_as_zero() {
        // Reading an absent figure as `0` would be a claim about the pod when
        // it is a fact about the scraper: `unsampled` might be the busiest pod
        // in the namespace.
        let mut rows = vec![
            using("unsampled", None, None),
            using("busy", Some("900m"), None),
            using("quiet", Some("1m"), None),
        ];

        sort(&mut rows, Order::Cpu, Direction::Natural);

        assert_eq!(names(&rows), ["busy", "quiet", "unsampled"]);
    }

    #[test]
    fn a_pod_sampled_at_zero_still_outranks_one_with_no_sample() {
        // The distinction the previous test protects, stated the other way
        // round: a genuine zero is a measurement and belongs among the ranked.
        let mut rows = vec![
            using("unsampled", None, None),
            using("measured", Some("0"), None),
        ];

        sort(&mut rows, Order::Cpu, Direction::Natural);

        assert_eq!(names(&rows), ["measured", "unsampled"]);
    }

    // --- reversal ---

    #[test]
    fn reversing_the_default_order_walks_the_alphabet_backwards() {
        // `--sort-reverse` on its own is a sensible thing to type, and there is
        // no unrankable tail to protect here — every row has a name.
        let mut rows = vec![
            in_namespace("kube-system", "aws-node"),
            in_namespace("storefront", "api"),
            in_namespace("kube-system", "coredns"),
        ];

        sort(&mut rows, Order::Name, Direction::Reversed);

        assert_eq!(names(&rows), ["api", "coredns", "aws-node"]);
    }

    #[test]
    fn reversing_restarts_puts_the_oldest_crash_first() {
        let mut rows = vec![
            row("api", 3, Some(90)),
            row("ledger", 1, Some(2)),
            row("reconcile", 9, Some(20)),
        ];

        sort(&mut rows, Order::Restarts, Direction::Reversed);

        assert_eq!(names(&rows), ["api", "reconcile", "ledger"]);
    }

    #[test]
    fn reversing_cpu_puts_the_idlest_sampled_pod_first() {
        let mut rows = vec![
            using("hungry", Some("1500m"), None),
            using("idle", Some("2m"), None),
            using("steady", Some("250m"), None),
        ];

        sort(&mut rows, Order::Cpu, Direction::Reversed);

        assert_eq!(names(&rows), ["idle", "steady", "hungry"]);
    }

    #[test]
    fn reversing_a_usage_order_leaves_the_unsampled_pods_in_the_tail() {
        // The rule reversal exists to bend around: "least CPU" asks which pod
        // is idle, not which pod metrics-server has not reached. Flipping the
        // whole comparison would open the listing on every blank row.
        let rows = vec![
            using("unsampled", None, None),
            using("busy", Some("900m"), None),
            using("quiet", Some("1m"), None),
        ];

        assert_eq!(
            sorted(&rows, Order::Cpu, Direction::Reversed),
            ["quiet", "busy", "unsampled"]
        );
    }

    #[test]
    fn reversing_restarts_leaves_the_healthy_pods_in_the_tail() {
        // Same rule for the column it was first written for: a pod that has
        // never restarted has no place at the top of a restart ordering,
        // whichever way round the dated ones are being read.
        let rows = vec![
            row("healthy", 0, None),
            row("undated", 4, None),
            row("dated-old", 1, Some(300)),
            row("dated-new", 1, Some(3)),
        ];

        assert_eq!(
            sorted(&rows, Order::Restarts, Direction::Reversed),
            ["dated-old", "dated-new", "undated", "healthy"]
        );
    }

    #[test]
    fn reversing_never_moves_a_row_between_the_ranked_and_the_tail() {
        // Stated as the invariant rather than as one arrangement: whatever an
        // ordering has nothing to rank occupies exactly the end of the listing,
        // under either direction. `Order::Name` has no such rows — every pod
        // has a name — so for it this asserts nothing, which is correct.
        let rows = vec![
            using("sampled", Some("1"), Some("1Gi")),
            using("unsampled", None, None),
            aged("undated", None),
            row("healthy", 0, None),
            row("crashed", 2, Some(5)),
        ];

        // Read off the fields rather than from the ordering, so a change to how
        // a rank is derived has to keep agreeing with what the columns say.
        let unrankable = |row: &PodRow, order: Order| match order {
            Order::Name => false,
            Order::Restarts => row.last_restart.is_none(),
            Order::Age => row.created_at.is_none(),
            Order::Cpu => row.cpu_used.is_none(),
            Order::Memory => row.memory_used.is_none(),
        };

        for order in ORDERS {
            let mut expected: Vec<String> = rows
                .iter()
                .filter(|row| unrankable(row, order))
                .map(|row| row.name.clone())
                .collect();
            expected.sort();

            for direction in DIRECTIONS {
                let listing = sorted(&rows, order, direction);
                let mut tail = listing[listing.len() - expected.len()..].to_vec();
                tail.sort();

                assert_eq!(
                    tail, expected,
                    "{order:?} {direction:?} did not leave its unrankable rows in the tail"
                );
            }
        }
    }

    // --- properties that hold of every ordering ---

    #[test]
    fn the_ordering_does_not_depend_on_the_order_it_started_in() {
        // A total order, checked by sorting the same rows from a different
        // starting arrangement: an ordering that is only *nearly* total shows
        // up as a listing that changes shape between two runs of one command.
        let rows = vec![
            row("a", 2, Some(5)),
            row("b", 2, Some(5)),
            row("c", 0, None),
            row("d", 3, None),
            aged("e", None),
            aged("f", Some(5)),
            using("g", Some("100m"), Some("1Gi")),
            using("h", Some("100m"), Some("1Gi")),
            using("i", None, None),
        ];
        let mut reversed_input = rows.clone();
        reversed_input.reverse();

        for order in ORDERS {
            for direction in DIRECTIONS {
                assert_eq!(
                    sorted(&rows, order, direction),
                    sorted(&reversed_input, order, direction),
                    "{order:?} {direction:?} is not a total order"
                );
            }
        }
    }

    #[test]
    fn an_empty_listing_sorts_to_an_empty_listing() {
        // `eks pods --sort cpu` in an empty namespace is an ordinary thing to
        // type, and must not be a special case anywhere.
        for order in ORDERS {
            for direction in DIRECTIONS {
                let mut rows: Vec<PodRow> = Vec::new();
                sort(&mut rows, order, direction);
                assert!(rows.is_empty());
            }
        }
    }

    #[test]
    fn a_single_row_is_left_alone_by_every_order() {
        for order in ORDERS {
            for direction in DIRECTIONS {
                let mut rows = vec![row("api", 3, Some(9))];
                sort(&mut rows, order, direction);
                assert_eq!(names(&rows), ["api"]);
            }
        }
    }

    #[test]
    fn a_listing_with_nothing_to_rank_falls_back_to_the_alphabet_either_way() {
        // Every ordering degenerates to the same listing when no row carries
        // the column it sorts on — a cluster with no metrics-server, under
        // `--sort cpu`, is exactly this.
        let rows = vec![
            using("zebra", None, None),
            using("aardvark", None, None),
            using("mongoose", None, None),
        ];

        for direction in DIRECTIONS {
            for order in [Order::Cpu, Order::Memory] {
                assert_eq!(
                    sorted(&rows, order, direction),
                    ["aardvark", "mongoose", "zebra"],
                    "{order:?} {direction:?}"
                );
            }
        }
    }

    #[test]
    fn the_order_survives_into_the_rendered_table() {
        // The sort is only worth anything if it is what lands on screen, and
        // rendering takes the rows in whatever order it is handed them — so
        // this is the assertion that the two halves are actually connected.
        let mut rows = vec![
            row("aws-node-4kd9p", 0, None),
            row("reconcile-5d4b9-nzk8p", 9, Some(5)),
            row("api-7c9f6d4b8-x2vnq", 2, Some(90)),
        ];

        sort(&mut rows, Order::Restarts, Direction::Natural);
        let table = super::super::render(
            &rows,
            "prod (us-east-1)",
            &super::super::Scope::Namespace("payments".to_owned()),
            &super::super::Selectors::default(),
            &[],
            crate::format::Width::Default,
            crate::theme::Palette::Plain,
        );

        let printed: Vec<&str> = table
            .lines()
            .skip(1)
            .filter_map(|line| line.split_whitespace().next())
            .collect();
        assert_eq!(
            printed,
            [
                "reconcile-5d4b9-nzk8p",
                "api-7c9f6d4b8-x2vnq",
                "aws-node-4kd9p"
            ]
        );
    }

    #[test]
    fn a_reversed_usage_order_survives_into_the_rendered_table_too() {
        // The reversal has to reach the screen as well, and the usage columns
        // only appear at all when some row carries a figure.
        let mut rows = vec![
            using("hungry", Some("1500m"), Some("2Gi")),
            using("unsampled", None, None),
            using("idle", Some("2m"), Some("16Mi")),
        ];

        sort(&mut rows, Order::Cpu, Direction::Reversed);
        let table = super::super::render(
            &rows,
            "prod (us-east-1)",
            &super::super::Scope::Namespace("payments".to_owned()),
            &super::super::Selectors::default(),
            &[],
            crate::format::Width::Default,
            crate::theme::Palette::Plain,
        );

        let printed: Vec<&str> = table
            .lines()
            .skip(1)
            .filter_map(|line| line.split_whitespace().next())
            .collect();
        assert_eq!(printed, ["idle", "hungry", "unsampled"]);
    }

    #[test]
    fn every_ordering_names_itself_under_the_table() {
        // As for `eks nodes`: the note lives in `k8s::order`, but the names in
        // it are this enum's, and a renamed variant must not be able to change
        // the note without a test noticing.
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
    fn the_pod_and_node_tables_word_a_shared_ordering_identically() {
        // `cpu` means the same thing to a reader of either table, so it has to
        // read the same under both. The two enums are separate types and
        // nothing but this test stops them drifting apart.
        for direction in DIRECTIONS {
            assert_eq!(
                crate::k8s::order::note(Order::Cpu, direction),
                crate::k8s::order::note(crate::k8s::nodes::Order::Cpu, direction),
                "{direction:?}"
            );
        }
    }
}
