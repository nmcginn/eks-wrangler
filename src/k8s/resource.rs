//! Resource names: which of them Kubernetes ships with, and which a cluster
//! added.
//!
//! A node's `capacity` and `allocatable` maps are keyed by resource name, and
//! the tool treats two kinds of key very differently. `cpu` and `memory` have a
//! column each because every node has them. `nvidia.com/gpu` has a column only
//! where a device plugin advertised it, because a cluster with no GPUs should
//! not grow a column of dashes — the same `any`-not-`all` rule the usage
//! columns follow.
//!
//! Telling the two apart is a naming rule rather than a list, which is the
//! point: the whole reason extended resources exist is that a cluster can
//! invent one, so a hard-coded set of vendors would be wrong the first time
//! somebody advertised `smarter-devices/fuse` or a licence count.

/// Whether a resource name is an *extended* resource — one a device plugin or
/// an administrator added, rather than one Kubernetes itself defines.
///
/// Kubernetes reserves the `kubernetes.io` domain for its own resource names
/// and requires extended ones to be fully qualified outside it, so the rule is
/// exactly "has a domain, and the domain is not Kubernetes' own". That admits
/// `nvidia.com/gpu`, `amd.com/gpu`, `hugepages.example.com/thing`, and any
/// opaque integer resource somebody patched onto a node.
///
/// It deliberately excludes the unqualified names, all of which Kubernetes
/// defines and none of which is a device: `cpu`, `memory`, `pods`,
/// `ephemeral-storage`, `hugepages-2Mi`, and the `attachable-volumes-*` limits
/// a CSI driver reports. Those either have a column already or want a column of
/// their own with a heading a person recognises, which is a different task.
#[must_use]
pub fn is_extended(name: &str) -> bool {
    let Some((domain, remainder)) = name.split_once('/') else {
        return false;
    };
    if remainder.is_empty() {
        return false;
    }

    domain != KUBERNETES_DOMAIN && !domain.ends_with(KUBERNETES_SUBDOMAIN)
}

/// The domain Kubernetes reserves for the resources it defines itself.
const KUBERNETES_DOMAIN: &str = "kubernetes.io";

/// The same domain as a suffix, so a subdomain of it is caught too. A constant
/// rather than a `format!` on every key of every node's capacity map.
const KUBERNETES_SUBDOMAIN: &str = ".kubernetes.io";

/// A resource name as a table heading.
///
/// Upper-cased and otherwise left alone, so `nvidia.com/gpu` becomes
/// `NVIDIA.COM/GPU` and sits beside `CPU` and `MEMORY` without looking like a
/// different kind of thing. The domain stays: `amd.com/gpu` and
/// `nvidia.com/gpu` are two different resources on the mixed-vendor node that
/// makes this column worth having, and a heading of `GPU` over one of them
/// would be a lie on that node and ambiguous on every other.
#[must_use]
pub fn heading(name: &str) -> String {
    name.to_uppercase()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_device_plugin_resource_is_extended() {
        assert!(is_extended("nvidia.com/gpu"));
        assert!(is_extended("amd.com/gpu"));
        assert!(is_extended("smarter-devices/fuse"));
        assert!(is_extended("example.com/dongle"));
    }

    #[test]
    fn the_resources_kubernetes_defines_itself_are_not_extended() {
        // Each of these turns up in a real node's capacity map, and none of
        // them belongs in a column headed by a vendor domain.
        for name in [
            "cpu",
            "memory",
            "pods",
            "ephemeral-storage",
            "hugepages-2Mi",
            "hugepages-1Gi",
            "attachable-volumes-aws-ebs",
        ] {
            assert!(!is_extended(name), "{name}");
        }
    }

    #[test]
    fn the_kubernetes_domain_itself_is_never_extended() {
        // Reserved by Kubernetes, so anything under it is a resource the tool
        // should learn about deliberately rather than render as a device.
        assert!(!is_extended("kubernetes.io/something"));
        assert!(!is_extended("storage.kubernetes.io/thing"));
        // A domain that merely *ends* in the same letters is somebody else's.
        assert!(is_extended("notkubernetes.io/gpu"));
    }

    #[test]
    fn a_name_that_is_not_qualified_at_all_is_not_extended() {
        assert!(!is_extended(""));
        assert!(!is_extended("/"));
        assert!(!is_extended("gpu/"));
        // A leading slash leaves an empty domain, which is not Kubernetes', so
        // the name qualifies — nonsense in, nonsense out, but never a panic.
        assert!(is_extended("/gpu"));
    }

    #[test]
    fn a_heading_is_the_resource_name_shouted() {
        assert_eq!(heading("nvidia.com/gpu"), "NVIDIA.COM/GPU");
        assert_eq!(heading("smarter-devices/fuse"), "SMARTER-DEVICES/FUSE");
        assert_eq!(heading(""), "");
    }
}
