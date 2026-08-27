//! Reading the handful of `~/.aws/config` keys that say whether a profile
//! authenticates through IAM Identity Center, and which session it uses.
//!
//! Four keys are wanted — `sso_session`, `sso_start_url`, `source_profile`, and
//! the `sso_start_url` inside an `[sso-session]` block — and that is the whole
//! reason this is hand-written rather than a dependency. AWS's config format
//! looks like INI and is not: a value may be empty and continue as an indented
//! block underneath it,
//!
//! ```text
//! [profile prod]
//! s3 =
//!   addressing_style = path
//! ```
//!
//! which a general-purpose INI reader either rejects or folds into the section
//! as bare keys. Nothing here needs those sub-properties, so the parser skips
//! any indented line rather than trying to model them, and the four keys it
//! does want are the ones nobody writes that way.
//!
//! [`parse`] is pure over the file's text, so every awkward spelling — a
//! `[default]` that is not `[profile default]`, a `source_profile` chain, a
//! `source_profile` cycle — is a fixture rather than a directory somebody has
//! to arrange.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Where an IAM Identity Center login for a profile would point.
///
/// `session` is the `[sso-session]` block's name for the modern spelling, and
/// `None` for the legacy one where `sso_start_url` sits in the profile itself.
/// It is carried for the message rather than for the command: `aws sso login`
/// is always given `--profile`, which works under both spellings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sso {
    pub start_url: String,
    pub session: Option<String>,
}

/// The parsed contents of `~/.aws/config`, reduced to what this tool asks of it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AwsConfig {
    profiles: BTreeMap<String, BTreeMap<String, String>>,
    sessions: BTreeMap<String, BTreeMap<String, String>>,
}

/// How far a `source_profile` chain is followed before we call it a loop.
///
/// A cycle is already caught by the visited set; this is the second bound, for
/// a chain that is merely absurd. Nobody has ten hops on purpose.
const MAX_HOPS: usize = 10;

impl AwsConfig {
    /// Read and parse the AWS config file, returning an empty config when there
    /// is not one.
    ///
    /// A missing or unreadable `~/.aws/config` is not an error: plenty of
    /// people authenticate with environment variables or an instance role and
    /// have no such file, and refusing to talk to their cluster over it would
    /// be absurd. The answer in that case is simply "no Identity Center session
    /// to speak of", which is what an empty config says.
    #[must_use]
    pub fn load_from(path: &std::path::Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => parse(&text),
            Err(error) => {
                tracing::debug!(path = %path.display(), %error, "no AWS config file to read");
                Self::default()
            }
        }
    }

    /// Where the AWS CLI would look for its config file.
    ///
    /// `AWS_CONFIG_FILE` first, exactly as the AWS CLI reads it, then
    /// `~/.aws/config`. `None` only when there is no home directory to build
    /// the fallback from, which [`load_from`](Self::load_from)'s caller treats
    /// as "no config", not as a failure.
    ///
    /// `env` is the same layered lookup [`super::profile::profile_for`] takes,
    /// and for the same reason: a context whose `exec` block sets
    /// `AWS_CONFIG_FILE` must be *decided* against the file it will then fetch
    /// its token from. Reading one config to work out that the session is stale
    /// and logging in against another is the one way this check could confidently
    /// send somebody to the wrong account.
    #[must_use]
    pub fn path_from(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
        if let Some(value) = env("AWS_CONFIG_FILE")
            && !value.is_empty()
        {
            return Some(PathBuf::from(value));
        }

        let dirs = directories::UserDirs::new()?;
        Some(dirs.home_dir().join(".aws").join("config"))
    }

    /// The Identity Center session a profile authenticates through, following
    /// `source_profile` until it finds one.
    ///
    /// `None` for a profile that does not use Identity Center at all — static
    /// keys, a `credential_process`, an instance role — which is a perfectly
    /// ordinary thing to have and the reason this returns an `Option` rather
    /// than an error.
    #[must_use]
    pub fn sso_for(&self, profile: &str) -> Option<Sso> {
        let mut seen = Vec::new();
        let mut current = profile.to_owned();

        for _ in 0..MAX_HOPS {
            if seen.contains(&current) {
                tracing::debug!(profile, "source_profile chain loops; giving up on it");
                return None;
            }

            let entries = self.profiles.get(&current)?;

            // The modern spelling: the profile names a session block, and the
            // start URL lives there so several profiles can share one login.
            if let Some(name) = entries.get("sso_session") {
                let start_url = self.sessions.get(name)?.get("sso_start_url")?;
                return Some(Sso {
                    start_url: start_url.clone(),
                    session: Some(name.clone()),
                });
            }

            // The legacy spelling, still written by older `aws configure sso`
            // runs: the start URL sits in the profile itself.
            if let Some(start_url) = entries.get("sso_start_url") {
                return Some(Sso {
                    start_url: start_url.clone(),
                    session: None,
                });
            }

            let source = entries.get("source_profile")?.clone();
            seen.push(std::mem::replace(&mut current, source));
        }

        tracing::debug!(profile, "source_profile chain is too long to follow");
        None
    }
}

