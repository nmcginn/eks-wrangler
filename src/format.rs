//! Turning values into the strings a human reads.
//!
//! Everything here is a pure function over plain data, so the awkward cases —
//! a node created in the future, a table with no rows, a cell wider than its
//! header — are settled by tests rather than by squinting at a terminal.

use k8s_openapi::jiff::SignedDuration;

/// How many of a table's columns to print.
///
/// Both listings hold a handful of columns back from their default table — a
/// pod's IP, a node's kernel version — because they are noise on the question
/// the table is usually asked and exactly what is wanted on the day it is not.
/// `--wide` is the same flag on both, so it is one type rather than a `bool`
/// per listing: two listings each carrying their own would sooner or later
/// disagree about what "wide" means, which is the whole reason [`Direction`]
/// lives in one place too.
///
/// [`Direction`]: crate::k8s::order::Direction
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Width {
    /// The columns a listing shows when nobody asked for more.
    #[default]
    Default,
    /// Every column the listing has.
    Wide,
}

impl Width {
    /// `Wide` when `--wide` was given, `Default` otherwise.
    ///
    /// Named for the flag, like [`Direction::reversed`], so the call site in
    /// `main` reads as the command line does.
    ///
    /// [`Direction::reversed`]: crate::k8s::order::Direction::reversed
    #[must_use]
    pub fn widened(yes: bool) -> Self {
        if yes { Self::Wide } else { Self::Default }
    }

    /// Whether the extra columns are shown.
    #[must_use]
    pub fn is_wide(self) -> bool {
        matches!(self, Self::Wide)
    }
}

/// Human-readable age, in `kubectl`'s style.
///
/// `kubectl` shows a coarser unit as the value grows — `45s`, `5m30s`, `3h20m`,
/// `9d`, `2y64d` — and we match it deliberately. People read `AGE` columns by
/// habit, and a column that rounds differently from the tool next to it makes
/// them stop and check.
///
/// A negative delta means the object claims to have been created in the future,
/// which is clock skew rather than something to shout about, so it reads `0s`.
#[must_use]
pub fn human_duration(delta: SignedDuration) -> String {
    let seconds = delta.as_secs();
    if seconds < 0 {
        return "0s".to_owned();
    }
    if seconds < 120 {
        return format!("{seconds}s");
    }

    let minutes = seconds / 60;
    if minutes < 10 {
        let remainder = seconds % 60;
        return if remainder == 0 {
            format!("{minutes}m")
        } else {
            format!("{minutes}m{remainder}s")
        };
    }
    if minutes < 180 {
        return format!("{minutes}m");
    }

    let hours = minutes / 60;
    if hours < 8 {
        let remainder = minutes % 60;
        return if remainder == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h{remainder}m")
        };
    }
    if hours < 48 {
        return format!("{hours}h");
    }

    let days = hours / 24;
    if days < 8 {
        let remainder = hours % 24;
        return if remainder == 0 {
            format!("{days}d")
        } else {
            format!("{days}d{remainder}h")
        };
    }
    if days < 365 * 2 {
        return format!("{days}d");
    }

    let years = days / 365;
    let remainder = days % 365;
    if remainder == 0 {
        format!("{years}y")
    } else {
        format!("{years}y{remainder}d")
    }
}

/// Render an aligned, `kubectl`-style table.
///
/// Columns are as wide as their widest cell, separated by two spaces. The last
/// column is never padded: trailing whitespace is noise in a diff and a
/// nuisance when someone copies a line out of their terminal.
///
/// Rows shorter than `headers` are padded with empty cells and longer ones are
/// truncated, so a caller cannot produce a ragged table by accident.
#[must_use]
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(column, header)| {
            rows.iter()
                .filter_map(|row| row.get(column))
                .map(|cell| display_width(cell))
                .chain(std::iter::once(display_width(header)))
                .max()
                .unwrap_or(0)
        })
        .collect();

    let mut out = String::new();
    push_row(
        &mut out,
        &headers.iter().map(|h| (*h).to_owned()).collect::<Vec<_>>(),
        &widths,
    );
    for row in rows {
        push_row(&mut out, row, &widths);
    }
    out.trim_end().to_owned()
}

fn push_row(out: &mut String, cells: &[String], widths: &[usize]) {
    let mut line = String::new();

    for (column, width) in widths.iter().enumerate() {
        let cell = cells.get(column).map_or("", String::as_str);
        line.push_str(cell);
        let pad = width.saturating_sub(display_width(cell));
        line.extend(std::iter::repeat_n(' ', pad + 2));
    }

    // Padding is added after every column and then taken back off the end, so a
    // missing or empty final cell cannot leave a line full of spaces.
    out.push_str(line.trim_end());
    out.push('\n');
}

