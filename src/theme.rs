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

    /// The light theme, tuned for readability on a light terminal. Every
    /// colour here is darker/more saturated than its `dark()` counterpart —
    /// a pastel that reads fine on black turns nearly invisible on white, so
    /// this is not `dark()`'s palette lightened, but a second one chosen
    /// against the opposite background. See `tests::
    /// both_themes_meet_wcag_aa_contrast_for_body_text` for the numbers this
    /// was picked to clear.
    #[must_use]
    pub const fn light() -> Self {
        Self {
            background: Color::Reset,
            text: Color::Rgb(0x1F, 0x23, 0x28),
            muted: Color::Rgb(0x65, 0x6D, 0x76),
            accent: Color::Rgb(0x0B, 0x72, 0x85),
            border: Color::Rgb(0xD1, 0xD5, 0xDA),
            border_focused: Color::Rgb(0x0B, 0x72, 0x85),
            success: Color::Rgb(0x1A, 0x7F, 0x37),
            warning: Color::Rgb(0x9A, 0x67, 0x00),
            danger: Color::Rgb(0xCF, 0x22, 0x2E),
            selection_bg: Color::Rgb(0xE9, 0xEC, 0xEF),
        }
    }

    /// The theme tuned for a terminal whose background reads as
    /// `background`.
    #[must_use]
    pub const fn for_background(background: Background) -> Self {
        match background {
            Background::Dark => Self::dark(),
            Background::Light => Self::light(),
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

    /// A substring a search has matched, in the container-logs pane.
    ///
    /// Deliberately not [`Self::selected`]: that marks the row under the
    /// cursor, and a matched log line is neither selected nor removed from
    /// its neighbours, so it needs ink of its own rather than borrowing the
    /// row-highlight's background. Bold and underlined in the accent colour
    /// — [`Self::heading`]'s own colour, with the underline as the one thing
    /// that tells the two apart at a glance.
    #[must_use]
    pub fn match_highlight(self) -> Style {
        Style::default()
            .fg(self.accent)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
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

    /// Classify a pod's usage against its own request, rather than a node's
    /// allocatable.
    ///
    /// [`from_utilisation`](Self::from_utilisation) answers "how full is this
    /// node", where approaching capacity is the whole risk. A pod's own
    /// request has no such ceiling: CPU is compressible, so running above a
    /// request is exactly what a burst spends, and a pod sitting at 90% of
    /// what it asked for is well sized, not nearly full. Those thresholds
    /// would tell the reader something untrue, in red, on most rows of a
    /// healthy listing.
    ///
    /// The question worth colouring is not "is this over 100%" but "is this
    /// meaningfully over, for long enough to matter" — a burstable pod that
    /// outgrew its own request is the first thing evicted under memory
    /// pressure and the first throttled under CPU contention, so the request
    /// it is drifting from is a real number to be behind. `1.5` (150% of
    /// request) is where that drift stops looking like an ordinary burst;
    /// `3.0` (300%) is a request so undersized the pod is running on borrowed
    /// capacity most of the time. Everything at or a little above 100% stays
    /// `Ok`, deliberately: a request is a floor a scheduler holds open, not a
    /// ceiling a pod is expected to stay under.
    #[must_use]
    pub fn from_request_share(ratio: f64) -> Self {
        if !ratio.is_finite() || ratio < 0.0 {
            Self::Unknown
        } else if ratio >= 3.0 {
            Self::Critical
        } else if ratio >= 1.5 {
            Self::Warn
        } else {
            Self::Ok
        }
    }
}

/// What the user asked for with `--theme`.
///
/// A `clap::ValueEnum` on the domain type, the same reason `ColourChoice`
/// is one: a value this does not take is rejected with the ones it does
/// listed, before anything connects, rather than parsed into a silent
/// default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ThemeChoice {
    /// Detect the terminal's own background where possible, falling back to
    /// dark when it cannot be told. The default.
    #[default]
    Auto,
    Dark,
    Light,
}

/// Whether a terminal's background reads as dark or light, the one fact
/// [`resolve`] needs beyond what the user typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Background {
    Dark,
    Light,
}

