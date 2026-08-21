//! The colour palette, in one place.
//!
//! Every style the TUI draws comes from here, and so does every escape
//! sequence the CLI tables print. Keeping it centralised is what makes a
//! consistent look cheap to maintain — and makes a light-mode or
//! user-configurable theme a change to one file rather than a hundred call
//! sites.

use std::borrow::Cow;
use std::ffi::OsStr;

use ratatui::style::{Color, Modifier, Style};

/// A complete set of styles for the interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub background: Color,
    pub text: Color,
    /// De-emphasised text: units, hints, secondary columns.
    pub muted: Color,
    /// The single hue that carries identity and focus.
    pub accent: Color,
    pub border: Color,
    pub border_focused: Color,
    pub success: Color,
    pub warning: Color,
    pub danger: Color,
    pub selection_bg: Color,
}

impl Theme {
    /// The default dark theme, tuned for readability on a dark terminal.
    #[must_use]
    pub const fn dark() -> Self {
        Self {
            background: Color::Reset,
            text: Color::Rgb(0xE6, 0xE6, 0xE6),
            muted: Color::Rgb(0x8A, 0x8F, 0x98),
            accent: Color::Rgb(0x56, 0xB6, 0xC2),
            border: Color::Rgb(0x3A, 0x3F, 0x4B),
            border_focused: Color::Rgb(0x56, 0xB6, 0xC2),
            success: Color::Rgb(0x7E, 0xC6, 0x99),
            warning: Color::Rgb(0xE5, 0xC0, 0x7B),
            danger: Color::Rgb(0xE0, 0x6C, 0x75),
            selection_bg: Color::Rgb(0x2C, 0x31, 0x3C),
        }
    }

    /// Body text.
    #[must_use]
    pub fn body(self) -> Style {
        Style::default().fg(self.text)
    }

    /// Secondary text — never the thing the eye should land on first.
    #[must_use]
    pub fn dim(self) -> Style {
        Style::default().fg(self.muted)
    }

    /// Headings and the active context indicator.
    #[must_use]
    pub fn heading(self) -> Style {
        Style::default()
            .fg(self.accent)
            .add_modifier(Modifier::BOLD)
    }

    /// The highlighted row in a list or table.
    #[must_use]
    pub fn selected(self) -> Style {
        Style::default()
            .bg(self.selection_bg)
            .fg(self.text)
            .add_modifier(Modifier::BOLD)
    }

    /// Border style for a pane, varying with focus.
    #[must_use]
    pub fn pane_border(self, focused: bool) -> Style {
        Style::default().fg(if focused {
            self.border_focused
        } else {
            self.border
        })
    }

    /// Colour for a health-ish value, from calm to alarming.
    #[must_use]
    pub fn severity(self, level: Severity) -> Style {
        let colour = match level {
            Severity::Ok => self.success,
            Severity::Warn => self.warning,
            Severity::Critical => self.danger,
            Severity::Unknown => self.muted,
        };
        Style::default().fg(colour)
    }

