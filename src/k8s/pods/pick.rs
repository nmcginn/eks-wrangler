//! Turning what somebody typed into one pod and one container.
//!
//! `kubectl exec` wants the pod's full generated name, `api-7d9f8c6b5-xk2pq`,
//! which nobody remembers and everybody copies out of a listing first. `eks`
//! takes any unique prefix of it instead, so `api` is enough while there is
//! only one `api` pod, and says which pods it could mean when there are more.
//!
//! Every function here is pure over pods that have already been fetched, so
//! each rule is a fixture: an exact name that is also a prefix of a longer
//! one, a prefix shared by two pods, a pod with three containers and no
//! default. The wording lives beside the rules for the same reason the table
//! renderers live beside their rows — the sentence and the decision change
//! together. `eks logs` will resolve its pod and container by these same
//! rules, so they are kept apart from `eks exec`'s own session handling.

use k8s_openapi::api::core::v1::{ContainerStatus, Pod};
use k8s_openapi::jiff::Timestamp;

use crate::format::{self, Cell};
use crate::k8s::pods::PodRow;
use crate::theme::Palette;

/// The annotation `kubectl` reads to choose a container when none is named.
///
/// Set by a pod's author precisely so tools land in the application rather
/// than in a sidecar — `kubectl exec`, `kubectl logs`, and `kubectl attach`
/// all honour it, and a pod that sets it is saying which container is "the"
/// container.
pub const DEFAULT_CONTAINER: &str = "kubectl.kubernetes.io/default-container";

/// How a typed name matched the pods it was compared against.
#[derive(Debug, PartialEq)]
pub enum Match<'a> {
    /// One pod is called exactly this, or is the only one whose name starts
    /// with it.
    One(&'a Pod),
    /// Several pods start with it and none is called exactly it. In listing
    /// order, which is the API server's — alphabetical within a namespace.
    Several(Vec<&'a Pod>),
    /// No pod starts with it.
    None,
}

/// Find the pod `wanted` names, as a full name or a unique prefix.
///
/// An exact name always wins, even when it is also the start of a longer
/// one: with `api` and `api-canary` both running, `api` means `api` — there
/// would otherwise be no way to type the shorter one's name at all.
#[must_use]
pub fn find<'a>(pods: &'a [Pod], wanted: &str) -> Match<'a> {
    if let Some(exact) = pods.iter().find(|pod| name(pod) == wanted) {
        return Match::One(exact);
    }

    // An empty prefix starts every name; it is not a choice of pod. Clap
    // will not hand one over for a required argument, but `""` is a valid
    // string all the same and must not quietly pick the only pod there is.
    if wanted.is_empty() {
        return Match::None;
    }

    let mut starting: Vec<&Pod> = pods
        .iter()
        .filter(|pod| name(pod).starts_with(wanted))
        .collect();
    match starting.len() {
        0 => Match::None,
        1 => starting.pop().map_or(Match::None, Match::One),
        _ => Match::Several(starting),
    }
}

/// What to print when a prefix matches more than one pod: each candidate with
/// the facts that tell them apart, and the instruction to type more.
///
/// The namespace is listed even though every candidate shares it today, so
/// the same table can serve a cluster-wide search later without a second
/// layout, and so the reader can see which namespace was searched without
/// scrolling back to the command they typed.
#[must_use]
pub fn ambiguous(wanted: &str, candidates: &[&Pod], now: Timestamp) -> String {
    let table = candidate_table(candidates, now);
    format!(
        "{count} pods start with {wanted:?}:\n\n{table}\n\n\
         Type more of the name to pick one, e.g. `{example}`.",
        count = candidates.len(),
        example = candidates
            .first()
            .map(|pod| name(pod).to_owned())
            .unwrap_or_default(),
    )
}

