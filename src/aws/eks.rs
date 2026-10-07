//! Which EKS cluster a kubeconfig context is, as the AWS CLI needs to name
//! it, and what `aws eks describe-cluster` says about its logging.
//!
//! [`Target::of`] reads the cluster name, region, and profile out of what the
//! kubeconfig already holds — the context's ARN, the API server's hostname,
//! the `exec` block's arguments — so `eks control-plane-logs` needs no flags
//! of its own to say which cluster it means. [`logging`] reads the reply to
//! `describe-cluster`. Both are pure.

use kube::config::AuthInfo;
use serde::Deserialize;

use crate::aws::cli::Call;
use crate::aws::logs::LogType;
use crate::cluster::ClusterIdentity;
use crate::k8s::client;

/// The cluster and account an AWS CLI call is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The bare EKS cluster name, `prod`.
    pub cluster: String,
    pub region: String,
    /// The profile the context's `exec` block names, passed on as
    /// `--profile`. `None` leaves the choice to the environment, as the
    /// helper itself would: a `--profile default` added here would override
    /// `AWS_ACCESS_KEY_ID` in the shell, which the helper never does.
    pub profile: Option<String>,
    /// The `exec` block's own environment, layered over ours for every call.
    pub env: Vec<(String, String)>,
    /// The program to run. `aws` but in tests.
    pub program: String,
}

/// Why a context cannot be turned into a [`Target`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TargetError {
    #[error(
        "eks cannot tell which EKS cluster context {context:?} is: its name is not a cluster \
         ARN, and its credential helper does not name one with `--cluster-name`.\n\
         Rewrite the context with `aws eks update-kubeconfig --name <cluster> --region <region>`."
    )]
    NoCluster { context: String },

    #[error(
        "eks cannot tell which region {cluster:?} is in: context {context:?} has no cluster ARN, \
         no EKS endpoint, and no `--region` in its credential helper.\n\
         Rewrite the context with `aws eks update-kubeconfig --name {cluster} --region <region>`."
    )]
    NoRegion { context: String, cluster: String },
}

impl Target {
    /// Work out the target from a context.
    ///
    /// `context` and `cluster_entry` are the kubeconfig's context name and the
    /// cluster entry it points at; `region` is what [`crate::cluster::ClusterView`]
    /// already read out of an ARN or the endpoint.
    ///
    /// The cluster name comes from an ARN first — `aws eks update-kubeconfig`
    /// names both after it — then from the helper's `--cluster-name`, which
    /// is what `eksctl`'s contexts, named `user@cluster.region.eksctl.io`, carry.
    pub fn of(
        context: &str,
        cluster_entry: &str,
        region: Option<&str>,
        auth: &AuthInfo,
    ) -> Result<Self, TargetError> {
        let identity =
            ClusterIdentity::from_arn(context).or_else(|| ClusterIdentity::from_arn(cluster_entry));
        let env: Vec<(String, String)> = client::exec_env(auth)
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect();

        let cluster = identity
            .as_ref()
            .map(|identity| identity.name.clone())
            .or_else(|| exec_arg(auth, &["--cluster-name"]))
            // `aws-iam-authenticator token -i NAME`, the helper EKS used
            // before the AWS CLI could mint tokens itself.
            .or_else(|| exec_arg(auth, &["-i", "--cluster-id"]))
            .ok_or_else(|| TargetError::NoCluster {
                context: context.to_owned(),
            })?;

        let from_env = |wanted: &str| {
            env.iter()
                .find(|(name, value)| name == wanted && !value.is_empty())
                .map(|(_, value)| value.clone())
        };
        let region = region
            .map(ToOwned::to_owned)
            .or_else(|| exec_arg(auth, &["--region"]))
            .or_else(|| from_env("AWS_REGION"))
            .or_else(|| from_env("AWS_DEFAULT_REGION"))
            .ok_or_else(|| TargetError::NoRegion {
                context: context.to_owned(),
                cluster: cluster.clone(),
            })?;

        let profile = exec_arg(auth, &["--profile"])
            .or_else(|| from_env("AWS_PROFILE"))
            .or_else(|| from_env("AWS_DEFAULT_PROFILE"));

        Ok(Self {
            cluster,
            region,
            profile,
            env,
            program: "aws".to_owned(),
        })
    }