    /// The ink a severity is written in, in a table of plain text — or `None`
    /// where it is written in whatever colour the terminal was already using.
    ///
    /// The same four severities as [`severity`](Self::severity), and
    /// deliberately not the same four colours. A dashboard draws a severity as
    /// a *shape*: a bar filled green along its length is a quantity, and the
    /// green is the fill. A table draws it as ink on a line the reader is
    /// scanning, and a healthy cluster is almost every cell — so painting
    /// [`Severity::Ok`] green would put the strongest signal a terminal has on
    /// the rows with nothing to say, and leave the one node at 97% competing
    /// with two hundred green neighbours for the eye. Colour is worth what it
    /// is spent on.
    ///
    /// So `Ok` is the absence of an escape sequence, not a colour: the cell
    /// prints in whatever the user's terminal is already set to, which is what
    /// the whole table printed in before this existed. `Unknown` is muted
    /// rather than alarming, because it is an absence — a `-` where a figure
    /// could not be read — and greying it out says so without shouting.
    ///
    /// What counts as hot is *not* decided here: that stays
    /// [`Severity::from_utilisation`]'s, one rule for both surfaces. This
    /// decides only how a severity already settled is drawn on this one.
    #[must_use]
    pub const fn severity_ink(self, level: Severity) -> Option<Color> {
        match level {
            Severity::Ok => None,
            Severity::Warn => Some(self.warning),
            Severity::Critical => Some(self.danger),
            Severity::Unknown => Some(self.muted),
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

/// How worried the user should be about a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Ok,
    Warn,
    Critical,
    Unknown,
}

impl Severity {
    /// Classify a 0.0–1.0 utilisation ratio.
    ///
    /// Thresholds live here rather than at call sites so "what counts as hot"
    /// stays one decision.
    #[must_use]
    pub fn from_utilisation(ratio: f64) -> Self {
        if !ratio.is_finite() || ratio < 0.0 {
            Self::Unknown
        } else if ratio >= 0.90 {
            Self::Critical
        } else if ratio >= 0.75 {
            Self::Warn
        } else {
            Self::Ok
        }
    }
}

/// What the user asked for with `--color`.
///
/// A `clap::ValueEnum` on the domain type for the reason `--sort` is one
/// (decision 28): a value this does not take is rejected with the ones it does
/// listed, before anything connects, rather than parsed into a silent default.
/// The spellings are `auto`, `always`, and `never`, which is what every other
/// tool with this flag calls them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ColourChoice {
    /// Colour when stdout is a terminal that wants it. The default.
    #[default]
    Auto,
    /// Colour whatever stdout is — for a pager, or a CI log that renders it.
    Always,
    /// No escape sequences at all.
    Never,
}

/// Whether a listing prints colour, and what it prints for a severity.
///
/// The decision and the drawing, kept together and kept out of the tables: a
/// listing hands its cells to [`format::table`] with one of these and never
/// asks what a terminal is. The I/O that answers the question — is stdout a
/// terminal, what is in the environment — happens once, in `main`, and
/// [`choose`](Self::choose) is a pure function over its answers.
///
/// [`Plain`](Self::Plain) is the [`Default`] deliberately: a code path that
/// forgets to pass a palette prints the table it printed before, rather than
/// escape sequences into a file.
///
/// [`format::table`]: crate::format::table
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Palette {
    /// No escape sequences at all — the table, byte for byte, as it was
    /// before colour existed.
    #[default]
    Plain,
    /// The theme's severity ink, as ANSI escape sequences.
    Colour(Theme),
}

/// Sets the foreground back to the terminal's own default.
///
/// `39` rather than `0`: a full reset would also clear bold, italics, and the
/// background, none of which this tool set — and one of which the user's own
/// terminal or their pager may have. We turn off exactly what we turned on.
const FOREGROUND_DEFAULT: &str = "\x1b[39m";

impl Palette {
    /// Decide whether this run prints colour.
    ///
    /// Pure over the four things that decide it, so every combination below is
    /// a test rather than an environment variable somebody has to set:
    ///
    /// - `choice` is `--color`, and it wins outright. The user typed it.
    /// - `stdout_is_terminal` is the `auto` default: a pipe or a file gets the
    ///   plain table, so `eks nodes | grep NotReady` is unchanged and nothing
    ///   downstream has to strip escapes it did not ask for.
    /// - `no_color` is the [NO_COLOR] environment variable, honoured on its own
    ///   terms: *set and not empty* turns colour off. An empty value is the
    ///   spec's way of saying "not set", and a shell that exports `NO_COLOR=`
    ///   into every process must not silently disable colour everywhere.
    /// - `term` is `TERM`. `dumb` is the one value that promises no escape
    ///   sequences are understood, and it is what `M-x shell` and a handful of
    ///   CI runners set.
    ///
    /// [NO_COLOR]: https://no-color.org/
    #[must_use]
    pub fn choose(
        choice: ColourChoice,
        stdout_is_terminal: bool,
        no_color: Option<&OsStr>,
        term: Option<&OsStr>,
    ) -> Self {
        let wanted = match choice {
            ColourChoice::Always => true,
            ColourChoice::Never => false,
            ColourChoice::Auto => {
                stdout_is_terminal
                    // Set *and not empty*: an empty value is the spec's way of
                    // saying "not set".
                    && no_color.is_none_or(OsStr::is_empty)
                    && term != Some(OsStr::new("dumb"))
            }
        };

        if wanted {
            Self::Colour(Theme::default())
        } else {
            Self::Plain
        }
    }