/// What to print when no pod in `namespace` starts with `wanted`.
///
/// `elsewhere` is what a cluster-wide search for the same prefix turned up,
/// when the caller could make one. A person who rarely uses a cluster does
/// not know which namespace their pod lives in, so an empty answer here that
/// stops at "not found" leaves them guessing; naming the namespace it *is* in
/// turns the dead end into the command to type next.
#[must_use]
pub fn not_found(wanted: &str, namespace: &str, elsewhere: &[&Pod], now: Timestamp) -> String {
    let head = format!("no pod in namespace {namespace} is called {wanted:?} or starts with it.");
    match elsewhere {
        [] => format!(
            "{head}\nRun `eks pods -n {namespace}` to see what is there, or `eks pods -A` to search every namespace."
        ),
        [only] => {
            let namespace = only.metadata.namespace.as_deref().unwrap_or_default();
            format!(
                "{head}\nThere is one in namespace {namespace}: pass `-n {namespace}` to use it."
            )
        }
        several => format!(
            "{head}\nOther namespaces have pods that start with it:\n\n{table}\n\n\
             Pass `-n <namespace>` with the one you meant.",
            table = candidate_table(several, now),
        ),
    }
}

/// The candidates as a small table: what a person needs to choose between
/// them, and no more.
fn candidate_table(candidates: &[&Pod], now: Timestamp) -> String {
    let rows: Vec<Vec<Cell>> = candidates
        .iter()
        .map(|pod| {
            let row = PodRow::from_pod(pod, None, now);
            vec![
                Cell::plain(row.namespace),
                Cell::plain(row.name),
                Cell::plain(row.status),
                Cell::plain(row.age),
            ]
        })
        .collect();
    // Plain: this is an error message on stderr, which `main` has no palette
    // for, and a table of three names gains nothing from ink.
    format::table(
        &["NAMESPACE", "NAME", "STATUS", "AGE"],
        &rows,
        Palette::Plain,
    )
}

/// Why no container could be chosen.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContainerError {
    /// `--container` named one the pod does not have.
    #[error(
        "pod {pod} has no container called {wanted:?}.\n\
         Its containers are {available}; pass one of those to `--container`."
    )]
    NotFound {
        pod: String,
        wanted: String,
        available: String,
    },
    /// Several containers, no annotation choosing one, and none named.
    #[error(
        "pod {pod} has {count} containers and does not say which is the default.\n\
         Pick one with `--container`: {available}."
    )]
    Unchosen {
        pod: String,
        count: usize,
        available: String,
    },
    /// A pod spec with no containers at all, which the API server should never
    /// accept but which nothing here should assume.
    #[error("pod {pod} has no containers to run a command in.")]
    Empty { pod: String },
}

/// Which container to run in: the one named, else the one the pod's
/// [`DEFAULT_CONTAINER`] annotation names, else the only one.
///
/// Ephemeral containers are candidates for an explicit `--container`, since
/// one somebody added with `kubectl debug` is exactly where they will want a
/// shell, but never for the default — they come and go, and the annotation
/// and "the only one" are both statements about the pod as it was written.
/// An annotation naming a container that does not exist is ignored, as
/// `kubectl` ignores it, rather than turned into an error the user cannot
/// fix from their side.
pub fn container(pod: &Pod, wanted: Option<&str>) -> Result<String, ContainerError> {
    let pod_name = name(pod).to_owned();
    let regular = regular_containers(pod);
    let ephemeral = ephemeral_containers(pod);

    if let Some(wanted) = wanted {
        if regular.iter().chain(&ephemeral).any(|name| *name == wanted) {
            return Ok(wanted.to_owned());
        }
        return Err(ContainerError::NotFound {
            pod: pod_name,
            wanted: wanted.to_owned(),
            available: listed(regular.iter().chain(&ephemeral)),
        });
    }

    let annotated = pod
        .metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(DEFAULT_CONTAINER))
        .filter(|named| regular.contains(&named.as_str()));
    if let Some(named) = annotated {
        return Ok(named.clone());
    }

    match regular.as_slice() {
        [] => Err(ContainerError::Empty { pod: pod_name }),
        [only] => Ok((*only).to_owned()),
        several => Err(ContainerError::Unchosen {
            pod: pod_name,
            count: several.len(),
            available: listed(several.iter()),
        }),
    }
}

/// The names in a sentence: `app, sidecar, or proxy`.
fn listed<'a>(names: impl Iterator<Item = &'a &'a str>) -> String {
    let names: Vec<String> = names.map(|name| (*name).to_owned()).collect();
    format::list(&names, "or").unwrap_or_else(|| "none".to_owned())
}

fn regular_containers(pod: &Pod) -> Vec<&str> {
    pod.spec
        .as_ref()
        .map(|spec| spec.containers.iter().map(|c| c.name.as_str()).collect())
        .unwrap_or_default()
}

