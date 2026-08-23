//! One pod's containers, reduced to rows for the dashboard's drill-down.
//!
//! [`super::row`] derives a single `STATUS` for the whole pod, picking one
//! container's story to tell on the cluster's behalf when several disagree.
//! That is the right answer for a listing of many pods, and the wrong one
//! once a reader has drilled into *this* pod specifically and wants to know
//! what every one of its containers is doing — this module answers that
//! question instead, one row per container, nothing chosen on their behalf.

use k8s_openapi::api::core::v1::{Container, ContainerStatus, Pod};

use crate::theme::Severity;

use super::row::exit_reason;

/// Shown wherever the API server has not resolved an image yet, as elsewhere
/// in the tool.
const UNKNOWN: &str = "-";

/// One container — app or init — as a row in the pod-containers pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerRow {
    pub name: String,
    /// The image this container is running, or will run. Read from the
    /// container's own status once the kubelet has resolved one — which can
    /// differ from the manifest's tag, if the runtime rewrote it — falling
    /// back to the pod spec for a container nothing has reported on yet.
    pub image: String,
    /// Whether this is an init container. Init containers are listed first,
    /// spec order preserved within each group — the same grouping `kubectl
    /// describe pod` uses.
    pub init: bool,
    pub ready: bool,
    pub restarts: i32,
    /// A short sentence: `Running`, `Waiting: CrashLoopBackOff`, `Terminated:
    /// OOMKilled (137)`. Unlike [`super::PodRow::status`] this describes one
    /// container rather than a verdict for the whole pod, so it never has to
    /// choose which of several unhappy containers to report — every row
    /// speaks for itself.
    pub state: String,
    pub severity: Severity,
}

impl ContainerRow {
    /// Every container a pod declares: init containers first in spec order,
    /// then app containers, each paired with its status if the kubelet has
    /// reported one yet.
    ///
    /// A pod with no spec at all — one this tool should never actually be
    /// asked to show, since it came from a `get` on a name the cluster just
    /// listed — has no containers rather than a placeholder row.
    #[must_use]
    pub fn from_pod(pod: &Pod) -> Vec<Self> {
        let Some(spec) = pod.spec.as_ref() else {
            return Vec::new();
        };
        let status = pod.status.as_ref();

        let init_statuses = status
            .and_then(|status| status.init_container_statuses.as_deref())
            .unwrap_or_default();
        let app_statuses = status
            .and_then(|status| status.container_statuses.as_deref())
            .unwrap_or_default();

        let init =
            spec.init_containers.iter().flatten().map(|container| {
                Self::build(container, find(init_statuses, &container.name), true)
            });
        let app = spec
            .containers
            .iter()
            .map(|container| Self::build(container, find(app_statuses, &container.name), false));

        init.chain(app).collect()
    }

    fn build(spec: &Container, status: Option<&ContainerStatus>, init: bool) -> Self {
        let Some(status) = status else {
            // The kubelet has not reported on this container yet — a pod
            // still `Pending`, or an init container whose turn has not come.
            // `Waiting`, not `Unknown`: it is going to run, only not yet.
            return Self {
                name: spec.name.clone(),
                image: image(spec.image.as_deref(), ""),
                init,
                ready: false,
                restarts: 0,
                state: "Waiting".to_owned(),
                severity: Severity::Warn,
            };
        };

        let (state, severity) = state_text(status);
        Self {
            name: status.name.clone(),
            image: image(spec.image.as_deref(), &status.image),
            init,
            ready: status.ready,
            restarts: status.restart_count,
            state,
            severity,
        }
    }
}

/// The status the API server reported for one named container, if any.
fn find<'a>(statuses: &'a [ContainerStatus], name: &str) -> Option<&'a ContainerStatus> {
    statuses.iter().find(|status| status.name == name)
}

/// Which image to show: the status's own resolved one, falling back to what
/// the spec asked for when the kubelet has not filled the status in.
fn image(spec_image: Option<&str>, status_image: &str) -> String {
    if !status_image.is_empty() {
        return status_image.to_owned();
    }
    spec_image
        .filter(|image| !image.is_empty())
        .map_or_else(|| UNKNOWN.to_owned(), str::to_owned)
}