/// Read `COLORFGBG` for a hint about the terminal's own background.
///
/// The exact answer is an OSC 11 query ([`BACKGROUND_QUERY`]), but that means
/// writing to the terminal and waiting on its reply — I/O the CLI's one-shot
/// tables have no event loop to wait on, and which the dashboard only does
/// after its first frame (decision 111). `COLORFGBG` is the one hint that
/// costs nothing, so it is read first everywhere: several
/// terminals and multiplexers (rxvt, and `tmux`/`screen` forwarding it from
/// whatever set it) export it unasked, in `foreground;background` form —
/// sometimes `foreground;default;background`, which is why the *last*
/// `;`-separated field is read rather than the second — as an index into the
/// 16-colour ANSI palette. `7` (light grey, conventionally "white") and
/// `9`–`15` (the bright colours, `15` being bright white) read as a light
/// background; everything else, including `8`'s "bright black", stays dark.
/// Absent, unset, or unparseable is `None` — "cannot be told," not "is
/// dark" — so [`resolve`] is the one place that turns "cannot be told" into
/// a fallback.
#[must_use]
pub fn detect_background(colorfgbg: Option<&OsStr>) -> Option<Background> {
    let value = colorfgbg?.to_str()?;
    let field = value.rsplit(';').next()?;
    let index: u8 = field.trim().parse().ok()?;
    Some(if matches!(index, 7 | 9..=15) {
        Background::Light
    } else {
        Background::Dark
    })
}

/// The OSC 11 query: "what colour is your background?" Sent by the dashboard
/// after its first frame, never by a CLI table — see decision 111.
///
/// Terminated with BEL rather than ST (`ESC \`): xterm answers with whichever
/// terminator the query used, both are accepted by every terminal that
/// answers at all, and BEL is the one older terminals and multiplexers
/// (`screen`, and `tmux` passing it through) have understood the longest —
/// the same choice Vim and Neovim make for the same query.
pub const BACKGROUND_QUERY: &str = "\x1b]11;?\x07";

/// Whether the dashboard should send [`BACKGROUND_QUERY`] at all.
///
/// Only under `auto` — a theme the user named is never second-guessed — and
/// only when `COLORFGBG` could not tell, so a terminal that already answered
/// is not asked twice. `TERM` rules out the two kinds of terminal where
/// asking does harm rather than nothing: unset or `dumb`, which is not a
/// terminal this tool can expect to speak escape sequences at all, and the
/// Linux virtual console (`linux`), which does not know OSC 11 and prints
/// the query's tail (`1;?`) onto the screen instead of ignoring it.
#[must_use]
pub fn should_query_background(
    choice: ThemeChoice,
    colorfgbg: Option<&OsStr>,
    term: Option<&OsStr>,
) -> bool {
    let Some(term) = term.and_then(OsStr::to_str) else {
        return false;
    };
    choice == ThemeChoice::Auto
        && detect_background(colorfgbg).is_none()
        && !term.is_empty()
        && term != "dumb"
        && term != "linux"
        && !term.starts_with("linux-")
}

/// Read a terminal's reply to [`BACKGROUND_QUERY`].
///
/// The reply is `ESC ] 11 ; <colour>` terminated by BEL or ST (`ESC \`),
/// where `<colour>` is X11's `rgb:<r>/<g>/<b>` — each channel one to four
/// hex digits, scaled by its own width, so `rgb:f/f/f`, `rgb:ff/ff/ff` and
/// `rgb:ffff/ffff/ffff` are all white — or `rgba:` with a fourth, ignored,
/// alpha channel, which rxvt-unicode sends. Anything else — a truncated
/// reply, another OSC's answer, an `rgb:` with a channel that is not hex —
/// is `None`: "cannot be told," the same as `COLORFGBG` being unset, never a
/// guess.
#[must_use]
pub fn parse_background_reply(reply: &[u8]) -> Option<Background> {
    let body = reply.strip_prefix(b"\x1b]11;")?;
    let colour = body
        .strip_suffix(b"\x07")
        .or_else(|| body.strip_suffix(b"\x1b\\"))?;
    let colour = std::str::from_utf8(colour).ok()?;
    let channels = if let Some(rgb) = colour.strip_prefix("rgb:") {
        let channels: Vec<&str> = rgb.split('/').collect();
        (channels.len() == 3).then_some(channels)?
    } else {
        let rgba = colour.strip_prefix("rgba:")?;
        let mut channels: Vec<&str> = rgba.split('/').collect();
        (channels.len() == 4).then_some(())?;
        channels.truncate(3);
        channels
    };
    let mut rgb = [0.0; 3];
    for (slot, channel) in rgb.iter_mut().zip(channels) {
        *slot = hex_channel(channel)?;
    }
    Some(Background::of_rgb(rgb))
}