fn ephemeral_containers(pod: &Pod) -> Vec<&str> {
    pod.spec
        .as_ref()
        .and_then(|spec| spec.ephemeral_containers.as_ref())
        .map(|containers| containers.iter().map(|c| c.name.as_str()).collect())
        .unwrap_or_default()
}

/// Why a command cannot be run in a container right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotRunning {
    /// The pod as a whole is not `Running`: still scheduling or pulling, or
    /// already finished.
    Pod {
        /// `status.phase`, e.g. `Pending`.
        phase: String,
        /// `kubectl`'s STATUS wording when it says more than the phase does,
        /// e.g. `ImagePullBackOff` for a `Pending` pod.
        status: Option<String>,
    },
    /// The pod is running, but this container is not — usually a crash loop.
    Container {
        container: String,
        /// The waiting or terminated reason, e.g. `CrashLoopBackOff`.
        reason: Option<String>,
    },
}

/// Whether `container` in `pod` is running, and so can run a command.
///
/// Asked before connecting, because the API server's own answer to an exec
/// into a container that is not running is a bare `container not found` or
/// a `500` that says nothing about the crash loop behind it.
///
/// A container with no status at all on a running pod is let through: the
/// kubelet has not reported yet, and the API server is better placed to say
/// what it makes of that than a guess made here.
pub fn running(pod: &Pod, container: &str, now: Timestamp) -> Result<(), NotRunning> {
    let phase = pod
        .status
        .as_ref()
        .and_then(|status| status.phase.clone())
        .unwrap_or_else(|| "Unknown".to_owned());
    if phase != "Running" {
        let status = PodRow::from_pod(pod, None, now).status;
        return Err(NotRunning::Pod {
            status: (status != phase).then_some(status),
            phase,
        });
    }

    let Some(state) = container_status(pod, container).and_then(|status| status.state.as_ref())
    else {
        return Ok(());
    };
    if state.running.is_some() {
        return Ok(());
    }
    let reason = state
        .waiting
        .as_ref()
        .and_then(|waiting| waiting.reason.clone())
        .or_else(|| {
            state
                .terminated
                .as_ref()
                .and_then(|terminated| terminated.reason.clone())
        });
    Err(NotRunning::Container {
        container: container.to_owned(),
        reason,
    })
}

/// The pod's regular containers that are running right now, in spec order —
/// what to suggest instead when the one asked for is not.
#[must_use]
pub fn running_containers(pod: &Pod) -> Vec<String> {
    regular_containers(pod)
        .into_iter()
        .filter(|name| {
            container_status(pod, name)
                .and_then(|status| status.state.as_ref())
                .is_some_and(|state| state.running.is_some())
        })
        .map(ToOwned::to_owned)
        .collect()
}

fn container_status<'a>(pod: &'a Pod, container: &str) -> Option<&'a ContainerStatus> {
    let status = pod.status.as_ref()?;
    status
        .container_statuses
        .iter()
        .flatten()
        .chain(status.ephemeral_container_statuses.iter().flatten())
        .find(|status| status.name == container)
}

