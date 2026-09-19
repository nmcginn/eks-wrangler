//! The user's config file: colour, refresh interval, and default namespace,
//! read once at startup and overridden by anything the command line sets.
//!
//! Precedence is CLI flag, then config file, then built-in default — the same
//! order `--sort` and `--login` already answer their own defaults in, just
//! with a file added underneath. [`Config`] holds only what the file
//! supplied, so a key it left out reads as `None` rather than as a value
//! nobody wrote; [`crate::cli::GlobalArgs::effective_color`] and its two
//! siblings are the one place that chain is resolved.

use std::path::{Path, PathBuf};

use clap::ValueEnum;
use serde::Deserialize;

use crate::k8s::page::ParseError as DurationParseError;
use crate::theme::ColourChoice;
use crate::ui::RefreshInterval;

/// Settings read from the config file. Every field is optional: an absent
/// file, or a key the file left out, means "fall through to the next thing
/// in the precedence chain" — the same rule a CLI flag nobody typed follows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub color: Option<ColourChoice>,
    pub refresh: Option<RefreshInterval>,
    pub namespace: Option<String>,
}

/// Where the config file lives: `~/.config/eks/config.toml`, following
/// `kubeconfig::search_paths`' own choice of a literal path under the home
/// directory over a platform-varying one — see decision 103. `None` when
/// there is no resolvable home directory, which is not an error: it just
/// means there is nowhere this file could be.
#[must_use]
pub fn path() -> Option<PathBuf> {
    let dirs = directories::UserDirs::new()?;
    Some(
        dirs.home_dir()
            .join(".config")
            .join("eks")
            .join("config.toml"),
    )
}

/// Why the config file could not be used as written.
///
/// Never fatal — see [`load`] — so this is worded as a warning rather than as
/// an error a caller has to decide how to react to.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Warning {
    /// The file exists but could not be read at all — permissions, most
    /// likely, since a missing file is not a [`Warning`] in the first place.
    #[error("could not read {path}: {message}")]
    Unreadable { path: PathBuf, message: String },
    /// The file was read, but its TOML was malformed, named an unknown key,
    /// or gave one of the three settings a value that does not parse — a
    /// bad `color`, or a `refresh` that is not a duration.
    #[error("could not use {path}: {message}")]
    Invalid { path: PathBuf, message: String },
}

/// Read the config file at `path`.
///
/// The value returned is always usable, per CLAUDE.md's rule that a
/// malformed file warns and falls back rather than exiting: a missing file
/// and a broken one both come back as [`Config::default`], and the
/// difference is only whether a [`Warning`] comes with it. A broken file
/// loses the whole file's settings rather than salvaging the fields that did
/// parse — simpler to reason about than a partial config, and consistent
/// with a CLI flag rejected outright rather than half-applied.
#[must_use]
pub fn load(path: &Path) -> (Config, Option<Warning>) {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (Config::default(), None);
        }
        Err(error) => {
            return (
                Config::default(),
                Some(Warning::Unreadable {
                    path: path.to_owned(),
                    message: error.to_string(),
                }),
            );
        }
    };
    parse(&text, path)
}

fn parse(text: &str, path: &Path) -> (Config, Option<Warning>) {
    let invalid = |message: String| {
        (
            Config::default(),
            Some(Warning::Invalid {
                path: path.to_owned(),
                message,
            }),
        )
    };

    match toml::from_str::<RawConfig>(text) {
        Ok(raw) => match raw.resolve() {
            Ok(config) => (config, None),
            Err(message) => invalid(message),
        },
        Err(error) => invalid(error.to_string()),
    }
}

/// The file's own shape: every value still a string, so parsing each one
/// reuses the same grammar the equivalent CLI flag already accepts rather
/// than a second reading of "auto" or "30s". `deny_unknown_fields` is what
/// turns a typo'd key into a warning naming it, rather than a setting that
/// silently never takes effect.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(alias = "colour")]
    color: Option<String>,
    refresh: Option<String>,
    namespace: Option<String>,
}