/// One X11 colour channel, one to four hex digits, as a fraction of full
/// intensity.
fn hex_channel(digits: &str) -> Option<f64> {
    if digits.is_empty() || digits.len() > 4 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let value = u16::from_str_radix(digits, 16).ok()?;
    // `len` is 1..=4, so the shift is 4..=16 and the max is 0xF..=0xFFFF.
    let max = (1_u32 << (4 * digits.len())) - 1;
    Some(f64::from(value) / f64::from(max))
}

impl Background {
    /// Classify a background colour by which theme reads better on it: light
    /// when [`Theme::light`]'s body text has more contrast against it than
    /// [`Theme::dark`]'s, dark otherwise — a tie included, dark being the
    /// safer wrong guess for the reason [`resolve`] gives. Tied to the themes'
    /// own ink rather than a bare luminance threshold so the answer is always
    /// "the theme that is more readable here," whatever either palette
    /// becomes. Channels are fractions of full intensity.
    fn of_rgb(rgb: [f64; 3]) -> Self {
        let background = relative_luminance(rgb);
        let contrast = |ink: Color| {
            let Color::Rgb(r, g, b) = ink else {
                return 0.0;
            };
            let ink = relative_luminance([r, g, b].map(|c| f64::from(c) / 255.0));
            let (lighter, darker) = if ink > background {
                (ink, background)
            } else {
                (background, ink)
            };
            (lighter + 0.05) / (darker + 0.05)
        };
        if contrast(Theme::light().text) > contrast(Theme::dark().text) {
            Self::Light
        } else {
            Self::Dark
        }
    }
}

