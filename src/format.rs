//! Turning values into the strings a human reads.
//!
//! Everything here is a pure function over plain data, so the awkward cases —
//! a node created in the future, a table with no rows, a cell wider than its
//! header — are settled by tests rather than by squinting at a terminal.

use std::time::Duration;

use k8s_openapi::jiff::SignedDuration;

use crate::theme::{Palette, Severity};

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
/// The third variant, [`Width::Narrow`], is the other end of `--wide`: on a
/// terminal too small for the default table, a listing that knows it can drop
/// columns instead of wrapping. It carries the target width the caller wants
/// the row to fit in, because "narrow" means "this many characters" to a table
/// and a listing that guessed at the number would get it wrong on the next
/// resize. Which columns fall out under it is the listing's business — the
/// pod table and the node table hold different things back and drop them in
/// different orders — so this type only carries the ask, not the answer.
///
/// [`Direction`]: crate::k8s::order::Direction
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Width {
    /// The columns a listing shows when nobody asked for more.
    #[default]
    Default,
    /// Every column the listing has.
    Wide,
    /// Fit the row inside this many characters.
    ///
    /// Carries the target width, so a `Narrow(80)` prints the same table as a
    /// `Narrow(80)` on a different day — the terminal-size lookup is somebody
    /// else's problem, and asserting the columns for a given width does not
    /// need a terminal at all.
    Narrow(u16),
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

    /// The width to render at, given `--wide` and a terminal's columns.
    ///
    /// `--wide` wins over everything: the user typed it, and a narrow terminal
    /// is not a reason to override that. Otherwise, if we can see the terminal
    /// — `terminal_cols = Some(n)` — narrow to fit it. If we cannot — stdout
    /// is a pipe or a file, or the query failed — the [`Default`] set is
    /// unchanged, so a piped listing is the same as it was, byte for byte, and
    /// a script parsing it does not have to guess at the terminal that ran it.
    ///
    /// Pure over its inputs; the I/O lives at the call site.
    ///
    /// [`Default`]: Self::Default
    #[must_use]
    pub fn for_terminal(wide: bool, terminal_cols: Option<u16>) -> Self {
        match (wide, terminal_cols) {
            (true, _) => Self::Wide,
            (false, Some(cols)) => Self::Narrow(cols),
            (false, None) => Self::Default,
        }
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

/// A length of time written the shortest way that still means exactly this.
///
/// The opposite of [`human_duration`], which rounds an age to the unit a person
/// reads it in. Nothing is rounded here, because this prints the number the
/// *user* gave: it is how a `--timeout` is echoed back in an error message, and
/// it is deliberately a spelling [`Budget`] can parse again, so
/// ``allow longer: `--timeout 1m` `` is advice somebody can type rather than a
/// riddle about what this tool calls a minute.
///
/// [`Budget`]: crate::k8s::page::Budget
#[must_use]
pub fn exact_duration(span: Duration) -> String {
    let millis = span.as_millis();
    // Below a second, or not a whole number of them: the only unit that can say
    // this without rounding is the smallest one.
    if millis == 0 {
        return "0s".to_owned();
    }
    if !millis.is_multiple_of(1000) {
        return format!("{millis}ms");
    }

    let seconds = span.as_secs();
    if seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

/// A ratio as a whole-number percentage: `0.6` reads `60%`.
///
/// One function rather than a `{:.0}` at each call site, so a node's share of
/// its allocatable and a pod's share of its request cannot come to be rounded
/// differently — they are the same kind of figure, printed in two tables a
/// person reads one after the other.
///
/// `{:.0}` rather than a cast: no truncation to reason about, and nothing to
/// hand-roll for a value that will not fit an integer.
#[must_use]
pub fn percentage(ratio: f64) -> String {
    format!("{:.0}%", ratio * 100.0)
}

/// Write a list of items out as prose: `a`, `a or b`, `a, b, or c`.
///
/// One function rather than a `join` at each call site, because the awkward
/// part is not the join. It is the comma before the last item, which is what
/// keeps `cpu, memory, or age` from reading as a two-item list ending in an odd
/// pair — and a rule a sentence somewhere else would sooner or later get right
/// a different way.
///
/// `conjunction` is the word before the last item, so the same function writes
/// the "sort by one of these instead" advice and the "these columns are empty"
/// footnote, which want `or` and `and` respectively.
///
/// `None` for an empty list, which forces the caller to decide what its
/// sentence says when there is nothing to put in it rather than leaving a gap
/// in the middle of one.
#[must_use]
pub fn list(items: &[String], conjunction: &str) -> Option<String> {
    match items {
        [] => None,
        [only] => Some(only.clone()),
        [first, second] => Some(format!("{first} {conjunction} {second}")),
        [head @ .., last] => Some(format!("{}, {conjunction} {last}", head.join(", "))),
    }
}

/// One cell of a table: the text in it, and how alarming the value behind that
/// text is.
///
/// The severity travels with the text rather than in a grid beside it because
/// the two have to stay in step through a drop rule that removes columns and a
/// renderer that pads them: a parallel `Vec<Vec<Option<Severity>>>` would be
/// one `retain` away from colouring the wrong column, and nothing on screen
/// would say so.
///
/// `None` is the ordinary case and the honest one. A name, an age, a kubelet
/// version — most of a table is facts with no judgement attached, and a cell
/// that carries no severity prints exactly as it did before colour existed,
/// under any [`Palette`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub text: String,
    pub severity: Option<Severity>,
}

impl Cell {
    /// A cell with nothing to say about how worrying it is.
    #[must_use]
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            severity: None,
        }
    }

    /// A cell carrying a judgement, which a colour palette may draw in ink.
    #[must_use]
    pub fn graded(text: impl Into<String>, severity: Severity) -> Self {
        Self {
            text: text.into(),
            severity: Some(severity),
        }
    }
}