    /// A call to `aws <service> <operation> …`, with the region, the profile,
    /// and JSON output added.
    #[must_use]
    pub fn call(&self, words: &[&str], extra: Vec<String>, action: &'static str) -> Call {
        let mut argv = vec![self.program.clone()];
        argv.extend(words.iter().map(|word| (*word).to_owned()));
        argv.extend(extra);
        argv.extend(["--region".to_owned(), self.region.clone()]);
        if let Some(profile) = &self.profile {
            argv.extend(["--profile".to_owned(), profile.clone()]);
        }
        argv.extend(["--output".to_owned(), "json".to_owned()]);
        Call {
            argv,
            env: self.env.clone(),
            action,
        }
    }

    /// `aws eks describe-cluster`, for the logging configuration.
    #[must_use]
    pub fn describe_cluster(&self) -> Call {
        self.call(
            &["eks", "describe-cluster"],
            vec!["--name".to_owned(), self.cluster.clone()],
            "eks:DescribeCluster",
        )
    }

    /// The profile's name for messages: the one passed on, or the one the
    /// environment would pick.
    #[must_use]
    pub fn profile_name(&self, environment: &dyn Fn(&str) -> Option<String>) -> String {
        self.profile.clone().unwrap_or_else(|| {
            ["AWS_PROFILE", "AWS_DEFAULT_PROFILE"]
                .iter()
                .find_map(|name| environment(name).filter(|value| !value.is_empty()))
                .unwrap_or_else(|| "default".to_owned())
        })
    }

    /// The command that would switch `kind` on, as a line to paste.
    #[must_use]
    pub fn enable_command(&self, kind: LogType) -> String {
        let mut line = format!(
            "aws eks update-cluster-config --region {} --name {}",
            client::shell_word(&self.region),
            client::shell_word(&self.cluster),
        );
        if let Some(profile) = &self.profile {
            line.push_str(" --profile ");
            line.push_str(&client::shell_word(profile));
        }
        line.push_str(" --logging '{\"clusterLogging\":[{\"types\":[\"");
        line.push_str(kind.eks_name());
        line.push_str("\"],\"enabled\":true}]}'");
        line
    }
}

/// The value after `--flag` or in `--flag=value`, for the first of `flags`
/// the `exec` block's arguments carry.
fn exec_arg(auth: &AuthInfo, flags: &[&str]) -> Option<String> {
    let args: Vec<&String> = auth.exec.as_ref()?.args.iter().flatten().collect();
    for flag in flags {
        let mut words = args.iter();
        while let Some(word) = words.next() {
            if let Some(value) = word.strip_prefix(&format!("{flag}=")) {
                return (!value.is_empty()).then(|| value.to_owned());
            }
            if word.as_str() == *flag {
                return words
                    .next()
                    .filter(|value| !value.is_empty())
                    .map(|value| (*value).clone());
            }
        }
    }
    None
}

/// The parts of `describe-cluster`'s reply this reads.
#[derive(Debug, Deserialize)]
struct Described {
    cluster: DescribedCluster,
}

#[derive(Debug, Deserialize)]
struct DescribedCluster {
    #[serde(default)]
    logging: Option<Logging>,
}

#[derive(Debug, Deserialize)]
struct Logging {
    #[serde(rename = "clusterLogging", default)]
    cluster_logging: Vec<Setting>,
}

#[derive(Debug, Deserialize)]
struct Setting {
    #[serde(default)]
    types: Vec<String>,
    #[serde(default)]
    enabled: bool,
}

