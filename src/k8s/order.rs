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
//!
//! # Saying which way round it ran
//!
//! A sorted table looks exactly like an unsorted one to anyone who did not type
//! the command, and a reversed one looks like the ordering running the other
//! way. [`note`] is the line under the table that says, and it is generic over
//! the `Order` enums rather than written once per listing — it needs only the
//! name `clap` already knows the value by, so `--sort cpu-requested` and the
//! note under the table cannot start spelling the ordering differently.

use std::cmp::Ordering;

use clap::ValueEnum;

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

/// A line naming the ordering a listing is in, or `None` when there is nothing
/// worth saying.
///
/// Pure in both arguments and blind to the rows, so it says which ordering was
/// *asked for* rather than what the ordering happened to do to this particular
/// table. Whether an ordering managed to rank anything is a separate question,
/// and a separate note.
///
/// Silent for a listing nobody reordered, which keeps the default output of
/// every command exactly as it was. "Nobody reordered it" means both halves:
/// `--sort-reverse` on its own reverses the default ordering, so it prints a
/// table that is genuinely not the default one and says so.
///
/// The name is `clap`'s own — the text the user typed after `--sort` — so a
/// renamed value cannot leave the note describing the old spelling. A value
/// `clap` will not name is one hidden from `--help`; there is no honest thing
/// to call it, so the note is dropped rather than guessed at.
#[must_use]
pub fn note<O>(order: O, direction: Direction) -> Option<String>
where
    // By value, and `Copy` so that it can be — the call site should read like
    // the `sort` call it sits next to, and an `Order` is a fieldless enum.
    O: ValueEnum + Copy + Default + PartialEq,
{
    if order == O::default() && direction == Direction::Natural {
        return None;
    }

    let name = order.to_possible_value()?;
    let name = name.get_name();

    Some(match direction {
        Direction::Natural => format!("Sorted by {name}."),
        // Named after the flag that did it, so the line doubles as the way back
        // to the other reading of the same column.
        Direction::Reversed => format!("Sorted by {name}, reversed."),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const DIRECTIONS: [Direction; 2] = [Direction::Natural, Direction::Reversed];

    /// A stand-in for the real `Order` enums.
    ///
    /// `note` is generic, and the guarantees below are about the generic
    /// function rather than about either listing's set of orderings — which is
    /// also why the multi-word variant is here: it is what proves the note
    /// spells an ordering the way the flag does.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
    enum TestOrder {
        #[default]
        Name,
        CpuRequested,
    }

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

    #[test]
    fn a_listing_nobody_reordered_says_nothing_about_its_order() {
        // The whole reason the note is an `Option`: every existing command's
        // output has to be unchanged to the byte for the people who never
        // touched `--sort`.
        assert_eq!(note(TestOrder::Name, Direction::Natural), None);
    }

    #[test]
    fn an_ordering_names_itself_the_way_the_flag_spells_it() {
        // `cpu-requested`, not `CpuRequested`: the note has to be the text the
        // user would type, or it is telling them about a flag value that does
        // not exist.
        assert_eq!(
            note(TestOrder::CpuRequested, Direction::Natural).as_deref(),
            Some("Sorted by cpu-requested.")
        );
    }

    #[test]
    fn a_reversed_listing_says_which_way_round_it_ran() {
        assert_eq!(
            note(TestOrder::CpuRequested, Direction::Reversed).as_deref(),
            Some("Sorted by cpu-requested, reversed.")
        );
    }

    #[test]
    fn reversing_the_default_ordering_is_still_worth_saying() {
        // `--sort-reverse` on its own is accepted and prints Z-to-A. The order
        // is the default one, but the listing is not the default listing, and
        // it is the one most likely to be mistaken for it.
        assert_eq!(
            note(TestOrder::Name, Direction::Reversed).as_deref(),
            Some("Sorted by name, reversed.")
        );
    }

    #[test]
    fn every_ordering_but_the_untouched_default_has_something_to_say() {
        for direction in DIRECTIONS {
            for order in [TestOrder::Name, TestOrder::CpuRequested] {
                let quiet = order == TestOrder::default() && direction == Direction::Natural;
                assert_eq!(
                    note(order, direction).is_none(),
                    quiet,
                    "{order:?} {direction:?}"
                );
            }
        }
    }
}
