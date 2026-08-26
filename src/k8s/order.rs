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
//!
//! [`unranked_note`] is the second half of that, for the listing where the
//! ordering ranked nothing at all: `--sort cpu` on a cluster with no
//! metrics-server sorts by a column that is not in the table, and `Sorted by
//! cpu.` on its own then describes rows the alphabet arranged.
//!
//! Saying so is honest but it is not yet advice, so the note also says what to
//! type instead: the orderings that *would* have ranked something, worked out
//! from the rows in front of the user rather than listed by hand. What it does
//! not do is give the same advice twice. When the column is missing because
//! something above already said so — no metrics-server, a pod listing the
//! cluster refused — that footnote has already named the cause and linked to
//! the fix, and [`Cause::Explained`] points at it rather than repeating it a
//! line later.
//!
//! Ranking something is not the same as being worth suggesting. `--sort
//! status` on a cluster where every node is `Ready` ranks every row — a node
//! always has a status — so the old rule offered it as the fix for an ordering
//! that ranked nothing, and `--sort status` there reorders nothing at all: the
//! reader is sent to a second listing identical to the first. The advice list
//! is filtered by a stricter question than the diagnosis is: not just "does
//! this ordering rank a row" but "would it put two of these rows in a
//! different arrangement than they are already in". [`unranked_note`] takes
//! both answers from the listing, because only the listing's own rows can say.
//!
//! Both notes are blind to the rows on purpose — the keys are the part of an
//! ordering this module does not know — so the listing hands in what it alone
//! can answer: which orderings its rows can be ranked by, and which of its own
//! footnotes covers the column that came up empty.

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

impl<T> Rank<T> {
    /// Whether the ordering found something to rank this row by.
    ///
    /// Which tail tier an unranked row landed in is not the question here: the
    /// note under the table is about an ordering that ranked *nothing*, and a
    /// listing sorted entirely into its tail tiers is still one the flag did
    /// not order.
    pub(crate) fn is_ranked(&self) -> bool {
        matches!(self, Self::By(_))
    }
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

/// Whether the notes already above a table account for a missing column.
///
/// The unranked note is the last line under a listing, and it is not the only
/// line there. `eks nodes --sort cpu` on a cluster with no metrics-server has
/// already been told, two paragraphs up, that live usage could not be read and
/// where to get metrics-server; saying it again in different words would be the
/// same paragraph twice, a line apart. `eks pods --sort restarts` in a namespace
/// where nothing has ever crashed has nothing above it at all.
///
/// Which of a table's own footnotes covers which column is the listing's
/// knowledge rather than this module's, so the listing decides and this module
/// only chooses the wording. A value rather than a `bool` parameter, for the
/// reason [`Direction`] is one: `unranked_note(order, true, ranks)` says nothing
/// at the call site about what `true` does to the sentence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Cause {
    /// A note further up already says why the column is missing and what to do
    /// about it. The unranked note points back at it instead of restating it.
    ///
    /// "For the reason above" rather than "the note above says why", because
    /// the paragraph directly above is [`note`]'s `Sorted by cpu.` line, which
    /// gives no reason at all. A reason is the one thing up there that can only
    /// be the failure footnote.
    Explained,
    /// Nothing above says anything about it: the column is in the table and
    /// every cell in it is simply empty. The default, because a listing that
    /// has not said why has not said why.
    #[default]
    Unexplained,
}

impl Cause {
    /// `Explained` when a note above covers the column this ordering ranks on.
    #[must_use]
    pub fn explained(yes: bool) -> Self {
        if yes {
            Self::Explained
        } else {
            Self::Unexplained
        }
    }
}