/// What one container's current state reads as, and how alarming it is.
///
/// Deliberately not [`super::row::severity`]'s rule: that one judges a *pod*,
/// where `Running` only reads calm once every container is ready. Here each
/// container is its own row, so its own readiness is the only thing that
/// bears on its own colour.
fn state_text(status: &ContainerStatus) -> (String, Severity) {
    let Some(state) = status.state.as_ref() else {
        return ("Unknown".to_owned(), Severity::Unknown);
    };

    if state.running.is_some() {
        return (
            "Running".to_owned(),
            if status.ready {
                Severity::Ok
            } else {
                Severity::Warn
            },
        );
    }

    if let Some(waiting) = &state.waiting {
        let reason = waiting
            .reason
            .as_deref()
            .filter(|reason| !reason.is_empty())
            .unwrap_or("Waiting");
        // Progress reads calmer than a container stuck for a reason: the
        // kubelet says these two on the way up, and `kubectl` does not
        // colour either as a problem.
        let severity = if matches!(reason, "ContainerCreating" | "PodInitializing") {
            Severity::Warn
        } else {
            Severity::Critical
        };
        return (format!("Waiting: {reason}"), severity);
    }

    if let Some(terminated) = &state.terminated {
        let severity = if terminated.exit_code == 0 {
            Severity::Ok
        } else {
            Severity::Critical
        };
        return (
            format!(
                "Terminated: {} ({})",
                exit_reason(terminated),
                terminated.exit_code
            ),
            severity,
        );
    }

    ("Unknown".to_owned(), Severity::Unknown)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use k8s_openapi::api::core::v1::{
        ContainerState, ContainerStateRunning, ContainerStateTerminated, ContainerStateWaiting,
        PodSpec, PodStatus,
    };

    use super::*;

    fn spec_container(name: &str, image: &str) -> Container {
        Container {
            name: name.to_owned(),
            image: Some(image.to_owned()),
            ..Default::default()
        }
    }

    fn running() -> ContainerState {
        ContainerState {
            running: Some(ContainerStateRunning::default()),
            ..Default::default()
        }
    }

    fn waiting(reason: &str) -> ContainerState {
        ContainerState {
            waiting: Some(ContainerStateWaiting {
                reason: Some(reason.to_owned()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn terminated(reason: Option<&str>, exit_code: i32) -> ContainerState {
        ContainerState {
            terminated: Some(ContainerStateTerminated {
                reason: reason.map(str::to_owned),
                exit_code,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn status(
        name: &str,
        image: &str,
        state: ContainerState,
        ready: bool,
        restarts: i32,
    ) -> ContainerStatus {
        ContainerStatus {
            name: name.to_owned(),
            image: image.to_owned(),
            ready,
            restart_count: restarts,
            state: Some(state),
            ..Default::default()
        }
    }

    fn pod(spec: PodSpec, status: Option<PodStatus>) -> Pod {
        Pod {
            spec: Some(spec),
            status,
            ..Default::default()
        }
    }

    #[test]
    fn a_running_and_ready_container_reads_as_running_and_ok() {
        let pod = pod(
            PodSpec {
                containers: vec![spec_container("app", "app:1.0")],
                ..Default::default()
            },
            Some(PodStatus {
                container_statuses: Some(vec![status("app", "app:1.0", running(), true, 0)]),
                ..Default::default()
            }),
        );

        let rows = ContainerRow::from_pod(&pod);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "app");
        assert_eq!(rows[0].image, "app:1.0");
        assert!(!rows[0].init);
        assert!(rows[0].ready);
        assert_eq!(rows[0].state, "Running");
        assert_eq!(rows[0].severity, Severity::Ok);
    }

    #[test]
    fn a_running_but_not_ready_container_is_a_warning_not_ok() {
        // `kubectl`'s own case for a readiness probe still failing: the
        // process is up, the pod is not calling it ready.
        let pod = pod(
            PodSpec {
                containers: vec![spec_container("app", "app:1.0")],
                ..Default::default()
            },
            Some(PodStatus {
                container_statuses: Some(vec![status("app", "app:1.0", running(), false, 0)]),
                ..Default::default()
            }),
        );

        let rows = ContainerRow::from_pod(&pod);
        assert_eq!(rows[0].state, "Running");
        assert_eq!(rows[0].severity, Severity::Warn);
        assert!(!rows[0].ready);
    }

    #[test]
    fn a_crashlooping_container_names_the_reason_and_reads_as_critical() {
        let pod = pod(
            PodSpec {
                containers: vec![spec_container("app", "app:1.0")],
                ..Default::default()
            },
            Some(PodStatus {
                container_statuses: Some(vec![status(
                    "app",
                    "app:1.0",
                    waiting("CrashLoopBackOff"),
                    false,
                    9,
                )]),
                ..Default::default()
            }),
        );

        let rows = ContainerRow::from_pod(&pod);
        assert_eq!(rows[0].state, "Waiting: CrashLoopBackOff");
        assert_eq!(rows[0].restarts, 9);
        assert_eq!(rows[0].severity, Severity::Critical);
    }

    #[test]
    fn container_creating_is_progress_not_a_problem() {
        let pod = pod(
            PodSpec {
                containers: vec![spec_container("app", "app:1.0")],
                ..Default::default()
            },
            Some(PodStatus {
                container_statuses: Some(vec![status(
                    "app",
                    "",
                    waiting("ContainerCreating"),
                    false,
                    0,
                )]),
                ..Default::default()
            }),
        );

        let rows = ContainerRow::from_pod(&pod);
        assert_eq!(rows[0].state, "Waiting: ContainerCreating");
        assert_eq!(rows[0].severity, Severity::Warn);
        // No image resolved yet; falls back to the spec's own.
        assert_eq!(rows[0].image, "app:1.0");
    }

    #[test]
    fn a_container_that_exited_cleanly_reads_as_ok() {
        let pod = pod(
            PodSpec {
                containers: vec![spec_container("job", "job:1.0")],
                ..Default::default()
            },
            Some(PodStatus {
                container_statuses: Some(vec![status(
                    "job",
                    "job:1.0",
                    terminated(Some("Completed"), 0),
                    false,
                    0,
                )]),
                ..Default::default()
            }),
        );

        let rows = ContainerRow::from_pod(&pod);
        assert_eq!(rows[0].state, "Terminated: Completed (0)");
        assert_eq!(rows[0].severity, Severity::Ok);
    }

    #[test]
    fn a_container_killed_for_using_too_much_memory_names_the_reason() {
        let pod = pod(
            PodSpec {
                containers: vec![spec_container("app", "app:1.0")],
                ..Default::default()
            },
            Some(PodStatus {
                container_statuses: Some(vec![status(
                    "app",
                    "app:1.0",
                    terminated(Some("OOMKilled"), 137),
                    false,
                    2,
                )]),
                ..Default::default()
            }),
        );

        let rows = ContainerRow::from_pod(&pod);
        assert_eq!(rows[0].state, "Terminated: OOMKilled (137)");
        assert_eq!(rows[0].severity, Severity::Critical);
    }

    #[test]
    fn a_container_with_no_status_yet_reads_as_waiting_from_the_specs_image() {
        // A pod still `Pending`: nothing has been reported on this container
        // at all, which is not the same as an error.
        let pod = pod(
            PodSpec {
                containers: vec![spec_container("app", "app:1.0")],
                ..Default::default()
            },
            Some(PodStatus::default()),
        );

        let rows = ContainerRow::from_pod(&pod);
        assert_eq!(rows[0].state, "Waiting");
        assert_eq!(rows[0].severity, Severity::Warn);
        assert_eq!(rows[0].image, "app:1.0");
        assert!(!rows[0].ready);
        assert_eq!(rows[0].restarts, 0);
    }

    #[test]
    fn a_pod_with_no_status_at_all_still_lists_its_containers() {
        let pod = pod(
            PodSpec {
                containers: vec![spec_container("app", "app:1.0")],
                ..Default::default()
            },
            None,
        );

        let rows = ContainerRow::from_pod(&pod);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, "Waiting");
    }

    #[test]
    fn a_pod_with_no_spec_at_all_has_no_containers() {
        assert!(ContainerRow::from_pod(&Pod::default()).is_empty());
    }

    #[test]
    fn init_containers_come_first_in_spec_order_and_are_marked() {
        let pod = pod(
            PodSpec {
                init_containers: Some(vec![spec_container("migrate", "migrate:1.0")]),
                containers: vec![spec_container("app", "app:1.0")],
                ..Default::default()
            },
            Some(PodStatus {
                init_container_statuses: Some(vec![status(
                    "migrate",
                    "migrate:1.0",
                    terminated(Some("Completed"), 0),
                    false,
                    0,
                )]),
                container_statuses: Some(vec![status("app", "app:1.0", running(), true, 0)]),
                ..Default::default()
            }),
        );

        let rows = ContainerRow::from_pod(&pod);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "migrate");
        assert!(rows[0].init);
        assert_eq!(rows[1].name, "app");
        assert!(!rows[1].init);
    }

    #[test]
    fn an_unknown_state_reads_as_unknown_rather_than_a_guess() {
        let pod = pod(
            PodSpec {
                containers: vec![spec_container("app", "app:1.0")],
                ..Default::default()
            },
            Some(PodStatus {
                container_statuses: Some(vec![ContainerStatus {
                    name: "app".to_owned(),
                    image: "app:1.0".to_owned(),
                    state: None,
                    ..Default::default()
                }]),
                ..Default::default()
            }),
        );

        let rows = ContainerRow::from_pod(&pod);
        assert_eq!(rows[0].state, "Unknown");
        assert_eq!(rows[0].severity, Severity::Unknown);
    }

    #[test]
    fn the_resolved_image_is_preferred_over_the_manifests_own() {
        // The runtime may have rewritten the tag; the status's own image is
        // what is actually running.
        let pod = pod(
            PodSpec {
                containers: vec![spec_container("app", "app:latest")],
                ..Default::default()
            },
            Some(PodStatus {
                container_statuses: Some(vec![status(
                    "app",
                    "app@sha256:abcd1234",
                    running(),
                    true,
                    0,
                )]),
                ..Default::default()
            }),
        );

        assert_eq!(ContainerRow::from_pod(&pod)[0].image, "app@sha256:abcd1234");
    }
}
