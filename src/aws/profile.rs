//! Which AWS profile a kubeconfig context authenticates as.
//!
//! An EKS context runs `aws eks get-token`, and which profile that command
//! reads is decided by four things in a fixed order. Working it out here rather
//! than asking the AWS CLI is what keeps the whole credential check off the
//! network and out of a subprocess — and it has to agree with what the helper
//! itself would conclude, or `eks` would offer to log in to the wrong account.
//!
//! [`profile_for`] is pure over the `exec` block and a lookup closure, so the
//! precedence between the four sources is a table of fixtures rather than a
//! test that has to mutate the process environment.

use kube::config::AuthInfo;

use crate::k8s::client;

/// The profile name the AWS CLI would use, when it is `aws` that this context
/// runs.
///
/// The order is the AWS CLI's own, and each step exists because somebody
/// configures it that way:
///
/// 1. `--profile X` in the exec block's arguments — what `aws eks
///    update-kubeconfig --profile` writes.
/// 2. `AWS_PROFILE` (then the legacy `AWS_DEFAULT_PROFILE`) in the exec block's
///    `env:` list — what `--user-alias` setups and hand-edited kubeconfigs use.
///    These beat the process environment because `kube` layers them on top of
///    it, so they are what the child actually sees.
/// 3. The same two names in the process environment, for a context that leaves
///    the choice to the shell.
/// 4. `default`.
///
/// `env` is the process-environment lookup, taken as a closure so no test here
/// has to set a real variable.
#[must_use]
pub fn profile_for(auth: &AuthInfo, env: &dyn Fn(&str) -> Option<String>) -> String {
    if let Some(name) = from_args(auth) {
        return name;
    }

    // The exec block's own environment, read through the same walk
    // `client::helper_command` prints from, so the profile we log in to is the
    // profile the line in that message would have used.
    for wanted in ["AWS_PROFILE", "AWS_DEFAULT_PROFILE"] {
        if let Some((_, value)) = client::exec_env(auth).find(|(name, _)| *name == wanted)
            && !value.is_empty()
        {
            return value.to_owned();
        }
    }

    for wanted in ["AWS_PROFILE", "AWS_DEFAULT_PROFILE"] {
        if let Some(value) = env(wanted)
            && !value.is_empty()
        {
            return value;
        }
    }

    "default".to_owned()
}

/// `--profile X` or `--profile=X` from the exec block's arguments.
fn from_args(auth: &AuthInfo) -> Option<String> {
    let exec = auth.exec.as_ref()?;
    let mut args = exec.args.iter().flatten();

    while let Some(argument) = args.next() {
        if let Some(value) = argument.strip_prefix("--profile=") {
            return (!value.is_empty()).then(|| value.to_owned());
        }
        if argument == "--profile" {
            let value = args.next()?;
            return (!value.is_empty()).then(|| value.clone());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::collections::HashMap;

    use super::*;

    fn exec(args: &[&str], env: &[(&str, &str)]) -> AuthInfo {
        AuthInfo {
            exec: Some(kube::config::ExecConfig {
                command: Some("aws".to_owned()),
                args: Some(args.iter().map(|a| (*a).to_owned()).collect()),
                env: Some(
                    env.iter()
                        .map(|(name, value)| {
                            HashMap::from([
                                ("name".to_owned(), (*name).to_owned()),
                                ("value".to_owned(), (*value).to_owned()),
                            ])
                        })
                        .collect(),
                ),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// A process environment that holds nothing.
    fn empty(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn a_profile_flag_in_the_exec_block_wins() {
        let auth = exec(
            &[
                "eks",
                "get-token",
                "--cluster-name",
                "prod",
                "--profile",
                "team",
            ],
            &[("AWS_PROFILE", "shell")],
        );

        assert_eq!(
            profile_for(&auth, &|name| (name == "AWS_PROFILE")
                .then(|| "process".to_owned())),
            "team"
        );
    }

    #[test]
    fn the_joined_spelling_of_the_flag_is_read_too() {
        let auth = exec(&["eks", "get-token", "--profile=team"], &[]);

        assert_eq!(profile_for(&auth, &empty), "team");
    }

    #[test]
    fn the_exec_blocks_environment_beats_the_shells() {
        // `kube` layers the exec block's `env:` over the inherited environment,
        // so this is the one the child actually reads.
        let auth = exec(&["eks", "get-token"], &[("AWS_PROFILE", "kubeconfig")]);

        assert_eq!(
            profile_for(&auth, &|_| Some("process".to_owned())),
            "kubeconfig"
        );
    }

    #[test]
    fn the_process_environment_is_used_when_the_context_leaves_the_choice_open() {
        let auth = exec(&["eks", "get-token"], &[]);

        assert_eq!(
            profile_for(&auth, &|name| (name == "AWS_PROFILE")
                .then(|| "process".to_owned())),
            "process"
        );
    }

    #[test]
    fn aws_profile_beats_the_legacy_alias_beside_it() {
        let auth = exec(&["eks", "get-token"], &[]);

        assert_eq!(
            profile_for(&auth, &|name| match name {
                "AWS_PROFILE" => Some("current".to_owned()),
                "AWS_DEFAULT_PROFILE" => Some("legacy".to_owned()),
                _ => None,
            }),
            "current"
        );
    }

    #[test]
    fn the_legacy_alias_is_still_read_on_its_own() {
        let auth = exec(&["eks", "get-token"], &[]);

        assert_eq!(
            profile_for(&auth, &|name| (name == "AWS_DEFAULT_PROFILE")
                .then(|| "legacy".to_owned())),
            "legacy"
        );
    }

    #[test]
    fn a_context_that_names_no_profile_anywhere_uses_default() {
        let auth = exec(&["eks", "get-token", "--cluster-name", "prod"], &[]);

        assert_eq!(profile_for(&auth, &empty), "default");
    }

    #[test]
    fn a_context_with_no_exec_block_at_all_uses_default() {
        // A bare token or a client certificate: no helper, and nothing here to
        // read. The caller finds out it is not an SSO profile a step later.
        assert_eq!(profile_for(&AuthInfo::default(), &empty), "default");
    }

    #[test]
    fn an_empty_profile_value_is_not_a_profile_name() {
        // `AWS_PROFILE=` in a shell means "unset", not a profile called "".
        let auth = exec(&["eks", "get-token"], &[("AWS_PROFILE", "")]);

        assert_eq!(profile_for(&auth, &|_| Some(String::new())), "default");
    }

    #[test]
    fn a_trailing_profile_flag_with_nothing_after_it_names_no_profile() {
        // A malformed exec block. Falling through to `default` beats treating
        // the missing word as a profile name.
        let auth = exec(&["eks", "get-token", "--profile"], &[]);

        assert_eq!(profile_for(&auth, &empty), "default");
    }
}