impl RawConfig {
    fn resolve(self) -> Result<Config, String> {
        let color = self
            .color
            .map(|value| {
                ColourChoice::from_str(&value, true)
                    .map_err(|_| format!("color {value:?} is not one of auto, always, never"))
            })
            .transpose()?;
        let refresh = self
            .refresh
            .map(|value| {
                value
                    .parse::<RefreshInterval>()
                    .map_err(|error: DurationParseError| format!("refresh {value:?}: {error}"))
            })
            .transpose()?;

        Ok(Config {
            color,
            refresh,
            namespace: self.namespace,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::time::Duration;

    use super::*;

    #[test]
    fn a_missing_file_is_the_default_config_with_no_warning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.toml");

        let (config, warning) = load(&path);

        assert_eq!(config, Config::default());
        assert!(warning.is_none());
    }

    #[test]
    fn an_empty_file_is_the_default_config() {
        let (config, warning) = parse("", Path::new("config.toml"));

        assert_eq!(config, Config::default());
        assert!(warning.is_none());
    }

    #[test]
    fn every_key_parses_through_the_same_grammar_the_flags_use() {
        let (config, warning) = parse(
            "color = \"always\"\nrefresh = \"5s\"\nnamespace = \"payments\"\n",
            Path::new("config.toml"),
        );

        assert!(warning.is_none());
        assert_eq!(config.color, Some(ColourChoice::Always));
        assert_eq!(
            config.refresh,
            Some(RefreshInterval::every(Duration::from_secs(5)))
        );
        assert_eq!(config.namespace.as_deref(), Some("payments"));
    }

    #[test]
    fn colour_is_accepted_as_an_alias_for_color() {
        let (config, warning) = parse("colour = \"never\"\n", Path::new("config.toml"));

        assert!(warning.is_none());
        assert_eq!(config.color, Some(ColourChoice::Never));
    }

    #[test]
    fn a_partial_file_leaves_the_rest_absent() {
        let (config, warning) = parse("refresh = \"1m\"\n", Path::new("config.toml"));

        assert!(warning.is_none());
        assert_eq!(config.color, None);
        assert_eq!(
            config.refresh,
            Some(RefreshInterval::every(Duration::from_secs(60)))
        );
        assert_eq!(config.namespace, None);
    }

    #[test]
    fn malformed_toml_falls_back_to_defaults_with_a_warning_naming_the_file() {
        let (config, warning) = parse("color = [", Path::new("/home/x/.config/eks/config.toml"));

        assert_eq!(config, Config::default());
        let warning = warning.expect("malformed TOML should warn");
        assert!(matches!(warning, Warning::Invalid { .. }));
        assert!(
            warning
                .to_string()
                .contains("/home/x/.config/eks/config.toml")
        );
    }

    #[test]
    fn an_unknown_key_falls_back_to_defaults_with_a_warning() {
        let (config, warning) = parse("colr = \"always\"\n", Path::new("config.toml"));

        assert_eq!(config, Config::default());
        assert!(warning.is_some());
    }

    #[test]
    fn a_bad_color_value_falls_back_to_defaults_with_a_warning_naming_it() {
        let (config, warning) = parse("color = \"sometimes\"\n", Path::new("config.toml"));

        assert_eq!(config, Config::default());
        let warning = warning.expect("an unrecognised color should warn");
        assert!(warning.to_string().contains("sometimes"));
    }

    #[test]
    fn a_bad_refresh_value_falls_back_to_defaults_with_a_warning_naming_it() {
        let (config, warning) = parse("refresh = \"soon\"\n", Path::new("config.toml"));

        assert_eq!(config, Config::default());
        let warning = warning.expect("an unparsable refresh should warn");
        assert!(warning.to_string().contains("soon"));
    }

    #[test]
    fn an_unreadable_file_falls_back_to_defaults_with_a_warning() {
        // A directory where a file is expected is read-to-string's simplest
        // reliable way to fail with something other than `NotFound`, without
        // relying on filesystem permissions that behave differently as root.
        let dir = tempfile::tempdir().expect("tempdir");
        let as_a_directory = dir.path().join("config.toml");
        std::fs::create_dir(&as_a_directory).expect("mkdir");

        let (config, warning) = load(&as_a_directory);

        assert_eq!(config, Config::default());
        assert!(matches!(warning, Some(Warning::Unreadable { .. })));
    }

    #[test]
    fn the_path_sits_under_the_home_directory_dot_config() {
        let Some(path) = path() else {
            // No resolvable home directory in this environment (e.g. some CI
            // sandboxes) — nothing to assert against.
            return;
        };

        assert!(path.ends_with(".config/eks/config.toml"));
    }
}