/// The log types `describe-cluster` says are switched on, in the order
/// [`LogType::ALL`] lists them.
///
/// The reply lists the enabled and the disabled types as two entries; a type
/// is on when an enabled entry names it. A type this tool does not know — one
/// EKS adds later — is ignored rather than refused.
pub fn logging(reply: &[u8]) -> Result<Vec<LogType>, serde_json::Error> {
    let described: Described = serde_json::from_slice(reply)?;
    let on: Vec<LogType> = described
        .cluster
        .logging
        .map(|logging| logging.cluster_logging)
        .unwrap_or_default()
        .into_iter()
        .filter(|setting| setting.enabled)
        .flat_map(|setting| setting.types)
        .filter_map(|name| LogType::from_eks(&name))
        .collect();
    Ok(LogType::ALL
        .into_iter()
        .filter(|kind| on.contains(kind))
        .collect())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::collections::HashMap;

    use super::*;

    const ARN: &str = "arn:aws:eks:us-east-1:111122223333:cluster/prod";

    fn auth(args: &[&str], env: &[(&str, &str)]) -> AuthInfo {
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

    fn update_kubeconfig_auth() -> AuthInfo {
        auth(
            &[
                "--region",
                "us-east-1",
                "eks",
                "get-token",
                "--cluster-name",
                "prod",
                "--output",
                "json",
                "--profile",
                "prod-admin",
            ],
            &[],
        )
    }

    #[test]
    fn a_context_named_after_its_arn_is_that_cluster() {
        let target = Target::of(ARN, ARN, Some("us-east-1"), &update_kubeconfig_auth()).unwrap();

        assert_eq!(target.cluster, "prod");
        assert_eq!(target.region, "us-east-1");
        assert_eq!(target.profile.as_deref(), Some("prod-admin"));
    }

    #[test]
    fn an_eksctl_context_is_read_from_its_helper_s_arguments() {
        // eksctl names the context `user@cluster.region.eksctl.io` and the
        // cluster entry `cluster.region.eksctl.io`; neither is an ARN.
        let auth = auth(
            &[
                "eks",
                "get-token",
                "--output",
                "json",
                "--cluster-name",
                "staging",
                "--region",
                "eu-west-1",
            ],
            &[("AWS_STS_REGIONAL_ENDPOINTS", "regional")],
        );

        let target = Target::of(
            "alice@staging.eu-west-1.eksctl.io",
            "staging.eu-west-1.eksctl.io",
            None,
            &auth,
        )
        .unwrap();

        assert_eq!(target.cluster, "staging");
        assert_eq!(target.region, "eu-west-1");
        assert_eq!(target.profile, None);
        assert_eq!(
            target.env,
            [(
                "AWS_STS_REGIONAL_ENDPOINTS".to_owned(),
                "regional".to_owned()
            )]
        );
    }

    #[test]
    fn an_aws_iam_authenticator_context_is_read_from_its_cluster_id() {
        let auth = auth(
            &["token", "-i", "legacy"],
            &[("AWS_PROFILE", "ops"), ("AWS_REGION", "ap-southeast-2")],
        );

        let target = Target::of("legacy", "legacy", None, &auth).unwrap();

        assert_eq!(target.cluster, "legacy");
        assert_eq!(target.region, "ap-southeast-2");
        assert_eq!(target.profile.as_deref(), Some("ops"));
    }

    #[test]
    fn the_endpoint_s_region_is_used_when_the_helper_names_none() {
        let auth = auth(&["eks", "get-token", "--cluster-name=prod"], &[]);

        let target = Target::of("prod", "prod", Some("us-west-2"), &auth).unwrap();

        assert_eq!(target.cluster, "prod");
        assert_eq!(target.region, "us-west-2");
    }

    #[test]
    fn a_context_with_no_cluster_name_anywhere_says_how_to_rewrite_it() {
        let error = Target::of("minikube", "minikube", None, &AuthInfo::default()).unwrap_err();

        let message = error.to_string();
        assert!(message.contains("\"minikube\""), "{message}");
        assert!(message.contains("aws eks update-kubeconfig"), "{message}");
    }

    #[test]
    fn a_context_with_no_region_anywhere_says_how_to_rewrite_it() {
        let auth = auth(&["eks", "get-token", "--cluster-name", "prod"], &[]);

        let error = Target::of("prod", "prod", None, &auth).unwrap_err();

        assert_eq!(
            error,
            TargetError::NoRegion {
                context: "prod".to_owned(),
                cluster: "prod".to_owned()
            }
        );
        assert!(error.to_string().contains("--name prod --region <region>"));
    }

    #[test]
    fn every_call_carries_the_region_the_profile_and_json_output() {
        let target = Target::of(ARN, ARN, Some("us-east-1"), &update_kubeconfig_auth()).unwrap();

        let call = target.describe_cluster();

        assert_eq!(
            call.argv,
            [
                "aws",
                "eks",
                "describe-cluster",
                "--name",
                "prod",
                "--region",
                "us-east-1",
                "--profile",
                "prod-admin",
                "--output",
                "json"
            ]
        );
        assert_eq!(call.action, "eks:DescribeCluster");
    }

    #[test]
    fn a_profile_the_context_does_not_name_is_left_to_the_environment() {
        let auth = auth(&["eks", "get-token", "--cluster-name", "prod"], &[]);
        let target = Target::of(ARN, ARN, Some("us-east-1"), &auth).unwrap();

        assert!(
            !target
                .describe_cluster()
                .argv
                .contains(&"--profile".to_owned())
        );
        assert_eq!(
            target.profile_name(&|name| (name == "AWS_PROFILE").then(|| "shell".to_owned())),
            "shell"
        );
        assert_eq!(target.profile_name(&|_| None), "default");
    }

    #[test]
    fn the_enable_command_switches_on_one_type_and_pastes_as_it_is() {
        let target = Target::of(ARN, ARN, Some("us-east-1"), &update_kubeconfig_auth()).unwrap();

        assert_eq!(
            target.enable_command(LogType::ControllerManager),
            r#"aws eks update-cluster-config --region us-east-1 --name prod --profile prod-admin --logging '{"clusterLogging":[{"types":["controllerManager"],"enabled":true}]}'"#
        );
    }

    #[test]
    fn logging_reads_the_enabled_types_and_ignores_the_disabled_ones() {
        let reply = br#"{"cluster": {"name": "prod", "logging": {"clusterLogging": [
            {"types": ["api", "audit"], "enabled": true},
            {"types": ["authenticator", "controllerManager", "scheduler"], "enabled": false}
        ]}}}"#;

        assert_eq!(logging(reply).unwrap(), [LogType::Api, LogType::Audit]);
    }

    #[test]
    fn a_cluster_with_nothing_switched_on_has_no_types() {
        let reply = br#"{"cluster": {"logging": {"clusterLogging": [
            {"types": ["api", "audit", "authenticator", "controllerManager", "scheduler"], "enabled": false}
        ]}}}"#;
        assert_eq!(logging(reply).unwrap(), []);

        // An older reply with no `logging` key at all says the same.
        assert_eq!(logging(br#"{"cluster": {"name": "prod"}}"#).unwrap(), []);
    }

    #[test]
    fn a_type_eks_adds_later_is_ignored_rather_than_refused() {
        let reply = br#"{"cluster": {"logging": {"clusterLogging": [
            {"types": ["scheduler", "somethingNew"], "enabled": true}
        ]}}}"#;
        assert_eq!(logging(reply).unwrap(), [LogType::Scheduler]);
    }

    #[test]
    fn a_reply_that_is_not_json_is_an_error() {
        assert!(logging(b"not json").is_err());
    }
}
