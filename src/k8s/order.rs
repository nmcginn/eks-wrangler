//! The half of "sort a listing" that is the same for every listing.
//!
//! `eks nodes` and `eks pods` both print tables of numbers, and both have the
//! same problem when they print them alphabetically: the row a person went
//! looking for — the pod that just crashed, the node at 97% — sits wherever its
//! name puts it. Both answer it with a `--sort` and a `--sort-reverse`, and the
//! rules those two flags follow have to be one set of rules, or `--sort cpu`
//! means something subtly different depending on which table is on screen.
//!
//! What is shared is the *shape* of an ordering: which way round it runs, and
//! what happens to a row it cannot rank. What is not shared is the keys — a
//! node has no restart count and a pod has no kubelet version — so each listing
//! keeps its own `Order` enum and its own `sort`, and borrows [`Direction`] and
//! the crate-private `Rank` from here.
//!
//! # Which way is "first"
//!
//! Every ordering puts the row a person went looking for at the top: the newest
//! restart, the largest usage figure, the youngest row, the least healthy
//! status. That is deliberately not `kubectl --sort-by=.metadata
//! .creationTimestamp`, which prints oldest first — but one rule across every
//! ordering in the tool beats matching another tool on one of them.
//!
//! # The tail
//!
//! [`Direction::Reversed`] flips that, and flips only that. The rows an
//! ordering has *nothing to rank* — a pod that has never restarted under
//! `restarts`, a node metrics-server has not sampled under `cpu` — stay in the
//! tail under either direction, because "least CPU" means the row using the
//! least, not the row whose usage is unknown. Reversing the whole comparison
//! would open every reversed listing on exactly the rows nobody asked about.
//!
//! `Rank` is what keeps those two halves of a comparison apart, and `compare`
//! is the one place the rule is written down.

use std::cmp::Ordering;

/// Which way round an ordering runs.
///
/// A value rather than a `bool` parameter, because `sort(&mut rows, order,
/// true)` at a call site says nothing about what `true` does to the listing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Direction {
    /// The ordering's own direction: newest, largest, worst, or A-to-Z first.
    #[default]
    Natural,
    /// The ordering, flipped — except for the rows it has nothing to rank,
    /// which stay in the tail. See the module docs.
    Reversed,
}

impl Direction {
    /// `Reversed` when the flag was given, `Natural` otherwise.
    #[must_use]
    pub fn reversed(yes: bool) -> Self {
        if yes { Self::Reversed } else { Self::Natural }
    }

    /// Flip a comparison if this direction is reversed.
    pub(crate) fn apply(self, ordering: Ordering) -> Ordering {
        match self {
            Self::Natural => ordering,
            Self::Reversed => ordering.reverse(),
        }
    }
}

/// Where a row sits under an ordering, before the direction is applied.
///
/// The two cases are kept apart rather than folded into an `Option` because
/// only the ranked half is reversible — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rank<T> {
    /// Something to rank the row by. These are what a reversal flips.
    By(T),
    /// Nothing to rank the row by, in the tail tier given — a lower tier first.
    ///
    /// Tiers exist because "cannot rank this" is not always one thing. Under
    /// `restarts`, a restart the kubelet recorded no finishing time for is a
    /// different blank from never having restarted: the count is real, so
    /// burying it among the healthy pods would hide a genuine crash, but there
    /// is no moment to rank it against the dated ones either. Under a node's
    /// `cpu` the same split is a figure with no allocatable to divide it by,
    /// against no figure at all.
    Unranked(u8),
}

/// Compare two ranks, flipping only the part a direction is allowed to flip.
pub(crate) fn compare<T: Ord>(a: &Rank<T>, b: &Rank<T>, direction: Direction) -> Ordering {
    match (a, b) {
        (Rank::By(a), Rank::By(b)) => direction.apply(a.cmp(b)),
        // Ranked before unranked whichever way round the listing runs.
        (Rank::By(_), Rank::Unranked(_)) => Ordering::Less,
        (Rank::Unranked(_), Rank::By(_)) => Ordering::Greater,
        (Rank::Unranked(a), Rank::Unranked(b)) => a.cmp(b),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const DIRECTIONS: [Direction; 2] = [Direction::Natural, Direction::Reversed];

    #[test]
    fn a_listing_runs_the_natural_way_round_unless_asked() {
        assert_eq!(Direction::default(), Direction::Natural);
        assert_eq!(Direction::reversed(false), Direction::Natural);
        assert_eq!(Direction::reversed(true), Direction::Reversed);
    }

    #[test]
    fn reversing_flips_a_ranked_comparison() {
        let (first, second) = (Rank::By(1), Rank::By(2));

        assert_eq!(compare(&first, &second, Direction::Natural), Ordering::Less);
        assert_eq!(
            compare(&first, &second, Direction::Reversed),
            Ordering::Greater
        );
    }

    #[test]
    fn an_unrankable_row_stays_behind_a_ranked_one_in_either_direction() {
        // The rule the whole module exists for, asserted on the primitive
        // rather than on one listing's rows, so neither table can lose it.
        for direction in DIRECTIONS {
            assert_eq!(
                compare(&Rank::By(1), &Rank::<i32>::Unranked(0), direction),
                Ordering::Less,
                "{direction:?}"
            );
            assert_eq!(
                compare(&Rank::<i32>::Unranked(0), &Rank::By(1), direction),
                Ordering::Greater,
                "{direction:?}"
            );
        }
    }

    #[test]
    fn tail_tiers_keep_their_own_order_under_a_reversal() {
        // A reversal is about the ranked rows; the tail is not a ranking and
        // does not turn over with them.
        for direction in DIRECTIONS {
            assert_eq!(
                compare(&Rank::<i32>::Unranked(0), &Rank::Unranked(1), direction),
                Ordering::Less,
                "{direction:?}"
            );
        }
    }

    #[test]
    fn equal_ranks_compare_equal_so_a_caller_can_tie_break() {
        for direction in DIRECTIONS {
            assert_eq!(
                compare(&Rank::By(7), &Rank::By(7), direction),
                Ordering::Equal,
                "{direction:?}"
            );
        }
    }
}
