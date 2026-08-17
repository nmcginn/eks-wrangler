//! Turning kubeconfig entries into something a human wants to read.
//!
//! `eksctl` and `aws eks update-kubeconfig` name contexts after the cluster ARN,
//! which is precise and completely unreadable:
//!
//! ```text
//! arn:aws:eks:us-east-1:111122223333:cluster/prod-usw2
//! ```
//!
//! Nobody wants that in a status bar. This module recovers the interesting
//! pieces — cluster name, region, account — so the UI can show `prod-usw2` and
//! keep the ARN for when it is actually needed.

use crate::kubeconfig::ResolvedContext;

/// The pieces of an EKS cluster ARN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterIdentity {
    /// `aws`, `aws-cn`, or `aws-us-gov`.
    pub partition: String,
    pub region: String,
    pub account_id: String,
    /// The bare cluster name, e.g. `prod-usw2`.
    pub name: String,
}

impl ClusterIdentity {
    /// Parse an EKS cluster ARN.
    ///
    /// Returns `None` for anything that is not an `eks ... :cluster/NAME` ARN,
    /// including ARNs for other services, so callers can fall back to the raw
    /// context name rather than displaying nonsense.
    #[must_use]
    pub fn from_arn(arn: &str) -> Option<Self> {
        // arn : partition : service : region : account : cluster/NAME
        let mut parts = arn.splitn(6, ':');

        if parts.next()? != "arn" {
            return None;
        }
        let partition = parts.next()?;
        if parts.next()? != "eks" {
            return None;
        }
        let region = parts.next()?;
        let account_id = parts.next()?;
        let resource = parts.next()?;

        let name = resource.strip_prefix("cluster/")?;

        if partition.is_empty() || region.is_empty() || name.is_empty() {
            return None;
        }

        Some(Self {
            partition: partition.to_owned(),
            region: region.to_owned(),
            account_id: account_id.to_owned(),
            name: name.to_owned(),
        })
    }
}

/// How a context should be presented in the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterView {
    /// The kubeconfig context name — always the value to pass back to `eks use`.
    pub context_name: String,
    /// Short, human-facing label. The cluster name for EKS contexts.
    pub display_name: String,
    /// Region, when we could work one out.
    pub region: Option<String>,
    pub account_id: Option<String>,
    pub namespace: String,
    pub is_current: bool,
}

impl ClusterView {
    /// Build a display view from a resolved kubeconfig context.
    ///
    /// Falls back through three sources of truth: the context name as an ARN,
    /// the referenced cluster name as an ARN, then the API server hostname for
    /// the region alone.
    #[must_use]
    pub fn from_context(context: &ResolvedContext) -> Self {
        let identity = ClusterIdentity::from_arn(&context.name)
            .or_else(|| ClusterIdentity::from_arn(&context.cluster_name));

        let region = identity.as_ref().map(|i| i.region.clone()).or_else(|| {
            context
                .server
                .as_deref()
                .and_then(region_from_server_url)
                .map(str::to_owned)
        });

        Self {
            context_name: context.name.clone(),
            display_name: identity
                .as_ref()
                .map_or_else(|| context.name.clone(), |i| i.name.clone()),
            region,
            account_id: identity.as_ref().map(|i| i.account_id.clone()),
            namespace: context.effective_namespace().to_owned(),
            is_current: context.is_current,
        }
    }

    /// A one-line label such as `prod-usw2 (us-east-1)`.
    #[must_use]
    pub fn label(&self) -> String {
        match &self.region {
            Some(region) => format!("{} ({region})", self.display_name),
            None => self.display_name.clone(),
        }
    }
}

