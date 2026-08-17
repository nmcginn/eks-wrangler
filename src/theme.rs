//! The colour palette, in one place.
//!
//! Every style the TUI draws comes from here. Keeping it centralised is what
//! makes a consistent look cheap to maintain — and makes a light-mode or
//! user-configurable theme a change to one file rather than a hundred call
//! sites.

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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

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
}