/// Width of a string on screen, counted in characters.
///
/// Kubernetes names are ASCII, so this is exact where it matters and cheap
/// everywhere else. It is wrong for wide CJK glyphs; if user-supplied labels
/// ever reach a table, this is the function to replace.
fn display_width(value: &str) -> usize {
    value.chars().count()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn seconds(count: i64) -> SignedDuration {
        SignedDuration::from_secs(count)
    }

    #[test]
    fn ages_under_two_minutes_are_shown_in_seconds() {
        assert_eq!(human_duration(seconds(0)), "0s");
        assert_eq!(human_duration(seconds(45)), "45s");
        assert_eq!(human_duration(seconds(119)), "119s");
    }

    #[test]
    fn ages_under_ten_minutes_keep_their_seconds() {
        assert_eq!(human_duration(seconds(120)), "2m");
        assert_eq!(human_duration(seconds(330)), "5m30s");
        assert_eq!(human_duration(seconds(599)), "9m59s");
    }

    #[test]
    fn ages_switch_to_coarser_units_as_they_grow() {
        assert_eq!(human_duration(seconds(600)), "10m");
        assert_eq!(human_duration(seconds(60 * 179)), "179m");
        assert_eq!(human_duration(seconds(60 * 180)), "3h");
        assert_eq!(human_duration(seconds(60 * 185)), "3h5m");
        assert_eq!(human_duration(seconds(3600 * 8)), "8h");
        assert_eq!(human_duration(seconds(3600 * 47)), "47h");
        assert_eq!(human_duration(seconds(3600 * 49)), "2d1h");
        assert_eq!(human_duration(seconds(3600 * 24 * 8)), "8d");
        assert_eq!(human_duration(seconds(3600 * 24 * 400)), "400d");
    }

    #[test]
    fn ages_beyond_two_years_are_shown_in_years() {
        assert_eq!(human_duration(seconds(3600 * 24 * 730)), "2y");
        assert_eq!(human_duration(seconds(3600 * 24 * 794)), "2y64d");
    }

    #[test]
    fn an_object_created_in_the_future_reads_as_zero() {
        // Clock skew between the API server and this machine; not worth a scary
        // value in the AGE column.
        assert_eq!(human_duration(seconds(-30)), "0s");
        assert_eq!(human_duration(SignedDuration::from_hours(-24 * 400)), "0s");
    }

    #[test]
    fn a_width_is_wide_only_when_the_flag_was_given() {
        assert_eq!(Width::widened(true), Width::Wide);
        assert_eq!(Width::widened(false), Width::Default);
        // The default must be the narrow table, or a listing that forgot to
        // pass the flag through would quietly grow columns.
        assert_eq!(Width::default(), Width::Default);
        assert!(Width::Wide.is_wide());
        assert!(!Width::Default.is_wide());
    }

    #[test]
    fn table_columns_are_as_wide_as_their_widest_cell() {
        let rows = vec![
            vec!["ip-10-0-1-9".to_owned(), "Ready".to_owned()],
            vec!["ip-10-0-11-200".to_owned(), "NotReady".to_owned()],
        ];

        assert_eq!(
            table(&["NAME", "STATUS"], &rows),
            "NAME            STATUS\n\
             ip-10-0-1-9     Ready\n\
             ip-10-0-11-200  NotReady"
        );
    }

    #[test]
    fn table_never_leaves_trailing_whitespace() {
        let rows = vec![
            vec!["a".to_owned(), "long-value".to_owned()],
            vec!["b".to_owned(), "x".to_owned()],
        ];

        for line in table(&["ONE", "TWO"], &rows).lines() {
            assert_eq!(line.trim_end(), line, "line {line:?} has trailing spaces");
        }
    }

    #[test]
    fn table_with_no_rows_is_just_the_header() {
        assert_eq!(table(&["NAME", "AGE"], &[]), "NAME  AGE");
    }

    #[test]
    fn table_pads_short_rows_and_ignores_extra_cells() {
        let rows = vec![
            vec!["only-one".to_owned()],
            vec!["a".to_owned(), "b".to_owned(), "ignored".to_owned()],
        ];

        assert_eq!(
            table(&["ONE", "TWO"], &rows),
            "ONE       TWO\n\
             only-one\n\
             a         b"
        );
    }

    #[test]
    fn table_with_no_columns_is_empty() {
        assert_eq!(table(&[], &[vec!["orphan".to_owned()]]), "");
    }
}