/// A line saying an ordering found nothing at all to rank, and what to sort by
/// instead — or `None` when the ordering ranked something.
///
/// `ranks` is the one thing this module cannot work out for itself: whether any
/// row in the listing carries the figure a given ordering sorts on.
/// [`crate::k8s::nodes::ranks_any`] and [`crate::k8s::pods::ranks_any`] answer
/// it, as pure functions over the finished rows. It is asked about every
/// ordering rather than only the one the user typed, because the answer for the
/// others is exactly the advice this note owes them.
///
/// `distinguishes` is the second question, asked only of the orderings `ranks`
/// already said yes to: would sorting by this one actually rearrange these
/// rows, or would every row land back where it started? [`crate::k8s::nodes::
/// distinguishes`] and [`crate::k8s::pods::distinguishes`] answer it. An
/// ordering can rank every row and still distinguish none of them — a cluster
/// where every node is `Ready` ranks every row under `status`, and reorders
/// none of them — so the advice list needs both answers, not just the first.
///
/// `eks nodes --sort cpu` against a cluster with no metrics-server is the case
/// this exists for. There is no `CPU USE` column, every row is unranked, and the
/// alphabet decides the whole listing — and [`note`] then prints `Sorted by
/// cpu.` underneath, naming an ordering that did nothing.
///
/// So the note says two things:
///
/// ```text
/// Nothing here has cpu to sort by, for the reason above.
/// Sort by status, cpu-requested, memory-requested, or age instead.
/// ```
///
/// The first line is the diagnosis, and [`Cause`] decides whether it points at
/// an explanation that is already on screen. The second is the advice, and it
/// is dropped entirely rather than invented when there is nothing left to
/// suggest — a listing where no ordering can rank a row has no next command to
/// offer, and `Sort by nothing instead.` would be worse than silence.
///
/// Silent when the user asked for no ordering, so a command nobody passed
/// `--sort` to prints what it always printed. The direction is not a parameter:
/// reversing an ordering that ranked nothing is still an ordering that ranked
/// nothing, and the advice is the same either way.
///
/// It deliberately stops short of saying what order the rows came out in
/// instead. Unranked rows keep their tail tiers, and a listing split across two
/// of them is grouped by something even when nothing in it ranked, so "this is
/// in name order" would be a guess dressed as an explanation.
#[must_use]
pub fn unranked_note<O>(
    order: O,
    cause: Cause,
    ranks: impl Fn(O) -> bool,
    distinguishes: impl Fn(O) -> bool,
) -> Option<String>
where
    O: ValueEnum + Copy + Default + PartialEq,
{
    if order == O::default() || ranks(order) {
        return None;
    }

    // `clap`'s name again, for the same reason [`note`] uses it: the two notes
    // sit one under the other, and they must call the ordering the same thing.
    // The advice below spells its suggestions the same way for the same reason —
    // every ordering named here is a value the user can type after `--sort`.
    let name = order.to_possible_value()?;
    let name = name.get_name();

    let diagnosis = match cause {
        Cause::Explained => format!("Nothing here has {name} to sort by, for the reason above."),
        Cause::Unexplained => format!("Nothing here has {name} to sort by."),
    };

    Some(match alternatives(&ranks, &distinguishes) {
        Some(alternatives) => format!("{diagnosis}\nSort by {alternatives} instead."),
        None => diagnosis,
    })
}