/// Relative luminance, [WCAG 2.1]'s own formula: each channel (a fraction of
/// full intensity) linearised, then weighted by how much the eye actually
/// notices it.
///
/// [WCAG 2.1]: https://www.w3.org/TR/WCAG21/#dfn-relative-luminance
fn relative_luminance([r, g, b]: [f64; 3]) -> f64 {
    fn channel(c: f64) -> f64 {
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

/// Resolve `--theme`/the config file's own `theme` and `COLORFGBG` into the
/// [`Theme`] to draw with.
///
/// Pure over its two answers, the same shape [`Palette::choose`] already is:
/// `choice` wins outright when it names a theme outright, and `Auto` asks
/// [`detect_background`] — falling back to [`Theme::dark`] when that comes
/// back `None`, since a terminal already dark is the safer wrong guess than
/// one this tool just painted unreadable.
#[must_use]
pub fn resolve(choice: ThemeChoice, colorfgbg: Option<&OsStr>) -> Theme {
    match choice {
        ThemeChoice::Dark => Theme::dark(),
        ThemeChoice::Light => Theme::light(),
        ThemeChoice::Auto => match detect_background(colorfgbg) {
            Some(background) => Theme::for_background(background),
            None => Theme::dark(),
        },
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
    /// Decide whether this run prints colour, and in which theme's severity
    /// ink.
    ///
    /// Pure over the five things that decide it, so every combination below is
    /// a test rather than an environment variable somebody has to set:
    ///
    /// - `choice` is `--color`, and it wins outright. The user typed it.
    /// - `theme` is `--theme`/the config file's own `theme`, already resolved
    ///   by [`resolve`] — a listing's colour and a dashboard's are the same
    ///   `Theme`, so `eks nodes` on a light terminal reads its `STATUS`
    ///   column in the same ink the pane beside it would.
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
        theme: Theme,
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
            Self::Colour(theme)
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
    fn request_share_thresholds_are_inclusive_at_the_boundary() {
        assert_eq!(Severity::from_request_share(0.0), Severity::Ok);
        // A pod at 90% of its own request is well sized, not nearly full —
        // the reading `from_utilisation` would give it is the wrong one.
        assert_eq!(Severity::from_request_share(0.90), Severity::Ok);
        assert_eq!(Severity::from_request_share(1.0), Severity::Ok);
        assert_eq!(Severity::from_request_share(1.499), Severity::Ok);
        assert_eq!(Severity::from_request_share(1.5), Severity::Warn);
        assert_eq!(Severity::from_request_share(2.999), Severity::Warn);
        assert_eq!(Severity::from_request_share(3.0), Severity::Critical);
        assert_eq!(Severity::from_request_share(4.5), Severity::Critical);
    }

    #[test]
    fn nonsense_request_share_is_unknown_not_alarming() {
        assert_eq!(Severity::from_request_share(f64::NAN), Severity::Unknown);
        assert_eq!(Severity::from_request_share(-0.1), Severity::Unknown);
    }

    #[test]
    fn focused_panes_are_visually_distinct() {
        let theme = Theme::dark();
        assert_ne!(theme.pane_border(true), theme.pane_border(false));
    }

    /// The palette a CLI listing gets when `--color=always` was typed, without
    /// asking a terminal anything.
    fn colour() -> Palette {
        Palette::choose(ColourChoice::Always, Theme::dark(), false, None, None)
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
        let auto =
            |terminal| Palette::choose(ColourChoice::Auto, Theme::dark(), terminal, None, None);

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
                Theme::dark(),
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
        let auto = |term: &str| {
            Palette::choose(
                ColourChoice::Auto,
                Theme::dark(),
                true,
                None,
                Some(OsStr::new(term)),
            )
        };

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
                Theme::dark(),
                false,
                Some(OsStr::new("1")),
                Some(OsStr::new("dumb")),
            )
            .is_colour()
        );
        // And the other way: a terminal that would have been coloured.
        assert!(!Palette::choose(ColourChoice::Never, Theme::dark(), true, None, None).is_colour());
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

    /// [`super::relative_luminance`] over 8-bit channels, the form every
    /// swatch in these tests is written in.
    fn relative_luminance((r, g, b): (u8, u8, u8)) -> f64 {
        super::relative_luminance([r, g, b].map(|c| f64::from(c) / 255.0))
    }

    /// [WCAG 2.1]'s contrast ratio between two colours, from 1:1 (identical)
    /// to 21:1 (black on white). `4.5:1` is the AA bar for body text.
    ///
    /// [WCAG 2.1]: https://www.w3.org/TR/WCAG21/#dfn-contrast-ratio
    fn contrast_ratio(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
        let (la, lb) = (relative_luminance(a), relative_luminance(b));
        let (lighter, darker) = if la > lb { (la, lb) } else { (lb, la) };
        (lighter + 0.05) / (darker + 0.05)
    }

    fn rgb(colour: Color) -> (u8, u8, u8) {
        let Color::Rgb(r, g, b) = colour else {
            panic!("expected a 24-bit colour, got {colour:?}");
        };
        (r, g, b)
    }

    /// Neither theme paints a background of its own — [`Theme::background`]
    /// stays [`Color::Reset`] in both, trusting whatever the terminal already
    /// shows — so there is no literal swatch to measure `text` against.
    /// These are the backgrounds each theme was tuned to read well on: a
    /// common dark terminal default for [`Theme::dark`], and plain white for
    /// [`Theme::light`], which a user forcing `--theme light` on an
    /// already-light terminal is assumed to be close to.
    const ASSUMED_DARK_BACKGROUND: (u8, u8, u8) = (0x1E, 0x1E, 0x1E);
    const ASSUMED_LIGHT_BACKGROUND: (u8, u8, u8) = (0xFF, 0xFF, 0xFF);

    #[test]
    fn both_themes_meet_wcag_aa_contrast_for_body_text() {
        const AA_NORMAL_TEXT: f64 = 4.5;

        let dark = contrast_ratio(rgb(Theme::dark().text), ASSUMED_DARK_BACKGROUND);
        let light = contrast_ratio(rgb(Theme::light().text), ASSUMED_LIGHT_BACKGROUND);

        assert!(dark >= AA_NORMAL_TEXT, "dark theme body text: {dark:.2}:1");
        assert!(
            light >= AA_NORMAL_TEXT,
            "light theme body text: {light:.2}:1"
        );
    }

    #[test]
    fn detect_background_reads_the_last_colorfgbg_field_as_the_background_index() {
        let read = |value: &str| detect_background(Some(OsStr::new(value)));

        // `foreground;background` — the common two-field form.
        assert_eq!(read("15;0"), Some(Background::Dark));
        assert_eq!(read("0;15"), Some(Background::Light));
        // `7` (light grey) reads as light too; `8` (bright black) stays dark.
        assert_eq!(read("0;7"), Some(Background::Light));
        assert_eq!(read("15;8"), Some(Background::Dark));
        // Konsole's three-field form — the middle "default" is skipped
        // because only the *last* field is read.
        assert_eq!(read("0;default;15"), Some(Background::Light));
    }

    #[test]
    fn detect_background_is_none_when_it_cannot_be_told() {
        assert_eq!(detect_background(None), None);
        assert_eq!(detect_background(Some(OsStr::new(""))), None);
        assert_eq!(detect_background(Some(OsStr::new("not-a-number"))), None);
        assert_eq!(detect_background(Some(OsStr::new("0;256"))), None);
    }

    #[test]
    fn resolve_honours_an_explicit_choice_over_the_terminal() {
        // A light-reading terminal, overridden both ways.
        let light_env = Some(OsStr::new("0;15"));
        assert_eq!(resolve(ThemeChoice::Dark, light_env), Theme::dark());
        let dark_env = Some(OsStr::new("15;0"));
        assert_eq!(resolve(ThemeChoice::Light, dark_env), Theme::light());
    }

    #[test]
    fn resolve_auto_reads_the_terminal_and_falls_back_to_dark() {
        assert_eq!(
            resolve(ThemeChoice::Auto, Some(OsStr::new("0;15"))),
            Theme::light()
        );
        assert_eq!(
            resolve(ThemeChoice::Auto, Some(OsStr::new("15;0"))),
            Theme::dark()
        );
        // Cannot be told at all: the safer wrong guess, not a light theme
        // painted onto a terminal that never said it was one.
        assert_eq!(resolve(ThemeChoice::Auto, None), Theme::dark());
    }

    #[test]
    fn background_reply_reads_every_channel_width_x11_allows() {
        let light = [
            &b"\x1b]11;rgb:ffff/ffff/ffff\x07"[..],
            b"\x1b]11;rgb:fff/fff/fff\x07",
            b"\x1b]11;rgb:ff/ff/ff\x07",
            b"\x1b]11;rgb:f/f/f\x07",
            b"\x1b]11;rgb:FFFF/FFFF/FFFF\x07",
        ];
        for reply in light {
            assert_eq!(
                parse_background_reply(reply),
                Some(Background::Light),
                "{reply:?}"
            );
        }
        let dark = [
            &b"\x1b]11;rgb:0000/0000/0000\x07"[..],
            b"\x1b]11;rgb:1e1e/1e1e/1e1e\x07",
            b"\x1b]11;rgb:28/2c/34\x07",
            b"\x1b]11;rgb:0/0/0\x07",
        ];
        for reply in dark {
            assert_eq!(
                parse_background_reply(reply),
                Some(Background::Dark),
                "{reply:?}"
            );
        }
    }

    #[test]
    fn background_reply_accepts_both_terminators() {
        assert_eq!(
            parse_background_reply(b"\x1b]11;rgb:ffff/ffff/ffff\x07"),
            Some(Background::Light)
        );
        assert_eq!(
            parse_background_reply(b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\"),
            Some(Background::Light)
        );
    }

    #[test]
    fn background_reply_reads_rgba_and_ignores_its_alpha() {
        // rxvt-unicode's form.
        assert_eq!(
            parse_background_reply(b"\x1b]11;rgba:ffff/ffff/ffff/0000\x07"),
            Some(Background::Light)
        );
        assert_eq!(
            parse_background_reply(b"\x1b]11;rgba:0000/0000/0000/ffff\x1b\\"),
            Some(Background::Dark)
        );
    }

    #[test]
    fn background_reply_is_none_for_anything_it_cannot_read() {
        let garbage: &[&[u8]] = &[
            b"",
            b"garbage",
            // Truncated: no terminator.
            b"\x1b]11;rgb:ffff/ffff/ffff",
            // Truncated: no prefix.
            b"rgb:ffff/ffff/ffff\x07",
            // Another OSC's answer — the foreground, not the background.
            b"\x1b]10;rgb:ffff/ffff/ffff\x07",
            // The query echoed back rather than answered.
            b"\x1b]11;?\x07",
            // Two channels, four channels under `rgb:`, three under `rgba:`.
            b"\x1b]11;rgb:ffff/ffff\x07",
            b"\x1b]11;rgb:ffff/ffff/ffff/ffff\x07",
            b"\x1b]11;rgba:ffff/ffff/ffff\x07",
            // An empty channel, five digits, not hex, a sign.
            b"\x1b]11;rgb:ffff//ffff\x07",
            b"\x1b]11;rgb:fffff/ffff/ffff\x07",
            b"\x1b]11;rgb:gggg/ffff/ffff\x07",
            b"\x1b]11;rgb:+fff/ffff/ffff\x07",
            // A colour form X11 has but this does not read.
            b"\x1b]11;#ffffff\x07",
            // Not UTF-8.
            b"\x1b]11;rgb:\xff\xfe/ffff/ffff\x07",
        ];
        for reply in garbage {
            assert_eq!(parse_background_reply(reply), None, "{reply:?}");
        }
    }

    #[test]
    fn a_background_is_light_exactly_when_the_light_theme_reads_better_on_it() {
        // Mid-grey sits past the crossover, where dark ink already has more
        // contrast than light ink; a darker grey sits before it.
        assert_eq!(
            parse_background_reply(b"\x1b]11;rgb:80/80/80\x07"),
            Some(Background::Light)
        );
        assert_eq!(
            parse_background_reply(b"\x1b]11;rgb:60/60/60\x07"),
            Some(Background::Dark)
        );
        // Solarized's two backgrounds land where their names say.
        assert_eq!(
            parse_background_reply(b"\x1b]11;rgb:fdfd/f6f6/e3e3\x07"),
            Some(Background::Light)
        );
        assert_eq!(
            parse_background_reply(b"\x1b]11;rgb:0000/2b2b/3636\x07"),
            Some(Background::Dark)
        );
    }

    #[test]
    fn the_query_is_osc_11_asking_and_terminated_by_bel() {
        assert_eq!(BACKGROUND_QUERY.as_bytes(), b"\x1b]11;?\x07");
    }

    #[test]
    fn the_terminal_is_asked_only_under_auto_and_only_when_colorfgbg_is_silent() {
        let xterm = Some(OsStr::new("xterm-256color"));
        assert!(should_query_background(ThemeChoice::Auto, None, xterm));
        // A theme the user named is never second-guessed.
        assert!(!should_query_background(ThemeChoice::Dark, None, xterm));
        assert!(!should_query_background(ThemeChoice::Light, None, xterm));
        // `COLORFGBG` already answered, either way: not asked twice.
        assert!(!should_query_background(
            ThemeChoice::Auto,
            Some(OsStr::new("0;15")),
            xterm
        ));
        assert!(!should_query_background(
            ThemeChoice::Auto,
            Some(OsStr::new("15;0")),
            xterm
        ));
        // A `COLORFGBG` that says nothing readable is the same as none.
        assert!(should_query_background(
            ThemeChoice::Auto,
            Some(OsStr::new("default;default")),
            xterm
        ));
    }

    #[test]
    fn the_terminal_is_not_asked_where_the_query_would_scribble_on_it() {
        for term in [
            None,
            Some(""),
            Some("dumb"),
            Some("linux"),
            Some("linux-16color"),
        ] {
            assert!(
                !should_query_background(ThemeChoice::Auto, None, term.map(OsStr::new)),
                "{term:?}"
            );
        }
        for term in [
            "xterm-256color",
            "screen-256color",
            "tmux-256color",
            "alacritty",
            "xterm-kitty",
        ] {
            assert!(
                should_query_background(ThemeChoice::Auto, None, Some(OsStr::new(term))),
                "{term}"
            );
        }
    }

    #[test]
    fn for_background_picks_the_theme_tuned_for_it() {
        assert_eq!(Theme::for_background(Background::Light), Theme::light());
        assert_eq!(Theme::for_background(Background::Dark), Theme::dark());
    }

    #[test]
    fn the_default_theme_choice_is_the_one_that_looks_at_the_terminal() {
        assert_eq!(ThemeChoice::default(), ThemeChoice::Auto);
    }

    #[test]
    fn a_resolved_theme_reaches_the_cli_palette() {
        // `--theme light` on a listing colours its severity ink from
        // `Theme::light`, not a hardcoded dark default — the same table a
        // light-mode dashboard would draw beside it.
        let palette = Palette::choose(ColourChoice::Always, Theme::light(), false, None, None);
        assert_eq!(
            palette.paint("NotReady", Severity::Critical),
            colour_with(Theme::light(), Severity::Critical, "NotReady")
        );
        assert_ne!(
            palette.paint("NotReady", Severity::Critical),
            colour_with(Theme::dark(), Severity::Critical, "NotReady")
        );
    }

    fn colour_with(theme: Theme, severity: Severity, text: &str) -> String {
        Palette::choose(ColourChoice::Always, theme, false, None, None)
            .paint(text, severity)
            .into_owned()
    }
}
