//! How a pod listing is ordered.
//!
//! The default is alphabetical, which is the right answer for reading a
//! namespace and the wrong one for the question the `RESTARTS` column exists to
//! answer. "What is crashing *now*" is a sort, not a scan: in a hundred-row
//! listing the pod that restarted eight seconds ago sits wherever its name puts
//! it, indistinguishable at a glance from the ninety-nine healthy ones.
//!
//! [`sort`] is a pure function over the rows — no clock, no cluster, because
//! [`PodRow`] already carries the instant each restart finished. Every ordering
//! here is total: two rows only ever compare equal if they are the same pod in
//! the same namespace, so one listing always renders the same way twice.

use std::cmp::{Ordering, Reverse};

use k8s_openapi::jiff::Timestamp;

use super::PodRow;

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
}

/// Sort a listing in place.
pub fn sort(rows: &mut [PodRow], order: Order) {
    let compare = match order {
        Order::Name => by_name,
        Order::Restarts => by_restarts,
    };
    rows.sort_by(compare);
}

/// Namespace, then name.
///
/// Namespace leads because that is what the `NAMESPACE` column implies about
/// the listing it heads, and it is the order `kubectl get pods -A` uses.
fn by_name(a: &PodRow, b: &PodRow) -> Ordering {
    name_key(a).cmp(&name_key(b))
}

/// Most recently restarted first.
///
/// The count is only the tie-break, not the key. Sorting by how *many* times a
/// pod has restarted answers a different question — it puts a pod that failed
/// two hundred times last week above one that started crashing a minute ago,
/// which is precisely backwards during an incident.
fn by_restarts(a: &PodRow, b: &PodRow) -> Ordering {
    recency(a)
        .cmp(&recency(b))
        // A tie among the undated — and among the never-restarted, where every
        // row ties — is broken by count and then alphabetically, so the
        // ordering stays total and a listing cannot shuffle between runs.
        .then_with(|| Reverse(a.restarts).cmp(&Reverse(b.restarts)))
        .then_with(|| by_name(a, b))
}

fn name_key(row: &PodRow) -> (&str, &str) {
    (&row.namespace, &row.name)
}

/// Where a row sits in restart order.
///
/// The variants are declared in the order they sort, so the derived `Ord` is
/// the ranking: a dated restart (newest first, hence [`Reverse`]), then a
/// restart the kubelet recorded no finishing time for, then a pod that has
/// never restarted at all.
///
/// The undated middle rank is the one worth spelling out. Such a restart did
/// happen — the count is real — so sorting it in with the never-restarted pods
/// would bury a genuine crash; but there is no moment to rank it against the
/// dated ones either, and inventing one would put a pod somewhere it has not
/// earned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Recency {
    At(Reverse<Timestamp>),
    Undated,
    Never,
}