    /// Whether anything this palette paints will carry an escape sequence.
    #[must_use]
    pub fn is_colour(self) -> bool {
        matches!(self, Self::Colour(_))
    }

    /// `text`, in the ink this severity is written in.
    ///
    /// Borrowed and untouched in every case that adds nothing: a
    /// [`Plain`](Self::Plain) palette, a severity the theme writes in the
    /// terminal's own colour, and an empty cell. That last one is not an
    /// optimisation — a zero-width cell wrapped in escapes is invisible ink
    /// that [`format::table`]'s trailing-space trim cannot see, so it would
    /// leave a line ending in a sequence with nothing in it.
    ///
    /// [`format::table`]: crate::format::table
    #[must_use]
    pub fn paint(self, text: &str, severity: Severity) -> Cow<'_, str> {
        let Self::Colour(theme) = self else {
            return Cow::Borrowed(text);
        };
        if text.is_empty() {
            return Cow::Borrowed(text);
        }
        let Some(ink) = theme.severity_ink(severity) else {
            return Cow::Borrowed(text);
        };
        let Some(start) = foreground(ink) else {
            return Cow::Borrowed(text);
        };

        Cow::Owned(format!("{start}{text}{FOREGROUND_DEFAULT}"))
    }
}

/// The escape sequence that sets `colour` as the foreground, or `None` for a
/// colour that is the terminal's own default and so needs no sequence.
///
/// Written out rather than delegated to `crossterm`, because the mapping is
/// the part worth pinning: a test can assert the exact bytes, which is the
/// only way to be sure a table is not quietly emitting a sequence that shifts
/// a column by five characters on somebody else's terminal.
///
/// Every variant is spelled out and there is no catch-all arm, so a colour
/// added to `ratatui` in a future release stops the build here rather than
/// silently printing plain.
fn foreground(colour: Color) -> Option<String> {
    let code = match colour {
        // 24-bit. The theme's own colours are all this, and a terminal that
        // does not understand the sequence ignores it rather than printing it
        // — which is the same table, in the same colour it had before.
        Color::Rgb(r, g, b) => return Some(format!("\x1b[38;2;{r};{g};{b}m")),
        Color::Indexed(index) => return Some(format!("\x1b[38;5;{index}m")),
        // "Whatever the terminal was using" is the absence of a sequence, not
        // a sequence that sets it — see `FOREGROUND_DEFAULT`, which is what
        // ends a painted cell.
        Color::Reset => return None,
        Color::Black => 30,
        Color::Red => 31,
        Color::Green => 32,
        Color::Yellow => 33,
        Color::Blue => 34,
        Color::Magenta => 35,
        Color::Cyan => 36,
        // `ratatui` names the eight bright colours after the dim ones, so
        // `Gray` is plain white and `DarkGray` is bright black. The pairing is
        // `ratatui`'s own, not ours — see its crossterm backend.
        Color::Gray => 37,
        Color::DarkGray => 90,
        Color::LightRed => 91,
        Color::LightGreen => 92,
        Color::LightYellow => 93,
        Color::LightBlue => 94,
        Color::LightMagenta => 95,
        Color::LightCyan => 96,
        Color::White => 97,
    };

    Some(format!("\x1b[{code}m"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn utilisation_thresholds_are_inclusive_at_the_boundary() {
        assert_eq!(Severity::from_utilisation(0.0), Severity::Ok);
        assert_eq!(Severity::from_utilisation(0.749), Severity::Ok);
        assert_eq!(Severity::from_utilisation(0.75), Severity::Warn);
        assert_eq!(Severity::from_utilisation(0.899), Severity::Warn);
        assert_eq!(Severity::from_utilisation(0.90), Severity::Critical);
        assert_eq!(Severity::from_utilisation(1.5), Severity::Critical);
    }

    #[test]
    fn nonsense_utilisation_is_unknown_not_alarming() {
        assert_eq!(Severity::from_utilisation(f64::NAN), Severity::Unknown);
        assert_eq!(Severity::from_utilisation(-0.1), Severity::Unknown);
    }

    #[test]
    fn focused_panes_are_visually_distinct() {
        let theme = Theme::dark();
        assert_ne!(theme.pane_border(true), theme.pane_border(false));
    }

    /// The palette a CLI listing gets when `--color=always` was typed, without
    /// asking a terminal anything.
    fn colour() -> Palette {
        Palette::choose(ColourChoice::Always, false, None, None)
    }

    #[test]
    fn a_calm_reading_is_written_in_the_terminals_own_colour() {
        // The decision the whole CLI palette turns on. A healthy cluster is
        // almost every cell, and painting all of them green would spend the
        // strongest signal a terminal has on the rows with nothing to say.
        let theme = Theme::dark();
        assert_eq!(theme.severity_ink(Severity::Ok), None);
        assert_eq!(colour().paint("Ready", Severity::Ok), "Ready");
    }

    #[test]
    fn the_readings_worth_looking_at_are_the_ones_with_ink() {
        let theme = Theme::dark();
        assert_eq!(theme.severity_ink(Severity::Warn), Some(theme.warning));
        assert_eq!(theme.severity_ink(Severity::Critical), Some(theme.danger));
        // An absence, not an alarm: a `-` where a figure could not be read.
        assert_eq!(theme.severity_ink(Severity::Unknown), Some(theme.muted));
    }

    #[test]
    fn the_thresholds_are_the_dashboards_even_though_the_colours_are_not() {
        // The two surfaces draw a severity differently and must never disagree
        // about which severity it is: `severity_ink` re-reads the same four
        // variants and invents no fifth rule.
        let theme = Theme::dark();
        for level in [
            Severity::Ok,
            Severity::Warn,
            Severity::Critical,
            Severity::Unknown,
        ] {
            let dashboard = theme.severity(level).fg;
            match theme.severity_ink(level) {
                Some(ink) => assert_eq!(dashboard, Some(ink), "{level:?}"),
                // The one that differs, and only in that a table leaves it
                // alone where a bar fills it green.
                None => assert_eq!(dashboard, Some(theme.success), "{level:?}"),
            }
        }
    }

    #[test]
    fn a_painted_cell_sets_the_colour_and_puts_it_back() {
        // The exact bytes, because a sequence with a typo in it is a column
        // five characters out of place on somebody else's terminal, and
        // nothing short of an assertion on the escape itself would catch it.
        let theme = Theme::dark();
        let Color::Rgb(r, g, b) = theme.danger else {
            panic!("the dark theme's danger colour is expected to be 24-bit");
        };

        assert_eq!(
            colour().paint("NotReady", Severity::Critical),
            format!("\x1b[38;2;{r};{g};{b}mNotReady\x1b[39m")
        );
    }

    #[test]
    fn the_reset_puts_back_the_foreground_and_nothing_else() {
        // `39`, not `0`. A full reset would also clear bold, italics, and the
        // background — none of which this tool set, and one of which the
        // user's pager may have.
        assert!(colour().paint("x", Severity::Warn).ends_with("\x1b[39m"));
        assert!(!colour().paint("x", Severity::Warn).contains("\x1b[0m"));
    }

    #[test]
    fn a_plain_palette_writes_no_escapes_at_all() {
        for level in [
            Severity::Ok,
            Severity::Warn,
            Severity::Critical,
            Severity::Unknown,
        ] {
            assert_eq!(Palette::Plain.paint("97%", level), "97%", "{level:?}");
        }
        assert!(!Palette::Plain.is_colour());
        assert!(colour().is_colour());
    }

    #[test]
    fn nothing_is_the_default_so_a_forgotten_palette_prints_plain() {
        // The safe direction to be wrong in: a code path that forgets to pass
        // a palette prints the table it printed before, rather than escape
        // sequences into somebody's file.
        assert_eq!(Palette::default(), Palette::Plain);
    }

    #[test]
    fn an_empty_cell_is_never_wrapped_in_invisible_ink() {
        // A zero-width cell in escapes is a sequence the table's trailing-space
        // trim cannot see, so it would leave a line ending in ink with nothing
        // in it.
        assert_eq!(colour().paint("", Severity::Critical), "");
    }

    #[test]
    fn auto_colours_a_terminal_and_leaves_a_pipe_alone() {
        let auto = |terminal| Palette::choose(ColourChoice::Auto, terminal, None, None);

        assert!(auto(true).is_colour());
        // `eks nodes | grep NotReady` must be the bytes it was before colour
        // existed; nothing downstream asked to strip escapes.
        assert!(!auto(false).is_colour());
    }

    #[test]
    fn no_color_turns_auto_off_and_an_empty_value_does_not() {
        let auto = |no_color: Option<&str>| {
            Palette::choose(
                ColourChoice::Auto,
                true,
                no_color.map(OsStr::new),
                Some(OsStr::new("xterm-256color")),
            )
        };

        assert!(!auto(Some("1")).is_colour());
        assert!(!auto(Some("anything at all")).is_colour());
        // The spec's own rule: an empty value means "not set". A shell that
        // exports `NO_COLOR=` into every process must not silently turn colour
        // off everywhere.
        assert!(auto(Some("")).is_colour());
        assert!(auto(None).is_colour());
    }

    #[test]
    fn a_terminal_that_says_it_is_dumb_is_believed() {
        let auto =
            |term: &str| Palette::choose(ColourChoice::Auto, true, None, Some(OsStr::new(term)));

        assert!(!auto("dumb").is_colour());
        assert!(auto("xterm-256color").is_colour());
        assert!(auto("screen").is_colour());
        // Not a prefix match: `dumb` is the one value that promises no escape
        // sequences are understood, and `dumb-emacs-ansi` does not say that.
        assert!(auto("dumb-emacs-ansi").is_colour());
    }

    #[test]
    fn what_the_user_typed_beats_the_environment_in_both_directions() {
        // `--color=always` for a pager, on a machine whose shell exports
        // NO_COLOR and whose TERM is dumb: they asked, and they are looking at
        // the answer.
        assert!(
            Palette::choose(
                ColourChoice::Always,
                false,
                Some(OsStr::new("1")),
                Some(OsStr::new("dumb")),
            )
            .is_colour()
        );
        // And the other way: a terminal that would have been coloured.
        assert!(!Palette::choose(ColourChoice::Never, true, None, None).is_colour());
    }

    #[test]
    fn the_default_choice_is_the_one_that_looks_at_the_terminal() {
        assert_eq!(ColourChoice::default(), ColourChoice::Auto);
    }

    #[test]
    fn every_colour_a_theme_can_hold_has_an_escape_sequence() {
        // A palette is only as honest as this mapping: a colour with no
        // sequence prints plain, silently, and a light theme built from the
        // named colours would lose its ink without a word.
        let named = [
            (Color::Black, "\x1b[30m"),
            (Color::Red, "\x1b[31m"),
            (Color::Green, "\x1b[32m"),
            (Color::Yellow, "\x1b[33m"),
            (Color::Blue, "\x1b[34m"),
            (Color::Magenta, "\x1b[35m"),
            (Color::Cyan, "\x1b[36m"),
            (Color::Gray, "\x1b[37m"),
            (Color::DarkGray, "\x1b[90m"),
            (Color::LightRed, "\x1b[91m"),
            (Color::LightGreen, "\x1b[92m"),
            (Color::LightYellow, "\x1b[93m"),
            (Color::LightBlue, "\x1b[94m"),
            (Color::LightMagenta, "\x1b[95m"),
            (Color::LightCyan, "\x1b[96m"),
            (Color::White, "\x1b[97m"),
            (Color::Rgb(0xE0, 0x6C, 0x75), "\x1b[38;2;224;108;117m"),
            (Color::Indexed(203), "\x1b[38;5;203m"),
        ];

        for (colour, expected) in named {
            assert_eq!(foreground(colour).as_deref(), Some(expected), "{colour:?}");
        }

        // "Whatever the terminal was already using" is the absence of a
        // sequence, not a sequence that sets it.
        assert_eq!(foreground(Color::Reset), None);
    }
}