impl From<String> for Cell {
    fn from(text: String) -> Self {
        Self::plain(text)
    }
}

impl From<&str> for Cell {
    fn from(text: &str) -> Self {
        Self::plain(text)
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
///
/// `palette` decides whether a cell carrying a [`Severity`] is written in ink.
/// Alignment does not depend on it: every width here is measured from the
/// cell's *text*, and the escape sequences are wrapped around the text after
/// the padding has been worked out. A table printed with
/// [`Palette::Colour`] and the same table printed with [`Palette::Plain`] have
/// their columns in the same places, character for character — which is the
/// property that makes colour safe to add to a listing that already narrows
/// itself to a terminal.
///
/// Headers are never coloured. A heading is the name of a column, not a
/// reading off it, and there is nothing about `STATUS` to be alarmed by.
#[must_use]
pub fn table(headers: &[&str], rows: &[Vec<Cell>], palette: Palette) -> String {
    let widths = column_widths(headers, rows);

    let mut out = String::new();
    push_row(
        &mut out,
        &headers.iter().map(|h| Cell::plain(*h)).collect::<Vec<_>>(),
        &widths,
        palette,
    );
    for row in rows {
        push_row(&mut out, row, &widths, palette);
    }
    out.trim_end().to_owned()
}

/// How wide a row of columns this wide prints — the widest line [`table`]
/// will produce for them.
///
/// A listing narrowing itself to a terminal has to know what a row measures
/// before it can decide which column to drop, and the only honest answer is
/// the renderer's own arithmetic: a second sum kept beside the drop rule would
/// drift from [`table`] the day a separator changed, and the listing would
/// then drop a column to fit a width nothing prints at. So the renderer and
/// both drop rules go through this and [`column_widths`], and "as wide as the
/// widest cell, two spaces between" is stated once.
///
/// The answer is the nominal row width rather than something shorter, even
/// though [`table`] trims every line: the line carrying the widest cell of the
/// last column has every column before it padded to full width, so that line
/// is exactly this long and no line is longer.
///
/// Widths rather than the cells they came from, because a drop rule asks this
/// once per step and the answer changes while the widths do not — a column is
/// as wide as its own widest cell whatever its neighbours do. So a listing
/// measures once with [`column_widths`] and then does arithmetic over a dozen
/// numbers, rather than rendering every cell again at every step.
///
/// `0` for a table with no columns, which has no lines to measure.
#[must_use]
pub fn row_width(widths: &[usize]) -> usize {
    // `len() - 1` separators, and no separators at all when there are no
    // columns — the subtraction that would wrap is the empty case.
    let Some(separators) = widths.len().checked_sub(1) else {
        return 0;
    };
    widths.iter().sum::<usize>() + 2 * separators
}

/// Each column as wide as its widest cell, or its header where that is wider.
///
/// What [`table`] pads to, and so what a listing deciding which columns it can
/// afford has to measure by.
///
/// Measured from each cell's text, never from what a [`Palette`] would print
/// for it: an escape sequence is bytes the terminal consumes and does not
/// draw, so a width that counted them would pad every coloured column five
/// characters too narrow and hand a listing's drop rule a row length nothing
/// prints at.
#[must_use]
pub fn column_widths(headers: &[&str], rows: &[Vec<Cell>]) -> Vec<usize> {
    headers
        .iter()
        .enumerate()
        .map(|(column, header)| {
            rows.iter()
                .filter_map(|row| row.get(column))
                .map(|cell| display_width(&cell.text))
                .chain(std::iter::once(display_width(header)))
                .max()
                .unwrap_or(0)
        })
        .collect()
}

fn push_row(out: &mut String, cells: &[Cell], widths: &[usize], palette: Palette) {
    let mut line = String::new();

    for (column, width) in widths.iter().enumerate() {
        let text = cells.get(column).map_or("", |cell| cell.text.as_str());
        // The pad comes from the text and the ink goes on afterwards, so a
        // coloured column is exactly as wide as the same column without ink.
        let pad = width.saturating_sub(display_width(text));
        match cells.get(column).and_then(|cell| cell.severity) {
            Some(severity) => line.push_str(&palette.paint(text, severity)),
            None => line.push_str(text),
        }
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
    use crate::theme::ColourChoice;

    /// A cell with no judgement attached — most of a table, and all of the
    /// alignment tests below, which are about widths rather than about ink.
    fn cell(text: &str) -> Cell {
        Cell::plain(text)
    }

    /// A palette that paints, without asking a terminal anything.
    fn colour() -> Palette {
        Palette::choose(ColourChoice::Always, false, None, None)
    }

    /// The visible text of a rendered table: every escape sequence removed.
    ///
    /// Written here rather than pulled in, because a dependency that strips
    /// ANSI would be a second opinion about what this crate emits — and the
    /// point of these tests is that the two agree.
    fn stripped(rendered: &str) -> String {
        let mut out = String::new();
        let mut chars = rendered.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\u{1b}' {
                out.push(c);
                continue;
            }
            // ESC `[` opens the sequence; the `[` itself is inside the range a
            // final byte comes from, so it has to be stepped over before the
            // scan starts or every sequence ends where it began.
            if chars.peek() == Some(&'[') {
                chars.next();
            }
            // Parameters and intermediates, then a final byte in `@`..=`~`.
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
        out
    }

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
    fn an_exact_duration_uses_the_largest_unit_that_loses_nothing() {
        assert_eq!(exact_duration(Duration::from_secs(30)), "30s");
        assert_eq!(exact_duration(Duration::from_secs(90)), "90s");
        assert_eq!(exact_duration(Duration::from_secs(60)), "1m");
        assert_eq!(exact_duration(Duration::from_secs(3600)), "60m");
        assert_eq!(exact_duration(Duration::from_millis(500)), "500ms");
        assert_eq!(exact_duration(Duration::from_millis(1_500)), "1500ms");
        assert_eq!(exact_duration(Duration::ZERO), "0s");
    }

    #[test]
    fn an_exact_duration_rounds_nothing_where_an_age_would() {
        // The distinction between the two functions, in one assertion: five and
        // a half minutes is an age of `5m30s` and a timeout of `330s`, and only
        // the second can be typed back in after `--timeout`.
        assert_eq!(human_duration(seconds(330)), "5m30s");
        assert_eq!(exact_duration(Duration::from_secs(330)), "330s");
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
        // `Narrow` is a request to drop columns, not an ask for the wide tail.
        assert!(!Width::Narrow(80).is_wide());
    }

    #[test]
    fn wide_wins_when_the_user_asked_for_it() {
        // The user typed `--wide` on a small terminal deliberately: they want
        // every column, and they will scroll. A choice that overrode them
        // would make the flag mean nothing on the terminals it exists for.
        assert_eq!(Width::for_terminal(true, Some(40)), Width::Wide);
        assert_eq!(Width::for_terminal(true, None), Width::Wide);
    }

    #[test]
    fn a_terminal_width_becomes_narrow_and_a_pipe_becomes_default() {
        // A pipe is not a "narrow terminal": there is no terminal at all, and
        // a listing that dropped columns for the length of a `grep` line
        // would break every script parsing it.
        assert_eq!(Width::for_terminal(false, Some(80)), Width::Narrow(80));
        assert_eq!(Width::for_terminal(false, None), Width::Default);
    }

    #[test]
    fn percentages_are_whole_numbers() {
        assert_eq!(percentage(0.0), "0%");
        assert_eq!(percentage(0.5), "50%");
        assert_eq!(percentage(1.0), "100%");
    }

    #[test]
    fn percentages_round_rather_than_truncate() {
        // A figure at 2/3 of its request reads `67%`; truncating would put it
        // at 66% and put the row a percentage point below every other tool.
        assert_eq!(percentage(2.0 / 3.0), "67%");
        assert_eq!(percentage(0.004), "0%");
    }

    #[test]
    fn a_measurement_past_its_denominator_is_reported_in_full() {
        // A node using more than its allocatable, or a pod burning four times
        // what it asked for. Both are real, and both are the moment somebody
        // wants the number.
        assert_eq!(percentage(1.04), "104%");
        assert_eq!(percentage(4.5), "450%");
    }

    #[test]
    fn a_list_puts_its_conjunction_before_the_last_item() {
        let items = |names: &[&str]| -> Vec<String> {
            names.iter().map(|name| (*name).to_owned()).collect()
        };

        assert_eq!(list(&items(&[]), "or"), None);
        assert_eq!(list(&items(&["cpu"]), "or").as_deref(), Some("cpu"));
        assert_eq!(
            list(&items(&["cpu", "memory"]), "or").as_deref(),
            Some("cpu or memory")
        );
        // The serial comma: without it, `cpu, memory or age` reads as a
        // two-item list whose second item is an odd pair.
        assert_eq!(
            list(&items(&["cpu", "memory", "age"]), "or").as_deref(),
            Some("cpu, memory, or age")
        );
        assert_eq!(
            list(&items(&["CPU REQ", "MEM REQ"]), "and").as_deref(),
            Some("CPU REQ and MEM REQ")
        );
    }

    #[test]
    fn table_columns_are_as_wide_as_their_widest_cell() {
        let rows = vec![
            vec![cell("ip-10-0-1-9"), cell("Ready")],
            vec![cell("ip-10-0-11-200"), cell("NotReady")],
        ];

        assert_eq!(
            table(&["NAME", "STATUS"], &rows, Palette::Plain),
            "NAME            STATUS\n\
             ip-10-0-1-9     Ready\n\
             ip-10-0-11-200  NotReady"
        );
    }

    #[test]
    fn table_never_leaves_trailing_whitespace() {
        let rows = vec![
            vec![cell("a"), cell("long-value")],
            vec![cell("b"), cell("x")],
        ];

        for line in table(&["ONE", "TWO"], &rows, Palette::Plain).lines() {
            assert_eq!(line.trim_end(), line, "line {line:?} has trailing spaces");
        }
    }

    #[test]
    fn table_with_no_rows_is_just_the_header() {
        assert_eq!(table(&["NAME", "AGE"], &[], Palette::Plain), "NAME  AGE");
    }

    #[test]
    fn table_pads_short_rows_and_ignores_extra_cells() {
        let rows = vec![
            vec![cell("only-one")],
            vec![cell("a"), cell("b"), cell("ignored")],
        ];

        assert_eq!(
            table(&["ONE", "TWO"], &rows, Palette::Plain),
            "ONE       TWO\n\
             only-one\n\
             a         b"
        );
    }

    #[test]
    fn table_with_no_columns_is_empty() {
        assert_eq!(table(&[], &[vec![cell("orphan")]], Palette::Plain), "");
    }

    /// The guarantee the narrowing rules in both listings are built on: the
    /// width measured from `column_widths` is the number of characters `table`
    /// will actually print at its widest. Asserted over the same awkward
    /// tables the renderer's own tests use — a cell wider than its header, a
    /// ragged row, a header-only table — because a drop rule that measured a
    /// row the renderer disagreed with would drop a column to fit a width
    /// nothing prints at.
    #[test]
    fn a_measured_row_is_the_longest_line_table_prints() {
        let cases: [(&[&str], Vec<Vec<Cell>>); 5] = [
            (
                &["NAME", "STATUS"],
                vec![
                    vec![cell("ip-10-0-1-9"), cell("Ready")],
                    vec![cell("ip-10-0-11-200"), cell("NotReady")],
                ],
            ),
            // The widest cell is in the last column, which is the case the
            // trailing-pad trim could have made shorter than the nominal width.
            (
                &["ONE", "TWO"],
                vec![
                    vec![cell("a"), cell("long-value")],
                    vec![cell("b"), cell("x")],
                ],
            ),
            // Ragged rows: one short, one with a cell the table ignores.
            (
                &["ONE", "TWO"],
                vec![
                    vec![cell("only-one")],
                    vec![cell("a"), cell("b"), cell("ignored")],
                ],
            ),
            // No rows at all: the header line is the whole table.
            (&["NAME", "AGE"], vec![]),
            // One column, so there are no separators to count.
            (&["NAME"], vec![vec![cell("api-7c9f")]]),
        ];

        for (headers, rows) in cases {
            let printed = table(headers, &rows, Palette::Plain)
                .lines()
                .map(|line| line.chars().count())
                .max()
                .unwrap_or(0);
            assert_eq!(
                row_width(&column_widths(headers, &rows)),
                printed,
                "headers {headers:?} rows {rows:?}"
            );
        }
    }

    #[test]
    fn a_row_is_its_columns_plus_two_spaces_between_them() {
        // The separator rule on its own, without a table to build: one column
        // has no separators, and each one after it costs two characters.
        assert_eq!(row_width(&[]), 0);
        assert_eq!(row_width(&[5]), 5);
        assert_eq!(row_width(&[5, 3]), 10);
        assert_eq!(row_width(&[5, 3, 1]), 13);
    }

    #[test]
    fn a_table_of_plain_cells_is_the_same_bytes_under_any_palette() {
        // Most of a table is facts with no judgement attached, and none of it
        // may gain a byte because a palette was passed in. This is what keeps
        // `eks contexts`, and every uncoloured column of the two listings,
        // exactly as it was.
        let rows = vec![
            vec![cell("ip-10-0-1-9"), cell("Ready")],
            vec![cell("ip-10-0-11-200"), cell("NotReady")],
        ];

        assert_eq!(
            table(&["NAME", "STATUS"], &rows, colour()),
            table(&["NAME", "STATUS"], &rows, Palette::Plain)
        );
    }

    #[test]
    fn ink_does_not_move_a_column() {
        // The property the whole design rests on: widths come from the cell's
        // text, and the escapes go on after the padding is worked out. Strip
        // the ink back off a coloured table and the plain one is underneath,
        // character for character — so a listing that narrows itself to a
        // terminal narrows to the same columns either way.
        let rows = vec![
            vec![
                cell("ip-10-0-1-9"),
                Cell::graded("NotReady", Severity::Critical),
                cell("v1.33.4-eks-1234567"),
            ],
            vec![
                cell("ip-10-0-11-200"),
                Cell::graded("Ready", Severity::Ok),
                cell("v1.33.4-eks-1234567"),
            ],
            vec![
                cell("ip-10-0-2-77"),
                Cell::graded("Unknown", Severity::Unknown),
                cell("-"),
            ],
        ];
        let headers = ["NAME", "STATUS", "VERSION"];

        let painted = table(&headers, &rows, colour());
        assert_ne!(painted, table(&headers, &rows, Palette::Plain));
        assert_eq!(stripped(&painted), table(&headers, &rows, Palette::Plain));
    }

    #[test]
    fn a_coloured_table_still_leaves_no_trailing_whitespace() {
        // The last cell is the awkward one: its ink ends in an escape, which
        // `trim_end` cannot see, so the padding in front of it has to have
        // gone before the ink went on.
        let rows = vec![
            vec![
                cell("api-7c9f"),
                Cell::graded("CrashLoopBackOff", Severity::Critical),
            ],
            vec![cell("db-0"), Cell::graded("Running", Severity::Ok)],
        ];

        for line in table(&["NAME", "STATUS"], &rows, colour()).lines() {
            let visible = stripped(line);
            assert_eq!(
                visible.trim_end(),
                visible,
                "line {line:?} has trailing spaces"
            );
        }
    }

    #[test]
    fn an_empty_graded_cell_leaves_no_ink_behind() {
        // A row whose last cell is graded and empty: painting it would leave a
        // line ending in a sequence with nothing inside it, which `trim_end`
        // would then keep because an escape is not a space.
        let rows = vec![vec![cell("only-one"), Cell::graded("", Severity::Critical)]];

        let painted = table(&["ONE", "TWO"], &rows, colour());
        assert_eq!(painted, table(&["ONE", "TWO"], &rows, Palette::Plain));
        assert!(!painted.contains('\u{1b}'), "{painted:?}");
    }

    #[test]
    fn a_heading_is_never_coloured() {
        // A heading names a column; it is not a reading off one, and there is
        // nothing about the word `STATUS` to be alarmed by.
        let rows = vec![vec![Cell::graded("NotReady", Severity::Critical)]];

        let header = table(&["STATUS"], &rows, colour())
            .lines()
            .next()
            .map(str::to_owned)
            .unwrap_or_default();
        assert_eq!(header, "STATUS");
    }

    #[test]
    fn a_measured_width_ignores_the_ink_a_cell_would_be_printed_in() {
        // What a drop rule measures by. Counting escape bytes here would pad
        // every coloured column short and stop the narrowing at a row length
        // nothing prints at.
        let graded = vec![vec![Cell::graded("CrashLoopBackOff", Severity::Critical)]];
        let plain = vec![vec![cell("CrashLoopBackOff")]];

        assert_eq!(
            column_widths(&["STATUS"], &graded),
            column_widths(&["STATUS"], &plain)
        );
        assert_eq!(column_widths(&["STATUS"], &graded), vec![16]);
    }

    #[test]
    fn a_cell_carries_no_judgement_unless_it_was_given_one() {
        // The `From` impls exist so a listing with nothing to grade — `eks
        // contexts` — reads as a list of strings and gets the plain table.
        assert_eq!(Cell::from("prod").severity, None);
        assert_eq!(Cell::from("prod".to_owned()), Cell::plain("prod"));
        assert_eq!(
            Cell::graded("97%", Severity::Critical).severity,
            Some(Severity::Critical)
        );
    }

    #[test]
    fn a_table_with_no_columns_measures_nothing() {
        // The empty case is the one that would underflow a `len() - 1`, and a
        // listing that dropped every column would ask exactly this.
        assert_eq!(row_width(&column_widths(&[], &[vec![cell("orphan")]])), 0);
    }
}