fn recency(row: &PodRow) -> Recency {
    match row.last_restart {
        Some(at) => Recency::At(Reverse(at)),
        // Defensive about the pairing rather than trusting it: `restarts` and
        // `last_restart` are set together in `PodRow::from_pod`, and a count
        // with no date is the honest reading if that ever comes apart.
        None if row.restarts > 0 => Recency::Undated,
        None => Recency::Never,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use k8s_openapi::jiff::SignedDuration;

    use super::*;
    use crate::theme::Severity;

    fn now() -> Timestamp {
        "2026-08-18T12:00:00Z".parse().unwrap()
    }

    /// A row, named, with a restart count and when the newest one finished.
    ///
    /// Built directly rather than through `PodRow::from_pod`: the ordering is a
    /// function of two fields, and going via a `Pod` would mean arranging
    /// container statuses to say something these tests are not about.
    fn row(name: &str, restarts: i32, restarted_minutes_ago: Option<i64>) -> PodRow {
        let last_restart =
            restarted_minutes_ago.map(|mins| now() - SignedDuration::from_mins(mins));

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
            cpu_used: None,
            memory_used: None,
            node: "ip-10-0-1-9.ec2.internal".to_owned(),
        }
    }

    fn in_namespace(namespace: &str, name: &str) -> PodRow {
        PodRow {
            namespace: namespace.to_owned(),
            ..row(name, 0, None)
        }
    }

    fn names(rows: &[PodRow]) -> Vec<&str> {
        rows.iter().map(|row| row.name.as_str()).collect()
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

        sort(&mut rows, Order::Name);

        assert_eq!(names(&rows), ["aws-node", "coredns", "api"]);
    }

    #[test]
    fn name_order_is_the_default_order() {
        assert_eq!(Order::default(), Order::Name);
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

        sort(&mut rows, Order::Restarts);

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

        sort(&mut rows, Order::Restarts);

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

        sort(&mut rows, Order::Restarts);

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

        sort(&mut rows, Order::Restarts);

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

        sort(&mut rows, Order::Restarts);

        assert_eq!(names(&rows), ["noisy", "middling", "quiet"]);
    }

    #[test]
    fn rows_that_tie_on_everything_fall_back_to_the_alphabet() {
        // Two undated restarts with the same count have nothing left to
        // separate them; without this the listing could shuffle between runs.
        let mut rows = vec![row("zebra", 2, None), row("aardvark", 2, None)];

        sort(&mut rows, Order::Restarts);

        assert_eq!(names(&rows), ["aardvark", "zebra"]);
    }

    #[test]
    fn the_ordering_does_not_depend_on_the_order_it_started_in() {
        // A total order, checked by sorting the same rows from a different
        // starting arrangement: an ordering that is only *nearly* total shows
        // up as a listing that changes shape between two runs of one command.
        let sorted = |mut rows: Vec<PodRow>| {
            sort(&mut rows, Order::Restarts);
            rows.iter().map(|row| row.name.clone()).collect::<Vec<_>>()
        };

        let rows = vec![
            row("a", 2, Some(5)),
            row("b", 2, Some(5)),
            row("c", 0, None),
            row("d", 3, None),
        ];
        let mut reversed = rows.clone();
        reversed.reverse();

        assert_eq!(sorted(rows), sorted(reversed));
    }

    #[test]
    fn a_restart_in_the_future_does_not_break_the_ordering() {
        // Clock skew between the kubelet and the API server can date a restart
        // a few seconds ahead of `now`. It is still just the newest one.
        let mut rows = vec![row("past", 1, Some(10)), row("skewed", 1, Some(-1))];

        sort(&mut rows, Order::Restarts);

        assert_eq!(names(&rows), ["skewed", "past"]);
    }

    #[test]
    fn an_empty_listing_sorts_to_an_empty_listing() {
        // `eks pods --sort restarts` in an empty namespace is an ordinary
        // thing to type, and must not be a special case anywhere.
        for order in [Order::Name, Order::Restarts] {
            let mut rows: Vec<PodRow> = Vec::new();
            sort(&mut rows, order);
            assert!(rows.is_empty());
        }
    }

    #[test]
    fn a_single_row_is_left_alone_by_either_order() {
        for order in [Order::Name, Order::Restarts] {
            let mut rows = vec![row("api", 3, Some(9))];
            sort(&mut rows, order);
            assert_eq!(names(&rows), ["api"]);
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

        sort(&mut rows, Order::Restarts);
        let table = super::super::render(
            &rows,
            "prod (us-east-1)",
            &super::super::Scope::Namespace("payments".to_owned()),
            &super::super::Selectors::default(),
            &[],
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
    fn sorting_by_restarts_does_not_reorder_by_count_alone() {
        // The count is the tie-break, never the key. A pod that failed two
        // hundred times last week is history; the one that restarted a minute
        // ago is the incident.
        let mut rows = vec![
            row("last-week", 200, Some(10_000)),
            row("just-now", 1, Some(1)),
        ];

        sort(&mut rows, Order::Restarts);

        assert_eq!(names(&rows), ["just-now", "last-week"]);
    }
}
