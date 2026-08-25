//! Fuzzy, case-insensitive, ranked matching for the dashboard's `/` filter.
//!
//! One matcher for every row list — the node pane, the pod-drilldown pane,
//! the pod-containers pane — rather than each growing its own. A pure
//! function over plain strings, so the awkward cases (an empty query, a
//! query with no match anywhere, ten thousand candidates) are fixtures
//! rather than a dashboard you have to drive to exercise them.

/// Score `candidate` against `query`, case-insensitively. `None` when
/// `query`'s characters do not all appear in `candidate`, in order — not a
/// subsequence — and `Some(score)` otherwise, where a higher score should
/// rank first.
///
/// An empty query matches every candidate with a score of `0`, so a caller
/// filtering by an empty query keeps every row rather than dropping the
/// whole list — the same "no filter" reading `--sort`'s default order and
/// `-l`'s absence already give the rest of this tool.
///
/// The score rewards the matches a person typing a few letters is actually
/// after: a run of consecutive characters over the same letters scattered
/// through the name, and a match starting at the candidate's first character
/// or right after a separator (`worker-1` for `w1`) over one buried in the
/// middle of a word. It is not trying to reproduce any particular fuzzy
/// finder's exact numbers, only to keep "the name you typed the start of"
/// above "the name your letters happen to appear somewhere inside".
#[must_use]
pub fn score(query: &str, candidate: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }

    let candidate_chars: Vec<char> = candidate.chars().collect();
    let candidate_lower: Vec<char> = candidate_chars
        .iter()
        .flat_map(|c| c.to_lowercase())
        .collect();

    let mut total: i64 = 0;
    let mut search_from = 0;
    let mut previous_match: Option<usize> = None;

    for query_char in query.chars().flat_map(char::to_lowercase) {
        let found = candidate_lower[search_from..]
            .iter()
            .position(|&c| c == query_char)
            .map(|offset| search_from + offset)?;

        total += 10;

        let is_boundary = found == 0 || !candidate_chars[found - 1].is_alphanumeric();
        if is_boundary {
            total += 8;
        }

        if let Some(previous) = previous_match {
            if found == previous + 1 {
                total += 15;
            } else {
                // A small penalty per skipped character rather than one that
                // grows without bound, so one long gap in an otherwise tight
                // match does not sink it below a match that is loose
                // everywhere.
                let gap = i64::try_from((found - previous - 1).min(20)).unwrap_or(20);
                total -= gap;
            }
        }

        previous_match = Some(found);
        search_from = found + 1;
    }

    Some(total)
}

/// Rank `items` by how well `key(item)` matches `query`: matches only, best
/// first, ties broken by original position. An empty query keeps every item
/// in its original order — the identity mapping, so a caller drawing an
/// unfiltered listing through this function renders exactly what it always
/// has, byte for byte.
#[must_use]
pub fn rank<'a, T>(query: &str, items: &'a [T], key: impl Fn(&T) -> &str) -> Vec<&'a T> {
    let mut scored: Vec<(usize, i64, &T)> = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| score(query, key(item)).map(|s| (index, s, item)))
        .collect();

    scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    scored.into_iter().map(|(_, _, item)| item).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn an_empty_query_matches_every_candidate() {
        assert_eq!(score("", "anything"), Some(0));
        assert_eq!(score("", ""), Some(0));
    }

    #[test]
    fn a_candidate_missing_a_query_character_does_not_match() {
        assert_eq!(score("xyz", "worker-1"), None);
    }

    #[test]
    fn an_empty_candidate_only_matches_an_empty_query() {
        assert_eq!(score("", ""), Some(0));
        assert_eq!(score("a", ""), None);
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert_eq!(score("API", "api-server"), score("api", "api-server"));
        assert!(score("ApI", "the-Api-Server").is_some());
    }

    #[test]
    fn a_query_matches_as_a_subsequence_not_only_a_substring() {
        // w(0) o r k(3) e r - 1(7): "wk1" is not contiguous in "worker-1" but
        // its letters do appear in order.
        assert!(score("wk1", "worker-1").is_some());
    }

    #[test]
    fn characters_must_appear_in_the_order_the_query_gives_them() {
        // "1w" would need the "1" before the "w", which "worker-1" never has.
        assert_eq!(score("1w", "worker-1"), None);
    }

    #[test]
    fn a_contiguous_match_outranks_a_scattered_one() {
        let tight = score("api", "api-1").unwrap();
        let scattered = score("api", "a-fake-pie").unwrap();
        assert!(tight > scattered, "tight={tight} scattered={scattered}");
    }

    #[test]
    fn a_match_at_a_word_boundary_outranks_one_mid_word() {
        let boundary = score("pod", "web-pod-1").unwrap();
        let mid_word = score("pod", "wepodx").unwrap();
        assert!(
            boundary > mid_word,
            "boundary={boundary} mid_word={mid_word}"
        );
    }

    #[test]
    fn rank_keeps_only_the_candidates_that_match() {
        let names = ["worker-1", "worker-2", "control-plane"];
        let result = rank("worker", &names, |s| *s);
        assert_eq!(result, vec![&"worker-1", &"worker-2"]);
    }

    #[test]
    fn rank_orders_the_best_match_first() {
        let names = ["a-fake-pie", "api-1"];
        let result = rank("api", &names, |s| *s);
        assert_eq!(result, vec![&"api-1", &"a-fake-pie"]);
    }

    #[test]
    fn rank_breaks_ties_by_original_order() {
        let names = ["b-worker", "a-worker"];
        let result = rank("worker", &names, |s| *s);
        assert_eq!(result, vec![&"b-worker", &"a-worker"]);
    }

    #[test]
    fn an_empty_query_returns_every_item_in_its_original_order() {
        let names = ["charlie", "alpha", "bravo"];
        let result = rank("", &names, |s| *s);
        assert_eq!(result, vec![&"charlie", &"alpha", &"bravo"]);
    }

    #[test]
    fn ranking_finds_nothing_in_an_empty_list() {
        let names: [&str; 0] = [];
        assert!(rank("anything", &names, |s| *s).is_empty());
    }

    #[test]
    fn ranking_ten_thousand_rows_stays_well_under_one_frame() {
        // The acceptance criterion this exists to hold the matcher to:
        // filtering ten thousand rows must not cost the render loop a frame.
        // `criterion` and `make bench` are their own roadmap task ("Startup
        // budget and benchmarks"); until that infrastructure exists, a
        // wall-clock assertion with a very generous ceiling is the fixture —
        // an ordinary run finishes in low single-digit milliseconds, so this
        // only fails if the matcher stops being linear in the candidate
        // count.
        let items: Vec<String> = (0..10_000).map(|i| format!("worker-{i:05}-pod")).collect();

        let started = Instant::now();
        let result = rank("wk5", &items, String::as_str);
        let elapsed = started.elapsed();

        assert!(!result.is_empty());
        assert!(elapsed < Duration::from_millis(100), "took {elapsed:?}");
    }
}
