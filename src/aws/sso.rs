//! Whether an IAM Identity Center session is still good, read from the AWS
//! CLI's own token cache.
//!
//! The AWS CLI keeps each session's access token in a JSON file under
//! `~/.aws/sso/cache/`, with the moment it expires beside it. That file is the
//! whole reason this tool can answer "will `aws eks get-token` work?" without a
//! network call, a subprocess, or an AWS SDK — it is two small reads on the
//! local disk, which is what keeps the check off the path to first paint.
//!
//! **Entries are matched on the `startUrl` inside them, not on the filename.**
//! The CLI names each file after the SHA-1 of the session name or start URL,
//! and that is an implementation detail of `botocore` rather than a documented
//! contract — a tool that recomputed the hash would break silently the day it
//! changed, and would be reading a hash of a value it is holding anyway.
//! Matching the field costs a directory scan of a few small files and cannot
//! drift. It also skips the `botocore-client-id-*.json` registrations sitting
//! in the same directory for free: they carry no `startUrl`.
//!
//! [`classify`] is pure over the parsed entries and an explicit `now`, so an
//! expiry, a token about to die, and a cache belonging to somebody else's
//! start URL are all fixtures.

use std::path::{Path, PathBuf};

use k8s_openapi::jiff::{SignedDuration, Timestamp};

use super::config::Sso;

/// How much life a token needs left before we call it usable.
///
/// A token with forty seconds on it is not worth starting a paged listing
/// with: it would be refused partway through, and the user would read a
/// credential error about a session that was alive when they pressed Enter.
/// Treating the last minute as already gone turns that into one login before
/// anything is asked of the cluster.
const SKEW: SignedDuration = SignedDuration::from_secs(60);

/// What the token cache says about one profile's Identity Center session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Session {
    /// Good for a while yet, so there is nothing to do.
    Valid { expires_at: Timestamp },
    /// Expired, or inside the last minute of its life — see [`classify`],
    /// which folds the two together. `expires_at` may therefore be a moment in
    /// the *future*, which is why it is not called `at`.
    Expired { expires_at: Timestamp },
    /// The profile authenticates through Identity Center, and there is no
    /// cached token for it — a first run, or a cache somebody cleared.
    Missing,
    /// This profile does not use Identity Center at all: static keys, a
    /// `credential_process`, an instance role. There is nothing to log in to,
    /// and today's advice remains the right advice.
    NotSso,
}

/// One usable entry from the cache directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    pub start_url: String,
    pub expires_at: Timestamp,
}

/// The shape of a cache file, reduced to the two fields that answer the
/// question. The access token itself is deliberately never read: this module
/// decides whether to log in, and nothing here ever needs to hold a credential.
#[derive(serde::Deserialize)]
struct CacheFile {
    #[serde(rename = "startUrl")]
    start_url: Option<String>,
    #[serde(rename = "expiresAt")]
    expires_at: Option<String>,
}

/// Where the AWS CLI keeps its Identity Center token cache.
#[must_use]
pub fn cache_dir() -> Option<PathBuf> {
    let dirs = directories::UserDirs::new()?;
    Some(dirs.home_dir().join(".aws").join("sso").join("cache"))
}

/// Read every usable entry out of the cache directory.
///
/// Every failure here is a skip rather than an error, and deliberately: a
/// directory that does not exist means nobody has logged in yet, a file this
/// tool cannot parse belongs to a newer AWS CLI than the one we were written
/// against, and neither is a reason to refuse to talk to a cluster. The worst
/// case of being wrong is offering a login that was not needed.
#[must_use]
pub fn read_cache(dir: &Path) -> Vec<CacheEntry> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        tracing::debug!(dir = %dir.display(), "no SSO token cache to read");
        return Vec::new();
    };

    let mut found = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }

        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(file) = serde_json::from_str::<CacheFile>(&text) else {
            tracing::debug!(path = %path.display(), "skipping an SSO cache file we cannot read");
            continue;
        };

        // A client registration rather than a session: same directory, no
        // `startUrl`, nothing to say about whether anyone is logged in.
        let (Some(start_url), Some(expires_at)) = (file.start_url, file.expires_at) else {
            continue;
        };
        let Some(expires_at) = parse_expiry(&expires_at) else {
            tracing::debug!(path = %path.display(), "skipping an SSO cache file with an expiry we cannot read");
            continue;
        };

        found.push(CacheEntry {
            start_url,
            expires_at,
        });
    }

    found
}