/// A pod's name, or nothing for the nameless pod a fixture might build.
fn name(pod: &Pod) -> &str {
    pod.metadata.name.as_deref().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::collections::BTreeMap;

    use k8s_openapi::api::core::v1::{
        Container, ContainerState, ContainerStateRunning, ContainerStateTerminated,
        ContainerStateWaiting, EphemeralContainer, PodSpec, PodStatus,
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};

    use super::*;

    fn now() -> Timestamp {
        "2026-10-03T12:00:00Z".parse().unwrap()
    }

    fn pod(name: &str, containers: &[&str]) -> Pod {
        Pod {
            metadata: ObjectMeta {
                name: Some(name.to_owned()),
                namespace: Some("default".to_owned()),
                creation_timestamp: Some(Time("2026-10-03T11:00:00Z".parse().unwrap())),
                ..ObjectMeta::default()
            },
            spec: Some(PodSpec {
                containers: containers
                    .iter()
                    .map(|name| Container {
                        name: (*name).to_owned(),
                        ..Container::default()
                    })
                    .collect(),
                ..PodSpec::default()
            }),
            status: Some(PodStatus {
                phase: Some("Running".to_owned()),
                container_statuses: Some(
                    containers
                        .iter()
                        .map(|name| ContainerStatus {
                            name: (*name).to_owned(),
                            ready: true,
                            state: Some(ContainerState {
                                running: Some(ContainerStateRunning::default()),
                                ..ContainerState::default()
                            }),
                            ..ContainerStatus::default()
                        })
                        .collect(),
                ),
                ..PodStatus::default()
            }),
        }
    }

    fn in_namespace(mut pod: Pod, namespace: &str) -> Pod {
        pod.metadata.namespace = Some(namespace.to_owned());
        pod
    }

    fn annotated(mut pod: Pod, default: &str) -> Pod {
        pod.metadata.annotations = Some(BTreeMap::from([(
            DEFAULT_CONTAINER.to_owned(),
            default.to_owned(),
        )]));
        pod
    }

    fn names(found: &Match<'_>) -> Vec<String> {
        match found {
            Match::One(pod) => vec![name(pod).to_owned()],
            Match::Several(pods) => pods.iter().map(|pod| name(pod).to_owned()).collect(),
            Match::None => Vec::new(),
        }
    }

    // --- find ---

    #[test]
    fn a_full_name_finds_its_pod() {
        let pods = [pod("api-7d9f-xk2", &["app"]), pod("worker-1", &["app"])];
        assert_eq!(names(&find(&pods, "api-7d9f-xk2")), ["api-7d9f-xk2"]);
    }

    #[test]
    fn a_unique_prefix_finds_its_pod() {
        let pods = [pod("api-7d9f-xk2", &["app"]), pod("worker-1", &["app"])];
        assert!(matches!(find(&pods, "api"), Match::One(_)));
        assert_eq!(names(&find(&pods, "api")), ["api-7d9f-xk2"]);
    }

    #[test]
    fn a_shared_prefix_returns_every_candidate_in_listing_order() {
        let pods = [
            pod("api-7d9f-aaa", &["app"]),
            pod("api-7d9f-bbb", &["app"]),
            pod("worker-1", &["app"]),
        ];
        let found = find(&pods, "api");
        assert!(matches!(found, Match::Several(_)));
        assert_eq!(names(&found), ["api-7d9f-aaa", "api-7d9f-bbb"]);
    }

    #[test]
    fn an_exact_name_wins_over_longer_names_it_is_a_prefix_of() {
        // Otherwise `api` could never be chosen while `api-canary` exists.
        let pods = [pod("api", &["app"]), pod("api-canary", &["app"])];
        assert!(matches!(find(&pods, "api"), Match::One(found) if name(found) == "api"));
    }

    #[test]
    fn a_prefix_that_starts_no_name_matches_nothing() {
        let pods = [pod("api-1", &["app"])];
        assert_eq!(find(&pods, "web"), Match::None);
    }

    #[test]
    fn matching_is_by_prefix_not_by_substring() {
        let pods = [pod("my-api-1", &["app"])];
        assert_eq!(find(&pods, "api"), Match::None);
    }

    #[test]
    fn an_empty_namespace_matches_nothing() {
        assert_eq!(find(&[], "api"), Match::None);
    }

    #[test]
    fn an_empty_name_never_picks_the_only_pod() {
        let pods = [pod("api-1", &["app"])];
        assert_eq!(find(&pods, ""), Match::None);
    }

    // --- ambiguous / not_found ---

    #[test]
    fn an_ambiguous_prefix_lists_namespace_and_status_and_asks_for_more() {
        let mut crashing = pod("api-7d9f-bbb", &["app"]);
        crashing.status = Some(PodStatus {
            phase: Some("Running".to_owned()),
            container_statuses: Some(vec![ContainerStatus {
                name: "app".to_owned(),
                state: Some(ContainerState {
                    waiting: Some(ContainerStateWaiting {
                        reason: Some("CrashLoopBackOff".to_owned()),
                        ..ContainerStateWaiting::default()
                    }),
                    ..ContainerState::default()
                }),
                ..ContainerStatus::default()
            }]),
            ..PodStatus::default()
        });
        let pods = [pod("api-7d9f-aaa", &["app"]), crashing];
        let Match::Several(candidates) = find(&pods, "api") else {
            panic!("expected several");
        };

        let text = ambiguous("api", &candidates, now());

        assert_eq!(
            text,
            "2 pods start with \"api\":\n\
             \n\
             NAMESPACE  NAME          STATUS            AGE\n\
             default    api-7d9f-aaa  Running           60m\n\
             default    api-7d9f-bbb  CrashLoopBackOff  60m\n\
             \n\
             Type more of the name to pick one, e.g. `api-7d9f-aaa`."
        );
    }

    #[test]
    fn not_found_with_nothing_elsewhere_points_at_the_listings() {
        let text = not_found("web", "default", &[], now());
        assert_eq!(
            text,
            "no pod in namespace default is called \"web\" or starts with it.\n\
             Run `eks pods -n default` to see what is there, or `eks pods -A` to search every namespace."
        );
    }

    #[test]
    fn not_found_names_the_one_namespace_that_has_it() {
        let elsewhere = in_namespace(pod("web-1", &["app"]), "shop");
        let text = not_found("web", "default", &[&elsewhere], now());
        assert!(
            text.ends_with("There is one in namespace shop: pass `-n shop` to use it."),
            "{text}"
        );
    }

    #[test]
    fn not_found_lists_several_namespaces_that_have_it() {
        let shop = in_namespace(pod("web-1", &["app"]), "shop");
        let blog = in_namespace(pod("web-2", &["app"]), "blog");
        let text = not_found("web", "default", &[&shop, &blog], now());
        assert!(text.contains("shop       web-1"), "{text}");
        assert!(text.contains("blog       web-2"), "{text}");
        assert!(text.ends_with("Pass `-n <namespace>` with the one you meant."));
    }

    // --- container ---

    #[test]
    fn the_only_container_is_the_default() {
        assert_eq!(container(&pod("p", &["app"]), None).unwrap(), "app");
    }

    #[test]
    fn the_annotation_chooses_among_several() {
        let pod = annotated(pod("p", &["istio-proxy", "app"]), "app");
        assert_eq!(container(&pod, None).unwrap(), "app");
    }

    #[test]
    fn a_named_container_wins_over_the_annotation() {
        let pod = annotated(pod("p", &["istio-proxy", "app"]), "app");
        assert_eq!(container(&pod, Some("istio-proxy")).unwrap(), "istio-proxy");
    }

    #[test]
    fn an_annotation_naming_no_container_is_ignored() {
        let single = annotated(pod("p", &["app"]), "gone");
        assert_eq!(container(&single, None).unwrap(), "app");

        let several = annotated(pod("p", &["a", "b"]), "gone");
        assert!(matches!(
            container(&several, None),
            Err(ContainerError::Unchosen { .. })
        ));
    }

    #[test]
    fn several_containers_and_no_default_names_them_all() {
        let error = container(&pod("api-1", &["app", "sidecar", "proxy"]), None).unwrap_err();
        assert_eq!(
            error.to_string(),
            "pod api-1 has 3 containers and does not say which is the default.\n\
             Pick one with `--container`: app, sidecar, or proxy."
        );
    }

    #[test]
    fn an_unknown_container_names_the_real_ones() {
        let error = container(&pod("api-1", &["app", "sidecar"]), Some("ap")).unwrap_err();
        assert_eq!(
            error.to_string(),
            "pod api-1 has no container called \"ap\".\n\
             Its containers are app or sidecar; pass one of those to `--container`."
        );
    }

    #[test]
    fn an_ephemeral_container_can_be_named_but_is_never_the_default() {
        let mut debugged = pod("p", &["app"]);
        if let Some(spec) = debugged.spec.as_mut() {
            spec.ephemeral_containers = Some(vec![EphemeralContainer {
                name: "debugger".to_owned(),
                ..EphemeralContainer::default()
            }]);
        }
        assert_eq!(container(&debugged, Some("debugger")).unwrap(), "debugger");
        assert_eq!(container(&debugged, None).unwrap(), "app");
    }

    #[test]
    fn a_pod_without_a_spec_has_no_container_to_choose() {
        let mut bare = pod("p", &[]);
        bare.spec = None;
        assert_eq!(
            container(&bare, None),
            Err(ContainerError::Empty {
                pod: "p".to_owned()
            })
        );
    }

    // --- running ---

    #[test]
    fn a_running_container_in_a_running_pod_can_run_a_command() {
        assert_eq!(running(&pod("p", &["app"]), "app", now()), Ok(()));
    }

    #[test]
    fn a_pending_pod_reports_its_phase_and_the_status_behind_it() {
        let mut pending = pod("p", &["app"]);
        pending.status = Some(PodStatus {
            phase: Some("Pending".to_owned()),
            container_statuses: Some(vec![ContainerStatus {
                name: "app".to_owned(),
                state: Some(ContainerState {
                    waiting: Some(ContainerStateWaiting {
                        reason: Some("ImagePullBackOff".to_owned()),
                        ..ContainerStateWaiting::default()
                    }),
                    ..ContainerState::default()
                }),
                ..ContainerStatus::default()
            }]),
            ..PodStatus::default()
        });
        assert_eq!(
            running(&pending, "app", now()),
            Err(NotRunning::Pod {
                phase: "Pending".to_owned(),
                status: Some("ImagePullBackOff".to_owned()),
            })
        );
    }

    #[test]
    fn a_status_that_only_repeats_the_phase_is_left_out() {
        let mut pending = pod("p", &["app"]);
        pending.status = Some(PodStatus {
            phase: Some("Pending".to_owned()),
            ..PodStatus::default()
        });
        assert_eq!(
            running(&pending, "app", now()),
            Err(NotRunning::Pod {
                phase: "Pending".to_owned(),
                status: None,
            })
        );
    }

    #[test]
    fn a_pod_with_no_status_is_unknown_rather_than_running() {
        let mut fresh = pod("p", &["app"]);
        fresh.status = None;
        assert!(matches!(
            running(&fresh, "app", now()),
            Err(NotRunning::Pod { phase, .. }) if phase == "Unknown"
        ));
    }

    #[test]
    fn a_crash_looping_container_in_a_running_pod_is_named_with_its_reason() {
        let mut pod = pod("p", &["app", "sidecar"]);
        if let Some(status) = pod.status.as_mut() {
            status.container_statuses = Some(vec![
                ContainerStatus {
                    name: "app".to_owned(),
                    state: Some(ContainerState {
                        waiting: Some(ContainerStateWaiting {
                            reason: Some("CrashLoopBackOff".to_owned()),
                            ..ContainerStateWaiting::default()
                        }),
                        ..ContainerState::default()
                    }),
                    ..ContainerStatus::default()
                },
                ContainerStatus {
                    name: "sidecar".to_owned(),
                    state: Some(ContainerState {
                        running: Some(ContainerStateRunning::default()),
                        ..ContainerState::default()
                    }),
                    ..ContainerStatus::default()
                },
            ]);
        }
        assert_eq!(
            running(&pod, "app", now()),
            Err(NotRunning::Container {
                container: "app".to_owned(),
                reason: Some("CrashLoopBackOff".to_owned()),
            })
        );
        assert_eq!(running(&pod, "sidecar", now()), Ok(()));
    }

    #[test]
    fn running_containers_lists_only_those_running_in_spec_order() {
        let mut pod = pod("p", &["app", "sidecar", "proxy"]);
        if let Some(status) = pod.status.as_mut()
            && let Some(statuses) = status.container_statuses.as_mut()
        {
            statuses[0].state = Some(ContainerState {
                waiting: Some(ContainerStateWaiting::default()),
                ..ContainerState::default()
            });
        }
        assert_eq!(running_containers(&pod), ["sidecar", "proxy"]);

        pod.status = None;
        assert_eq!(running_containers(&pod), Vec::<String>::new());
    }

    #[test]
    fn a_terminated_container_reports_why_it_ended() {
        let mut pod = pod("p", &["app"]);
        if let Some(status) = pod.status.as_mut() {
            status.container_statuses = Some(vec![ContainerStatus {
                name: "app".to_owned(),
                state: Some(ContainerState {
                    terminated: Some(ContainerStateTerminated {
                        reason: Some("OOMKilled".to_owned()),
                        ..ContainerStateTerminated::default()
                    }),
                    ..ContainerState::default()
                }),
                ..ContainerStatus::default()
            }]);
        }
        assert!(matches!(
            running(&pod, "app", now()),
            Err(NotRunning::Container { reason: Some(reason), .. }) if reason == "OOMKilled"
        ));
    }

    #[test]
    fn a_container_the_kubelet_has_not_reported_on_is_let_through() {
        let mut pod = pod("p", &["app"]);
        if let Some(status) = pod.status.as_mut() {
            status.container_statuses = None;
        }
        assert_eq!(running(&pod, "app", now()), Ok(()));
    }
}
