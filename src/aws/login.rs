//! Running `aws sso login`.
//!
//! The login itself is the AWS CLI's, not ours. That is the whole design: the
//! CLI is already a hard requirement of every EKS context this tool opens — it
//! is what `exec` runs to get a token — and it already knows how to do the
//! device-authorisation dance, open a browser, and write the token cache in the
//! format `aws eks get-token` reads back. Reimplementing that over
//! `aws-sdk-ssooidc` would add a second HTTP stack to a binary whose startup
//! time is a feature, and would leave us writing a cache format that is
//! `botocore`'s to change.
//!
//! [`command`] is pure, so the argv is asserted rather than observed;
//! [`run`] is the three lines of process handling underneath it.

use std::io::IsTerminal;
use std::process::{Command, Stdio};

/// Failures from running the login.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "could not start `{line}`: {message}\n\
         The AWS CLI is what logs you in to IAM Identity Center, and it is not on your PATH. \
         Install it, or pass `--login never` to be told to log in by hand instead."
    )]
    CouldNotStart { line: String, message: String },

    #[error(
        "`{line}` exited without logging you in{status}.\n\
         Its own output above says why. Run it again by hand if you need to see it a second time."
    )]
    Failed { line: String, status: String },
}

/// The command that logs a profile in, spelled the way it would be typed.
///
/// Always the `--profile` form, never `--sso-session`: `aws sso login
/// --profile X` works for both spellings of an Identity Center profile in
/// `~/.aws/config` and on every AWS CLI v2 that has the subcommand at all,
/// where `--sso-session` needs a recent one. One form is also one thing to
/// print in the message that offers it.
#[must_use]
pub fn command(profile: &str) -> Vec<String> {
    vec![
        "aws".to_owned(),
        "sso".to_owned(),
        "login".to_owned(),
        "--profile".to_owned(),
        profile.to_owned(),
    ]
}

/// The command written as one pasteable line, for the messages about it.
#[must_use]
pub fn line(argv: &[String]) -> String {
    argv.join(" ")
}

/// Run a login to completion, giving it this terminal while it waits.
///
/// **Deliberately not under `--timeout`.** That budget is about a cluster that
/// will not answer; this is a human at a browser, and the whole point of the
/// step is to wait for them. Bounding it would recreate the hang-versus-give-up
/// problem the budget was written to solve, on the one path where waiting is
/// the correct behaviour.
///
/// `env` is the exec block's own environment, passed through so the login reads
/// the same `~/.aws/config` the token fetch will — a context that sets
/// `AWS_CONFIG_FILE` must not log in against a different file from the one it
/// then reads the token for.
pub fn run(argv: &[String], env: &[(String, String)]) -> Result<(), Error> {
    let line = line(argv);
    let Some((program, rest)) = argv.split_first() else {
        // `command` never builds one, so this is unreachable in practice and
        // worded rather than `unwrap`ped, as everything in this crate is.
        return Err(Error::CouldNotStart {
            line,
            message: "there is no command to run".to_owned(),
        });
    };

    let mut child = Command::new(program);
    child
        .args(rest)
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit());
    for (name, value) in env {
        child.env(name, value);
    }

    // The device code and the "open this URL" line go to the child's stdout.
    // When ours is a terminal that is exactly where the user is looking, so it
    // is inherited. When it is a pipe — `eks nodes --login always | less` — it
    // must not be, or the login's chatter lands in the middle of the table.
    // Sending it to stderr instead keeps stdout byte-identical and still puts
    // the URL in front of whoever is watching.
    let piped = !std::io::stdout().is_terminal();
    child.stdout(if piped {
        Stdio::piped()
    } else {
        Stdio::inherit()
    });

    let mut spawned = child.spawn().map_err(|error| Error::CouldNotStart {
        line: line.clone(),
        message: error.to_string(),
    })?;

    // Copied on this thread rather than after the wait, so the URL appears
    // while it still matters rather than once the login is over.
    if let Some(mut out) = spawned.stdout.take() {
        let _ = std::io::copy(&mut out, &mut std::io::stderr());
    }

    let status = spawned.wait().map_err(|error| Error::CouldNotStart {
        line: line.clone(),
        message: error.to_string(),
    })?;

    if status.success() {
        return Ok(());
    }

    Err(Error::Failed {
        line,
        // A process killed by a signal has no code, and `" (exit code 1)"`
        // reads better than a bare number when there is one.
        status: match status.code() {
            Some(code) => format!(" (exit code {code})"),
            None => String::new(),
        },
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn the_login_command_names_the_profile() {
        assert_eq!(
            command("prod"),
            vec!["aws", "sso", "login", "--profile", "prod"]
        );
    }

    #[test]
    fn the_command_is_written_as_a_line_somebody_could_paste() {
        assert_eq!(line(&command("prod")), "aws sso login --profile prod");
    }

    #[test]
    fn a_command_that_cannot_be_started_says_to_install_the_aws_cli() {
        // The same failure `k8s::client` calls `HelperMissing`, met one step
        // earlier: there is no point telling somebody to log in with a tool
        // they do not have.
        let argv = vec![
            "eks-test-no-such-aws-cli".to_owned(),
            "sso".to_owned(),
            "login".to_owned(),
        ];

        let error = run(&argv, &[]).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("eks-test-no-such-aws-cli"), "{message}");
        assert!(message.contains("not on your PATH"), "{message}");
        assert!(message.contains("--login never"), "{message}");
    }

    #[test]
    fn an_empty_command_is_worded_rather_than_panicked_on() {
        let error = run(&[], &[]).unwrap_err();

        assert!(error.to_string().contains("no command to run"));
    }
}