/// Parse the `expiresAt` field, in either spelling the AWS CLI has written.
///
/// Current versions write RFC 3339 (`2026-08-26T20:00:00Z`); older `botocore`
/// releases wrote the same instant with a literal `UTC` suffix instead of the
/// zone offset, and a cache written by one is read by the other. Handling both
/// costs three lines and saves a user on an older CLI from being told to log in
/// every single time.
fn parse_expiry(text: &str) -> Option<Timestamp> {
    if let Ok(stamp) = text.parse::<Timestamp>() {
        return Some(stamp);
    }
    text.strip_suffix("UTC")?
        .trim_end()
        .parse::<Timestamp>()
        .ok()
        .or_else(|| {
            format!("{}Z", text.strip_suffix("UTC")?.trim_end())
                .parse()
                .ok()
        })
}

/// Decide what the cache says about this profile's session.
///
/// Pure over the entries, the session being asked about, and an explicit `now`,
/// for the reason every other computation in this tool takes its own clock: a
/// token that expires in fifty-nine seconds is a fixture rather than a wait.
#[must_use]
pub fn classify(entries: &[CacheEntry], sso: &Sso, now: Timestamp) -> Session {
    // Several entries can name one start URL — a re-login writes a new file
    // before the old one is swept up — so the freshest is the one that decides.
    let latest = entries
        .iter()
        .filter(|entry| same_start_url(&entry.start_url, &sso.start_url))
        .map(|entry| entry.expires_at)
        .max();

    match latest {
        None => Session::Missing,
        Some(expires_at) if expires_at.duration_since(now) > SKEW => Session::Valid { expires_at },
        Some(expires_at) => Session::Expired { expires_at },
    }
}