/// The orderings that would both rank and actually rearrange at least one pair
/// of these rows, written out as `a`, `a or b`, or `a, b, or c`.
///
/// Computed from the rows rather than from the `Order` enum alone, so the advice
/// cannot name an ordering that would have failed the same way the one the user
/// typed did. `None` when there is nothing to suggest.
///
/// Two filters, not one. `ranks` is the existing bar — the one that also
/// decides whether an ordering counts as unranked in the first place — and it
/// is not enough on its own here: a node's `status` ranks every row (every
/// node has one), but on a cluster where they are all `Ready` it sorts nothing
/// relative to anything else, and suggesting it sends the reader to a listing
/// identical to the one that just told them nothing worked. `distinguishes`
/// is that second, stricter question, so a candidate has to both have
/// something to rank a row by *and* actually put two rows in a different
/// order before it earns a place in the advice.
///
/// The default ordering is left out on purpose. It is what dropping `--sort`
/// altogether gives you, so "sort by name instead" is advice to type a flag in
/// order to get the listing you would have got without one — and on a table
/// where nothing else ranks either, dropping the advice line says "there is
/// nothing else here to sort by" far better than a suggestion nobody needs.
///
/// Orderings hidden from `--help` are left out too. Suggesting a flag value the
/// user cannot find any other way is a worse answer than suggesting none.
///
/// The ordering that just failed needs no filter of its own, which is why it is
/// not even a parameter: it is being advised about precisely because `ranks`
/// said no to it, and `ranks` is a pure function over the rows, so the same
/// question gets the same answer here.
fn alternatives<O>(ranks: &impl Fn(O) -> bool, distinguishes: &impl Fn(O) -> bool) -> Option<String>
where
    O: ValueEnum + Copy + Default + PartialEq,
{
    let names: Vec<String> = O::value_variants()
        .iter()
        .copied()
        .filter(|&candidate| candidate != O::default())
        .filter(|&candidate| ranks(candidate))
        .filter(|&candidate| distinguishes(candidate))
        .filter_map(|candidate| candidate.to_possible_value())
        .filter(|value| !value.is_hide_set())
        .map(|value| value.get_name().to_owned())
        .collect();

    // The serial comma, and everything else about writing a list out as prose,
    // is `format::list`'s: the same rule writes the footnote naming the columns
    // a failed pod listing emptied.
    crate::format::list(&names, "or")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const DIRECTIONS: [Direction; 2] = [Direction::Natural, Direction::Reversed];

    /// A stand-in for the real `Order` enums.
    ///
    /// `note` and `unranked_note` are generic, and the guarantees below are
    /// about the generic functions rather than about either listing's set of
    /// orderings. The shape of this enum is chosen for what it can prove: a
    /// multi-word variant, because it is what shows the notes spell an ordering
    /// the way the flag does; three ordinary variants beyond the default,
    /// because the advice has to read as a list of one, of two, and of more;
    /// and a hidden one, because a flag value nobody can find in `--help` is not
    /// advice.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
    enum TestOrder {
        #[default]
        Name,
        Restarts,
        CpuRequested,
        Memory,
        #[value(hide = true)]
        Secret,
    }

    /// A `ranks` or `distinguishes` predicate for a listing where exactly these
    /// orderings found something to rank, or actually rearranged a row.
    ///
    /// The real ones close over the rows and ask `ranks_any`/`distinguishes`;
    /// what the generic function needs from either is only the answer, so the
    /// fixture is the answer. Shared between the two parameters because most
    /// tests below are not about the difference between them — an ordering
    /// that ranks a row in these fixtures also rearranges it, which is the
    /// ordinary case the split exists to be an exception to.
    fn ranking(ranked: &[TestOrder]) -> impl Fn(TestOrder) -> bool + Copy + '_ {
        move |order| ranked.contains(&order)
    }

    /// The listing nothing at all can be sorted by.
    fn ranks_nothing(_: TestOrder) -> bool {
        false
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
    fn a_rank_says_whether_it_ranked_anything() {
        assert!(Rank::By(1).is_ranked());
        assert!(!Rank::<i32>::Unranked(0).is_ranked());
        // Both tail tiers are the same answer to this question: an ordering
        // that put every row in a tier ordered none of them.
        assert!(!Rank::<i32>::Unranked(1).is_ranked());
    }

    #[test]
    fn a_cause_is_explained_only_when_a_listing_says_it_is() {
        assert_eq!(Cause::explained(true), Cause::Explained);
        assert_eq!(Cause::explained(false), Cause::Unexplained);
        // A listing that has not said why has not said why.
        assert_eq!(Cause::default(), Cause::Unexplained);
    }

    #[test]
    fn an_ordering_that_ranked_nothing_says_so_and_what_to_sort_by_instead() {
        // `eks nodes --sort cpu` with no metrics-server: the `Sorted by cpu.`
        // line above this one is about a column the table does not have, and a
        // user who has just been told their flag did nothing is owed the flag
        // that would do something.
        let ranks = ranking(&[TestOrder::Memory]);
        assert_eq!(
            unranked_note(TestOrder::CpuRequested, Cause::Unexplained, ranks, ranks).as_deref(),
            Some("Nothing here has cpu-requested to sort by.\nSort by memory instead.")
        );
    }

    #[test]
    fn an_explained_column_is_pointed_at_rather_than_explained_a_second_time() {
        // The metrics footnote two paragraphs up has already named the cause and
        // linked to the fix. Restating it here would be the same paragraph
        // twice, a line apart.
        let ranks = ranking(&[TestOrder::Memory]);
        assert_eq!(
            unranked_note(TestOrder::CpuRequested, Cause::Explained, ranks, ranks).as_deref(),
            Some(
                "Nothing here has cpu-requested to sort by, for the reason above.\n\
                 Sort by memory instead."
            )
        );
    }

    #[test]
    fn the_advice_is_the_same_whether_or_not_the_cause_is_explained() {
        // The two causes change the diagnosis, never what to type next.
        let ranks = ranking(&[TestOrder::Memory]);
        let advice = |cause| {
            unranked_note(TestOrder::CpuRequested, cause, ranks, ranks)
                .and_then(|note| Some(note.lines().nth(1)?.to_owned()))
        };

        assert_eq!(advice(Cause::Explained), advice(Cause::Unexplained));
    }

    #[test]
    fn an_ordering_that_ranks_but_never_rearranges_a_row_is_not_suggested() {
        // `--sort status` on a cluster where every node is `Ready`: every row
        // has a status, so `ranks` says yes, but the ordering puts none of
        // them anywhere different — the case this split exists for.
        assert_eq!(
            unranked_note(
                TestOrder::CpuRequested,
                Cause::Unexplained,
                ranking(&[TestOrder::Memory, TestOrder::Restarts]),
                ranking(&[TestOrder::Restarts]),
            )
            .as_deref(),
            Some("Nothing here has cpu-requested to sort by.\nSort by restarts instead.")
        );
    }

    #[test]
    fn distinguishing_a_row_is_not_enough_without_something_to_rank_it_by() {
        // The reverse gap, stated for completeness: `distinguishes` alone does
        // not earn a suggestion either. An ordering the advice offers has to
        // clear both bars, not swap one for the other.
        assert_eq!(
            unranked_note(
                TestOrder::CpuRequested,
                Cause::Unexplained,
                ranks_nothing,
                ranking(&[TestOrder::Memory]),
            )
            .as_deref(),
            Some("Nothing here has cpu-requested to sort by.")
        );
    }

    #[test]
    fn the_advice_lists_several_orderings_the_way_the_flag_lists_them() {
        // Declaration order, which is `--help`'s order, whatever order the rows
        // happened to answer in. And an Oxford comma, so a three-item list does
        // not read as a two-item one ending in an odd pair.
        let everything = ranking(&[
            TestOrder::Memory,
            TestOrder::Restarts,
            TestOrder::CpuRequested,
        ]);
        assert_eq!(
            unranked_note(TestOrder::Name, Cause::Unexplained, everything, everything),
            None,
            "the default ordering has nothing to explain"
        );
        assert_eq!(
            unranked_note(
                TestOrder::Secret,
                Cause::Unexplained,
                everything,
                everything
            )
            .as_deref(),
            Some(
                "Nothing here has secret to sort by.\n\
                 Sort by restarts, cpu-requested, or memory instead."
            )
        );
    }

    #[test]
    fn two_alternatives_are_joined_without_a_comma() {
        let ranks = ranking(&[TestOrder::Restarts, TestOrder::Memory]);
        assert_eq!(
            unranked_note(TestOrder::CpuRequested, Cause::Unexplained, ranks, ranks).as_deref(),
            Some("Nothing here has cpu-requested to sort by.\nSort by restarts or memory instead.")
        );
    }

    #[test]
    fn the_advice_never_suggests_the_default_ordering() {
        // Every listing can be sorted by name, and `--sort name` is what you get
        // by typing nothing at all. Advice to type a flag for the listing you
        // would have had anyway is not advice.
        let ranks = ranking(&[TestOrder::Name]);
        assert_eq!(
            unranked_note(TestOrder::CpuRequested, Cause::Unexplained, ranks, ranks).as_deref(),
            Some("Nothing here has cpu-requested to sort by.")
        );
    }

    #[test]
    fn an_ordering_hidden_from_help_is_never_suggested() {
        // A flag value the user cannot find any other way is a worse answer
        // than no suggestion at all.
        let ranks = ranking(&[TestOrder::Secret]);
        assert_eq!(
            unranked_note(TestOrder::CpuRequested, Cause::Unexplained, ranks, ranks).as_deref(),
            Some("Nothing here has cpu-requested to sort by.")
        );
    }

    #[test]
    fn a_listing_with_nothing_to_sort_by_at_all_invents_no_advice() {
        // The awkward case: a one-column-short table where every other ordering
        // is just as blank. Silence says "there is nothing else here" better
        // than a suggestion that would fail the same way.
        assert_eq!(
            unranked_note(
                TestOrder::CpuRequested,
                Cause::Explained,
                ranks_nothing,
                ranks_nothing,
            )
            .as_deref(),
            Some("Nothing here has cpu-requested to sort by, for the reason above.")
        );
    }

    #[test]
    fn an_ordering_that_ranked_one_row_says_nothing_extra() {
        // One ranked row puts the row somebody went looking for at an end of
        // the table, which is the whole job. Saying anything here would be
        // noise under a listing that worked.
        let ranks = ranking(&[TestOrder::CpuRequested]);
        assert_eq!(
            unranked_note(TestOrder::CpuRequested, Cause::Unexplained, ranks, ranks),
            None
        );
    }

    #[test]
    fn the_default_ordering_stays_silent_even_when_it_ranks_nothing() {
        // Nobody typed a flag, so there is no flag to explain — and the default
        // output of every command has to stay unchanged to the byte.
        assert_eq!(
            unranked_note(
                TestOrder::Name,
                Cause::Unexplained,
                ranks_nothing,
                ranks_nothing
            ),
            None
        );
    }

    #[test]
    fn the_two_notes_call_an_ordering_the_same_thing() {
        // They print one under the other. An ordering spelled two ways across
        // two adjacent lines reads as two different orderings.
        let named = note(TestOrder::CpuRequested, Direction::Reversed)
            .expect("a reordered listing should say so");
        let unranked = unranked_note(
            TestOrder::CpuRequested,
            Cause::Unexplained,
            ranks_nothing,
            ranks_nothing,
        )
        .expect("an ordering that ranked nothing should say so");

        assert!(named.contains("cpu-requested"), "{named}");
        assert!(unranked.contains("cpu-requested"), "{unranked}");
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
            for &order in TestOrder::value_variants() {
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