/// Which kind of section a `[...]` header opened, or `None` for one this tool
/// has nothing to say about — `[services x]`, `[plugins]`.
enum Section {
    Profile(String),
    Session(String),
}

impl Section {
    fn of(header: &str) -> Option<Self> {
        // `[default]` is the one profile written without the `profile` word.
        // `[profile default]` is legal too and names the same thing.
        if header == "default" {
            return Some(Self::Profile("default".to_owned()));
        }
        if let Some(name) = header.strip_prefix("profile ") {
            return Some(Self::Profile(name.trim().to_owned()));
        }
        if let Some(name) = header.strip_prefix("sso-session ") {
            return Some(Self::Session(name.trim().to_owned()));
        }
        None
    }
}

/// Parse the text of an AWS config file.
///
/// Unknown sections and unparseable lines are skipped rather than rejected: a
/// config file this tool cannot fully understand is still one the AWS CLI reads
/// happily, and failing here would block a cluster over a key we never wanted.
#[must_use]
pub fn parse(text: &str) -> AwsConfig {
    let mut config = AwsConfig::default();
    let mut current: Option<Section> = None;

    for line in text.lines() {
        // An indented line continues the key above it as a sub-property — the
        // `s3 =` block in this module's doc comment. None of the four keys we
        // want is ever written that way, so skipping is exactly right, and it
        // has to come before the trim below or the sub-property would read as
        // a key of the section itself.
        if line.starts_with([' ', '\t']) {
            continue;
        }

        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }

        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            current = Section::of(header.trim());
            continue;
        }

        let Some(section) = current.as_ref() else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if key.is_empty() {
            continue;
        }

        let entries = match section {
            Section::Profile(name) => config.profiles.entry(name.clone()),
            Section::Session(name) => config.sessions.entry(name.clone()),
        };
        entries
            .or_default()
            .insert(key.to_owned(), value.to_owned());
    }

    config
}