/// Whether two start URLs name the same Identity Center portal.
///
/// A trailing slash is the difference that actually turns up: `aws configure
/// sso` stores what the user typed, and half of them paste it with one. Nothing
/// clever beyond that — these are two spellings of one URL, not two URLs to
/// decide the equivalence of.
fn same_start_url(left: &str, right: &str) -> bool {
    left.trim_end_matches('/')
        .eq_ignore_ascii_case(right.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    const START_URL: &str = "https://acme.awsapps.com/start";

    fn now() -> Timestamp {
        "2026-08-26T12:00:00Z".parse().unwrap()
    }

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn sso() -> Sso {
        Sso {
            start_url: START_URL.to_owned(),
            session: Some("corp".to_owned()),
        }
    }

    fn entry(start_url: &str, expires_at: &str) -> CacheEntry {
        CacheEntry {
            start_url: start_url.to_owned(),
            expires_at: at(expires_at),
        }
    }

    #[test]
    fn a_token_with_hours_left_is_valid() {
        let entries = [entry(START_URL, "2026-08-26T20:00:00Z")];

        assert_eq!(
            classify(&entries, &sso(), now()),
            Session::Valid {
                expires_at: at("2026-08-26T20:00:00Z")
            }
        );
    }

    #[test]
    fn a_token_that_expired_an_hour_ago_is_expired() {
        let entries = [entry(START_URL, "2026-08-26T11:00:00Z")];

        assert_eq!(
            classify(&entries, &sso(), now()),
            Session::Expired {
                expires_at: at("2026-08-26T11:00:00Z")
            }
        );
    }

    #[test]
    fn a_token_inside_the_last_minute_of_its_life_is_already_expired() {
        // It would die partway through a paged listing, and a credential error
        // about a session that was alive when the user pressed Enter is the
        // worst of both answers.
        let entries = [entry(START_URL, "2026-08-26T12:00:40Z")];

        assert_eq!(
            classify(&entries, &sso(), now()),
            Session::Expired {
                expires_at: at("2026-08-26T12:00:40Z")
            }
        );
    }

    #[test]
    fn a_token_with_more_than_the_skew_left_is_still_valid() {
        // The other side of the boundary above, so the margin is asserted
        // rather than assumed.
        let entries = [entry(START_URL, "2026-08-26T12:01:30Z")];

        assert!(matches!(
            classify(&entries, &sso(), now()),
            Session::Valid { .. }
        ));
    }

    #[test]
    fn a_cache_holding_only_somebody_elses_portal_reads_as_missing() {
        let entries = [entry(
            "https://other.awsapps.com/start",
            "2026-08-26T20:00:00Z",
        )];

        assert_eq!(classify(&entries, &sso(), now()), Session::Missing);
    }

    #[test]
    fn an_empty_cache_reads_as_missing() {
        assert_eq!(classify(&[], &sso(), now()), Session::Missing);
    }

    #[test]
    fn a_trailing_slash_is_the_same_portal() {
        // `aws configure sso` stores what the user typed, and half of them
        // paste it with one.
        let entries = [entry(
            "https://acme.awsapps.com/start/",
            "2026-08-26T20:00:00Z",
        )];

        assert!(matches!(
            classify(&entries, &sso(), now()),
            Session::Valid { .. }
        ));
    }

    #[test]
    fn the_freshest_entry_for_a_portal_decides() {
        // A re-login writes a new file before the old one is swept up, so an
        // expired entry sitting beside a live one must not win.
        let entries = [
            entry(START_URL, "2026-08-26T11:00:00Z"),
            entry(START_URL, "2026-08-26T20:00:00Z"),
        ];

        assert_eq!(
            classify(&entries, &sso(), now()),
            Session::Valid {
                expires_at: at("2026-08-26T20:00:00Z")
            }
        );
    }

    #[test]
    fn the_botocore_utc_suffix_parses_as_the_same_instant() {
        assert_eq!(
            parse_expiry("2026-08-26T20:00:00UTC"),
            Some(at("2026-08-26T20:00:00Z"))
        );
    }

    #[test]
    fn an_rfc_3339_expiry_parses() {
        assert_eq!(
            parse_expiry("2026-08-26T20:00:00Z"),
            Some(at("2026-08-26T20:00:00Z"))
        );
        assert_eq!(
            parse_expiry("2026-08-26T15:00:00-05:00"),
            Some(at("2026-08-26T20:00:00Z"))
        );
    }

    #[test]
    fn an_expiry_in_no_spelling_we_know_is_not_a_timestamp() {
        assert_eq!(parse_expiry("soon"), None);
        assert_eq!(parse_expiry(""), None);
    }

    #[test]
    fn reading_a_cache_directory_keeps_the_sessions_and_skips_everything_else() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();

        std::fs::write(
            path.join("aaaa.json"),
            r#"{"startUrl":"https://acme.awsapps.com/start","region":"us-east-1",
                "accessToken":"secret","expiresAt":"2026-08-26T20:00:00Z"}"#,
        )
        .unwrap();
        // A client registration: same directory, no `startUrl`.
        std::fs::write(
            path.join("botocore-client-id-us-east-1.json"),
            r#"{"clientId":"x","clientSecret":"y","expiresAt":"2026-09-26T20:00:00Z"}"#,
        )
        .unwrap();
        // Truncated mid-write, which is what a killed `aws sso login` leaves.
        std::fs::write(path.join("broken.json"), "{\"startUrl\":").unwrap();
        // Not ours at all.
        std::fs::write(path.join("notes.txt"), "ignore me").unwrap();

        let entries = read_cache(path);

        assert_eq!(
            entries,
            vec![entry(START_URL, "2026-08-26T20:00:00Z")],
            "only the one real session should have been read"
        );
    }

    #[test]
    fn a_cache_directory_that_does_not_exist_reads_as_no_entries() {
        // The state of every machine before its first `aws sso login`.
        let directory = tempfile::tempdir().unwrap();

        assert_eq!(
            read_cache(&directory.path().join("never-created")),
            Vec::new()
        );
    }
}