/// Extract the region from an EKS API server hostname.
///
/// EKS endpoints look like `https://ABC123.gr7.us-east-1.eks.amazonaws.com`.
#[must_use]
pub fn region_from_server_url(url: &str) -> Option<&str> {
    let host = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let host = host.split('/').next()?;

    // Walk back from the `.eks.amazonaws.com` suffix; the label immediately
    // before `eks` is the region.
    let labels: Vec<&str> = host.split('.').collect();
    let eks_index = labels.iter().position(|label| *label == "eks")?;
    let region = labels.get(eks_index.checked_sub(1)?)?;

    if region.is_empty() {
        None
    } else {
        Some(region)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn context(name: &str, cluster: &str, server: Option<&str>) -> ResolvedContext {
        ResolvedContext {
            name: name.to_owned(),
            cluster_name: cluster.to_owned(),
            user: "user".to_owned(),
            namespace: None,
            server: server.map(str::to_owned),
            is_current: false,
        }
    }

    #[test]
    fn parses_a_standard_eks_arn() {
        let identity =
            ClusterIdentity::from_arn("arn:aws:eks:us-east-1:111122223333:cluster/prod").unwrap();

        assert_eq!(identity.partition, "aws");
        assert_eq!(identity.region, "us-east-1");
        assert_eq!(identity.account_id, "111122223333");
        assert_eq!(identity.name, "prod");
    }

    #[test]
    fn parses_govcloud_and_china_partitions() {
        let gov = ClusterIdentity::from_arn("arn:aws-us-gov:eks:us-gov-west-1:1234:cluster/secure")
            .unwrap();
        assert_eq!(gov.partition, "aws-us-gov");
        assert_eq!(gov.name, "secure");

        let cn = ClusterIdentity::from_arn("arn:aws-cn:eks:cn-north-1:1234:cluster/great").unwrap();
        assert_eq!(cn.region, "cn-north-1");
    }

    #[test]
    fn keeps_slashes_inside_cluster_names() {
        // The resource section is the final field, so a stray slash must not
        // truncate the name.
        let identity =
            ClusterIdentity::from_arn("arn:aws:eks:us-east-1:1234:cluster/team/prod").unwrap();
        assert_eq!(identity.name, "team/prod");
    }

    #[test]
    fn rejects_non_eks_and_malformed_arns() {
        assert!(ClusterIdentity::from_arn("arn:aws:s3:::my-bucket").is_none());
        assert!(ClusterIdentity::from_arn("arn:aws:eks:us-east-1:1234:nodegroup/ng").is_none());
        assert!(ClusterIdentity::from_arn("arn:aws:eks:us-east-1:1234:cluster/").is_none());
        assert!(ClusterIdentity::from_arn("minikube").is_none());
        assert!(ClusterIdentity::from_arn("").is_none());
    }

    #[test]
    fn extracts_region_from_eks_endpoints() {
        assert_eq!(
            region_from_server_url("https://ABC123.gr7.us-east-1.eks.amazonaws.com"),
            Some("us-east-1")
        );
        assert_eq!(
            region_from_server_url("https://ABC123.gr7.eu-west-2.eks.amazonaws.com/"),
            Some("eu-west-2")
        );
        assert_eq!(region_from_server_url("https://127.0.0.1:6443"), None);
        assert_eq!(region_from_server_url("https://eks.amazonaws.com"), None);
    }

    #[test]
    fn view_prefers_the_arn_cluster_name_over_the_raw_context_name() {
        let view = ClusterView::from_context(&context(
            "arn:aws:eks:us-east-1:111122223333:cluster/prod-usw2",
            "arn:aws:eks:us-east-1:111122223333:cluster/prod-usw2",
            None,
        ));

        assert_eq!(view.display_name, "prod-usw2");
        assert_eq!(view.label(), "prod-usw2 (us-east-1)");
        assert_eq!(view.account_id.as_deref(), Some("111122223333"));
    }

    #[test]
    fn view_falls_back_to_the_cluster_entry_when_the_context_is_renamed() {
        // `kubectl config rename-context` leaves a friendly context name
        // pointing at an ARN-named cluster.
        let view = ClusterView::from_context(&context(
            "prod",
            "arn:aws:eks:ap-southeast-2:1234:cluster/prod",
            None,
        ));

        assert_eq!(view.display_name, "prod");
        assert_eq!(view.region.as_deref(), Some("ap-southeast-2"));
    }

    #[test]
    fn view_falls_back_to_the_server_url_for_region() {
        let view = ClusterView::from_context(&context(
            "hand-written",
            "hand-written",
            Some("https://ABC.gr7.us-west-1.eks.amazonaws.com"),
        ));

        assert_eq!(view.display_name, "hand-written");
        assert_eq!(view.region.as_deref(), Some("us-west-1"));
        assert_eq!(view.account_id, None);
    }

    #[test]
    fn view_of_a_non_eks_cluster_is_still_usable() {
        let view = ClusterView::from_context(&context("minikube", "minikube", None));

        assert_eq!(view.display_name, "minikube");
        assert_eq!(view.label(), "minikube");
        assert_eq!(view.namespace, "default");
    }
}