/// Drop a trailing comment, which AWS's own parser only recognises when the
/// marker is preceded by whitespace or begins the line.
///
/// The whitespace rule is what keeps a `#` inside a value — a URL fragment, a
/// role name — from truncating it.
fn strip_comment(line: &str) -> &str {
    if line.starts_with(['#', ';']) {
        return "";
    }

    let bytes = line.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if (*byte == b'#' || *byte == b';')
            && index > 0
            && matches!(bytes.get(index - 1), Some(b' ' | b'\t'))
        {
            return line.get(..index).unwrap_or(line);
        }
    }

    line
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    const START_URL: &str = "https://acme.awsapps.com/start";

    #[test]
    fn a_profile_naming_a_session_block_takes_its_start_url_from_there() {
        let config = parse(
            "[profile prod]\n\
             sso_session = corp\n\
             sso_account_id = 111122223333\n\
             \n\
             [sso-session corp]\n\
             sso_start_url = https://acme.awsapps.com/start\n\
             sso_region = us-east-1\n",
        );

        assert_eq!(
            config.sso_for("prod"),
            Some(Sso {
                start_url: START_URL.to_owned(),
                session: Some("corp".to_owned()),
            })
        );
    }

    #[test]
    fn the_legacy_spelling_keeps_the_start_url_in_the_profile() {
        let config = parse(
            "[profile old]\n\
             sso_start_url = https://acme.awsapps.com/start\n\
             sso_region = us-east-1\n\
             sso_role_name = Admin\n",
        );

        assert_eq!(
            config.sso_for("old"),
            Some(Sso {
                start_url: START_URL.to_owned(),
                session: None,
            })
        );
    }

    #[test]
    fn a_bare_default_section_is_the_default_profile() {
        let config = parse(
            "[default]\n\
             sso_start_url = https://acme.awsapps.com/start\n",
        );

        assert!(config.sso_for("default").is_some());
    }

    #[test]
    fn profile_default_names_the_same_profile_as_default() {
        // Both spellings are legal and mean one profile. A tool that read only
        // one of them would work for half its users.
        let config = parse(
            "[profile default]\n\
             sso_start_url = https://acme.awsapps.com/start\n",
        );

        assert!(config.sso_for("default").is_some());
    }

    #[test]
    fn a_source_profile_chain_is_followed_to_the_session_behind_it() {
        // The shape of a profile that assumes a role: the role has no login of
        // its own, and the credentials it starts from do.
        let config = parse(
            "[profile deploy]\n\
             role_arn = arn:aws:iam::111122223333:role/Deploy\n\
             source_profile = middle\n\
             \n\
             [profile middle]\n\
             source_profile = base\n\
             \n\
             [profile base]\n\
             sso_session = corp\n\
             \n\
             [sso-session corp]\n\
             sso_start_url = https://acme.awsapps.com/start\n",
        );

        assert_eq!(
            config.sso_for("deploy").map(|sso| sso.start_url),
            Some(START_URL.to_owned())
        );
    }

    #[test]
    fn a_source_profile_cycle_ends_rather_than_spinning() {
        let config = parse(
            "[profile a]\n\
             source_profile = b\n\
             \n\
             [profile b]\n\
             source_profile = a\n",
        );

        assert_eq!(config.sso_for("a"), None);
    }

    #[test]
    fn a_profile_with_no_identity_centre_anywhere_has_no_session() {
        // Static keys and a `credential_process` are both perfectly ordinary,
        // and neither has anything to log in to.
        let config = parse(
            "[profile keys]\n\
             aws_access_key_id = AKIAIOSFODNN7EXAMPLE\n\
             \n\
             [profile helper]\n\
             credential_process = /usr/local/bin/my-creds\n",
        );

        assert_eq!(config.sso_for("keys"), None);
        assert_eq!(config.sso_for("helper"), None);
    }

    #[test]
    fn a_profile_that_is_not_in_the_file_has_no_session() {
        assert_eq!(
            parse("[default]\nregion = us-east-1\n").sso_for("prod"),
            None
        );
    }

    #[test]
    fn a_session_a_profile_names_but_the_file_does_not_define_has_no_start_url() {
        // Worth telling apart from "no SSO here": the answer is still that we
        // cannot log this profile in, and guessing a start URL would be worse.
        let config = parse("[profile prod]\nsso_session = missing\n");

        assert_eq!(config.sso_for("prod"), None);
    }

    #[test]
    fn indented_sub_properties_are_skipped_rather_than_read_as_keys() {
        // The nested form a general-purpose INI reader gets wrong. The
        // indented `sso_start_url` here belongs to nothing and must not be
        // mistaken for the profile's own.
        let config = parse(
            "[profile prod]\n\
             s3 =\n\
             \x20 addressing_style = path\n\
             \x20 sso_start_url = https://wrong.example/start\n\
             sso_start_url = https://acme.awsapps.com/start\n",
        );

        assert_eq!(
            config.sso_for("prod").map(|sso| sso.start_url),
            Some(START_URL.to_owned())
        );
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let config = parse(
            "# the corporate login\n\
             ; an older comment marker\n\
             [profile prod]\n\
             \n\
             sso_start_url = https://acme.awsapps.com/start  # inline\n",
        );

        assert_eq!(
            config.sso_for("prod").map(|sso| sso.start_url),
            Some(START_URL.to_owned())
        );
    }

    #[test]
    fn a_hash_inside_a_value_does_not_truncate_it() {
        // AWS only treats a marker as a comment when whitespace precedes it,
        // and so must we: a role name or a URL fragment is part of the value.
        let config = parse("[profile prod]\nsso_start_url = https://acme.example/start#frag\n");

        assert_eq!(
            config.sso_for("prod").map(|sso| sso.start_url),
            Some("https://acme.example/start#frag".to_owned())
        );
    }

    #[test]
    fn carriage_returns_are_not_part_of_a_value() {
        // A config file written on Windows, or checked out with CRLF endings.
        let config = parse("[profile prod]\r\nsso_start_url = https://acme.awsapps.com/start\r\n");

        assert_eq!(
            config.sso_for("prod").map(|sso| sso.start_url),
            Some(START_URL.to_owned())
        );
    }

    #[test]
    fn keys_without_spaces_around_the_equals_parse_too() {
        let config = parse("[profile prod]\nsso_start_url=https://acme.awsapps.com/start\n");

        assert!(config.sso_for("prod").is_some());
    }

    #[test]
    fn sections_this_tool_has_no_use_for_are_skipped() {
        // `[services]` blocks carry their own nested keys; reading them as a
        // profile's would be worse than ignoring them.
        let config = parse(
            "[services local]\n\
             sso_start_url = https://wrong.example/start\n\
             \n\
             [profile prod]\n\
             sso_start_url = https://acme.awsapps.com/start\n",
        );

        assert_eq!(
            config.sso_for("prod").map(|sso| sso.start_url),
            Some(START_URL.to_owned())
        );
    }

    #[test]
    fn a_named_config_file_is_used_ahead_of_the_one_in_the_home_directory() {
        let named = AwsConfig::path_from(&|name| {
            (name == "AWS_CONFIG_FILE").then(|| "/etc/aws-config".to_owned())
        });

        assert_eq!(named, Some(PathBuf::from("/etc/aws-config")));
    }

    #[test]
    fn an_empty_config_file_variable_falls_back_to_the_home_directory() {
        // `AWS_CONFIG_FILE=` in a shell means "unset", not a file called "".
        let path = AwsConfig::path_from(&|_| Some(String::new()));

        assert!(
            path.is_none_or(|path| path.ends_with(".aws/config")),
            "an empty value should not name a config file"
        );
    }

    #[test]
    fn a_missing_config_file_reads_as_no_sessions_rather_than_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let config = AwsConfig::load_from(&directory.path().join("nothing-here"));

        assert_eq!(config, AwsConfig::default());
        assert_eq!(config.sso_for("default"), None);
    }

    #[test]
    fn an_empty_file_parses_to_an_empty_config() {
        assert_eq!(parse(""), AwsConfig::default());
    }
}
