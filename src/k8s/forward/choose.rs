//! Which pod a forward lands on, and what to do when that pod goes away.
//!
//! `kubectl port-forward svc/api` resolves the service to one pod when it
//! starts and then forgets the service: when that pod is replaced — a deploy,
//! an eviction, a crash — the forward dies with a stream error, which is
//! exactly the moment somebody debugging a service needed it most. `eks`
//! remembers what it was asked to reach, and these functions decide, from a
//! fresh look at the pods, whether to stay, move to another pod, wait for
//! one, or stop and say why.

use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::{Pod, Service};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use k8s_openapi::jiff::Timestamp;
use kube::ResourceExt;

use crate::format;
use crate::k8s::forward::Surface;
use crate::k8s::forward::spec::{Kind, Target};
use crate::k8s::pods::PodRow;
use crate::k8s::pods::pick;
use crate::k8s::selector;

/// What state a pod is in, as far as forwarding to it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    /// Running and passing its readiness checks: what a service sends to.
    Ready,
    /// Running, but not ready. A service would not send to it; somebody who
    /// named it may well want to, to find out why.
    Unready,
    /// Deleted and in its grace period. Still answering, perhaps, but not for
    /// long.
    Terminating,
    /// `Succeeded` or `Failed`: nothing in it is listening any more.
    Finished(String),
    /// Not started: `Pending`, or `kubectl`'s word for why, such as
    /// `ImagePullBackOff`.
    NotStarted(String),
}

/// How `pod` stands.
#[must_use]
pub fn health(pod: &Pod, now: Timestamp) -> Health {
    let status = pod.status.as_ref();
    let phase = status
        .and_then(|status| status.phase.as_deref())
        .unwrap_or("Unknown");
    if matches!(phase, "Succeeded" | "Failed") {
        return Health::Finished(phase.to_owned());
    }
    if pod.metadata.deletion_timestamp.is_some() {
        return Health::Terminating;
    }
    if phase != "Running" {
        return Health::NotStarted(PodRow::from_pod(pod, None, now).status);
    }
    let ready = status
        .and_then(|status| status.conditions.as_ref())
        .and_then(|conditions| conditions.iter().find(|c| c.type_ == "Ready"))
        .is_some_and(|condition| condition.status == "True");
    if ready {
        Health::Ready
    } else {
        Health::Unready
    }
}

/// The ready pod to forward to: `current` while it is still ready, so a
/// forward does not hop between replicas on every look, and otherwise the
/// oldest ready one — the replica least likely to be the next one replaced.
#[must_use]
pub fn choose<'a>(pods: &'a [Pod], current: Option<&str>, now: Timestamp) -> Option<&'a Pod> {
    let mut ready: Vec<&Pod> = pods
        .iter()
        .filter(|pod| health(pod, now) == Health::Ready)
        .collect();
    if let Some(current) = current
        && let Some(pod) = ready.iter().find(|pod| pod.name_any() == current)
    {
        return Some(pod);
    }
    ready.sort_by(|a, b| {
        let created = |pod: &Pod| pod.metadata.creation_timestamp.as_ref().map(|time| time.0);
        // `None` sorts first in an `Option`, and a pod with no creation time
        // is one nothing can be said about, so it goes last instead.
        match (created(a), created(b)) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| a.name_any().cmp(&b.name_any()))
    });
    ready.first().copied()
}

/// What a fresh look at the pods behind a service or deployment says to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Move {
    /// Carry on with the pod in use, or carry on waiting.
    Keep,
    /// Forward to `to` from now on, because of `why`.
    Switch { to: String, why: String },
    /// Stop forwarding until a pod is ready, because of this.
    Wait(String),
}

/// Decide, for a `svc/` or `deploy/` forward, from the pods its selector
/// matches now.
///
/// `current` is the pod being forwarded to, or `None` while waiting. When no
/// pod is ready, the current one is kept for as long as it exists and has
/// not finished: an unready or terminating pod may still answer, and is a
/// better destination than none.
#[must_use]
pub fn after_set(target: &Target, current: Option<&str>, pods: &[Pod], now: Timestamp) -> Move {
    let chosen = choose(pods, current, now).map(ResourceExt::name_any);
    let current_pod = current.and_then(|name| pods.iter().find(|pod| pod.name_any() == name));
    match (current, chosen) {
        (Some(current), Some(chosen)) if current == chosen => Move::Keep,
        (None, None) => Move::Keep,
        (None, Some(to)) => Move::Switch {
            to,
            why: format!("a pod behind {target} is ready"),
        },
        (Some(current), Some(to)) => Move::Switch {
            to,
            why: why_left(current, current_pod, now),
        },
        (Some(current), None) => match current_pod.map(|pod| health(pod, now)) {
            Some(Health::Unready | Health::Terminating | Health::Ready) => Move::Keep,
            _ => Move::Wait(format!(
                "{}, and no other pod behind {target} is ready yet",
                why_left(current, current_pod, now)
            )),
        },
    }
}

/// Why the pod in use is being left: the first half of a sentence.
fn why_left(name: &str, pod: Option<&Pod>, now: Timestamp) -> String {
    match pod.map(|pod| health(pod, now)) {
        None => format!("pod {name} is gone"),
        Some(Health::Terminating) => format!("pod {name} is shutting down"),
        Some(Health::Finished(phase)) => format!("pod {name} has finished ({phase})"),
        Some(Health::Unready) => format!("pod {name} is not ready"),
        Some(Health::NotStarted(status)) => format!("pod {name} is {status}"),
        // Not reached through `after_set`, which keeps a ready current pod.
        Some(Health::Ready) => format!("pod {name} was replaced"),
    }
}

/// The line printed when a forward moves to another pod.
#[must_use]
pub fn switched(target: &Target, why: &str, to: &str) -> String {
    format!("{why}; {target} now forwards to pod {to}.")
}

/// The line printed when a forward stops until a pod is ready.
#[must_use]
pub fn waiting(why: &str) -> String {
    format!("{why}. Connections are refused until one is; eks is watching for it.")
}

/// What a fresh look at a pod named directly says to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Keep,
    /// Carry on, but say this once.
    Warn(String),
    /// Stop forwarding, and exit with this.
    Stop(String),
}

/// Decide, for a forward to a pod named directly, from the pod as it is now
/// (`None` when it no longer exists) and as it was last seen.
///
/// A pod has no successor to move to: its replacement has a new name, and
/// following one is what `svc/` and `deploy/` are for. So the message on the
/// way out says which of those would have followed it, when the pod says.
#[must_use]
pub fn after_pod(seen: Option<&Pod>, last: &Pod, now: Timestamp, surface: Surface) -> Verdict {
    let name = last.name_any();
    let follow = follow_hint(last, surface);
    match seen.map(|pod| health(pod, now)) {
        None => Verdict::Stop(format!(
            "pod {name} was deleted, so there is nothing left to forward to.\n{follow}"
        )),
        Some(Health::Finished(phase)) => Verdict::Stop(format!(
            "pod {name} has finished ({phase}), so nothing in it is listening any more.\n{follow}"
        )),
        Some(Health::Terminating) => Verdict::Warn(format!(
            "pod {name} is shutting down; its forwards stop when it is gone."
        )),
        Some(_) => Verdict::Keep,
    }
}

/// What to do instead of forwarding to `pod` by name, so the next forward
/// survives its replacement.
///
/// Following a deployment or a service is something only `eks
/// port-forward` does, so that advice names the command on either surface;
/// starting again on a pod that comes back under its own name is `f` in the
/// dashboard.
#[must_use]
pub fn follow_hint(pod: &Pod, surface: Surface) -> String {
    let owner = pod
        .metadata
        .owner_references
        .iter()
        .flatten()
        .find(|owner| owner.controller == Some(true))
        .or_else(|| pod.metadata.owner_references.iter().flatten().next());
    let Some(owner) = owner else {
        return "It had no controller, so nothing will replace it.".to_owned();
    };
    match owner.kind.as_str() {
        "ReplicaSet" => {
            // A Deployment names its ReplicaSets `<deployment>-<pod-template-hash>`
            // and labels their pods with that hash, which is how the owner of
            // the owner is found without another request.
            let hash = pod
                .labels()
                .get("pod-template-hash")
                .map(|hash| format!("-{hash}"));
            match hash.and_then(|hash| owner.name.strip_suffix(hash.as_str()).map(str::to_owned)) {
                Some(deployment) => format!(
                    "To follow whichever pod replaces it, forward to its deployment: \
                     `eks port-forward deploy/{deployment}`."
                ),
                None => format!(
                    "It belonged to ReplicaSet {}; forward to a service in front of its pods to follow them.",
                    owner.name
                ),
            }
        }
        "StatefulSet" => {
            let again = match surface {
                Surface::Command => "run the same command again",
                Surface::Dashboard => "press f on its port again",
            };
            format!(
                "StatefulSet {} will recreate it under the same name: {again} once it is Running.",
                owner.name
            )
        }
        kind => format!(
            "{kind} {} may replace it under a new name; a service in front of its pods \
             (`eks port-forward svc/<name>`) follows them.",
            owner.name
        ),
    }
}

/// Why a bare pod cannot be forwarded to at all.
#[must_use]
pub fn unusable(pod: &Pod, now: Timestamp, surface: Surface) -> Option<String> {
    let name = pod.name_any();
    match health(pod, now) {
        Health::Finished(phase) => Some(format!(
            "pod {name} has finished ({phase}), so nothing in it is listening.\n{}",
            follow_hint(pod, surface)
        )),
        Health::NotStarted(status) => {
            let next = match surface {
                Surface::Command => format!(
                    "`eks pods` shows when it is Running; `kubectl describe pod {name}` shows its events."
                ),
                Surface::Dashboard => {
                    "Its events, under its containers, say why; press f again once it is Running."
                        .to_owned()
                }
            };
            Some(format!(
                "pod {name} is {status}, so nothing in it is listening yet.\n{next}"
            ))
        }
        Health::Ready | Health::Unready | Health::Terminating => None,
    }
}

/// The label selector that picks a service's pods, in the canonical form
/// [`selector::label_selector`] vouches for.
pub fn service_selector(service: &Service) -> Result<String, String> {
    let name = service.name_any();
    let spec = service.spec.as_ref();
    if let Some(external) = spec
        .filter(|spec| spec.type_.as_deref() == Some("ExternalName"))
        .and_then(|spec| spec.external_name.as_deref())
    {
        return Err(format!(
            "service {name} is an ExternalName for {external}, outside the cluster, so it has no pods to forward to.\n\
             Connect to {external} directly."
        ));
    }
    let labels: BTreeMap<String, String> = spec
        .and_then(|spec| spec.selector.clone())
        .unwrap_or_default();
    if labels.is_empty() {
        return Err(format!(
            "service {name} has no selector, so eks cannot tell which pods are behind it \
             (its endpoints are managed by hand or by another controller).\n\
             Forward to one of those pods by name instead."
        ));
    }
    let text = labels
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",");
    selector::label_selector(&text).map_err(|error| unreadable(Kind::Service, &name, &error))
}

/// The label selector that picks a deployment's pods, in canonical form.
pub fn deployment_selector(deployment: &Deployment) -> Result<String, String> {
    let name = deployment.name_any();
    let selector = deployment.spec.as_ref().map(|spec| &spec.selector);
    let text = selector.map(label_selector_text).unwrap_or_default();
    if text.is_empty() {
        return Err(format!(
            "deployment {name} has an empty selector, so eks cannot tell which pods are its own.\n\
             Forward to one of its pods by name instead."
        ));
    }
    selector::label_selector(&text).map_err(|error| unreadable(Kind::Deployment, &name, &error))
}

fn unreadable(kind: Kind, name: &str, error: &selector::Error) -> String {
    format!(
        "the selector on {noun} {name} could not be read ({error}).\n\
         Forward to one of its pods by name instead.",
        noun = kind.noun()
    )
}

/// A `LabelSelector` as the selector string the API server reads.
fn label_selector_text(selector: &LabelSelector) -> String {
    let labels = selector
        .match_labels
        .iter()
        .flatten()
        .map(|(key, value)| format!("{key}={value}"));
    let expressions = selector
        .match_expressions
        .iter()
        .flatten()
        .filter_map(|expression| {
            let key = &expression.key;
            let values = expression.values.clone().unwrap_or_default().join(",");
            match expression.operator.as_str() {
                "In" => Some(format!("{key} in ({values})")),
                "NotIn" => Some(format!("{key} notin ({values})")),
                "Exists" => Some(key.clone()),
                "DoesNotExist" => Some(format!("!{key}")),
                _ => None,
            }
        });
    labels.chain(expressions).collect::<Vec<_>>().join(",")
}

/// Why a service or deployment has no pod to forward to.
#[must_use]
pub fn none_ready(target: &Target, selector: &str, pods: &[Pod], now: Timestamp) -> String {
    let noun = target.kind.noun();
    let name = &target.name;
    if pods.is_empty() {
        let scaled = match target.kind {
            Kind::Deployment => {
                "It may be scaled to zero: `kubectl get deploy` shows its replicas."
            }
            _ => {
                "Its workload may be scaled to zero, or the selector may not match the pods' labels."
            }
        };
        return format!(
            "no pods match {noun} {name}'s selector ({selector}), so there is nothing to forward to.\n{scaled}"
        );
    }
    let candidates: Vec<&Pod> = pods.iter().collect();
    format!(
        "none of the {count} behind {target} is ready, so there is nothing to forward to yet:\n\n{table}\n\n\
         `eks pods -l {selector}` follows them; forward to one by name to reach it while it is not ready.",
        count = format::count(pods.len()) + if pods.len() == 1 { " pod" } else { " pods" },
        table = pick::candidate_table(&candidates, now),
    )
}

/// How a typed name matched the services or deployments in a namespace.
#[derive(Debug, PartialEq)]
pub enum Found<'a, T> {
    One(&'a T),
    Several(Vec<&'a T>),
    None,
}

/// Find the object `wanted` names, as a full name or a unique prefix — the
/// rule `eks exec` has for pods, for services and deployments too.
#[must_use]
pub fn find<'a, T: kube::Resource>(items: &'a [T], wanted: &str) -> Found<'a, T> {
    if let Some(exact) = items
        .iter()
        .find(|item| item.meta().name.as_deref() == Some(wanted))
    {
        return Found::One(exact);
    }
    if wanted.is_empty() {
        return Found::None;
    }
    let mut starting: Vec<&T> = items
        .iter()
        .filter(|item| {
            item.meta()
                .name
                .as_deref()
                .is_some_and(|name| name.starts_with(wanted))
        })
        .collect();
    match starting.len() {
        0 => Found::None,
        1 => starting.pop().map_or(Found::None, Found::One),
        _ => Found::Several(starting),
    }
}

/// How many names to list before saying how many more there are.
const NAMES_SHOWN: usize = 8;

/// The names in a sentence, cut short after [`NAMES_SHOWN`].
fn some_names(names: &[String]) -> String {
    if names.len() <= NAMES_SHOWN {
        return format::list(names, "and").unwrap_or_default();
    }
    let shown = names[..NAMES_SHOWN].join(", ");
    format!("{shown}, and {} more", names.len() - NAMES_SHOWN)
}

/// What to print when a prefix matches several services or deployments.
#[must_use]
pub fn ambiguous(kind: Kind, wanted: &str, names: &[String]) -> String {
    format!(
        "{count} {noun}s start with {wanted:?}: {listed}.\n\
         Type more of the name to pick one, e.g. `{prefix}/{example}`.",
        count = names.len(),
        noun = kind.noun(),
        listed = some_names(names),
        prefix = kind.prefix(),
        example = names.first().map(String::as_str).unwrap_or_default(),
    )
}

/// What to print when no service or deployment in `namespace` is called
/// `wanted` or starts with it.
///
/// `here` is every name of that kind in the namespace, so the reader can see
/// what they might have meant; `elsewhere` the namespaces where one does
/// match, so they can see where it is.
#[must_use]
pub fn not_found(
    kind: Kind,
    wanted: &str,
    namespace: &str,
    here: &[String],
    elsewhere: &[String],
) -> String {
    let noun = kind.noun();
    let mut lines = vec![format!(
        "no {noun} in namespace {namespace} is called {wanted:?} or starts with it."
    )];
    match elsewhere {
        [] => {}
        [only] => lines.push(format!(
            "There is one in namespace {only}: pass `-n {only}` to use it."
        )),
        several => lines.push(format!(
            "Namespaces {} have one; pass `-n` with the one you meant.",
            some_names(several)
        )),
    }
    if elsewhere.is_empty() {
        lines.push(if here.is_empty() {
            format!("There are no {noun}s in namespace {namespace} at all.")
        } else {
            format!("The {noun}s there are {}.", some_names(here))
        });
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use k8s_openapi::api::apps::v1::DeploymentSpec;
    use k8s_openapi::api::core::v1::{PodCondition, PodSpec, PodStatus, ServiceSpec};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{
        LabelSelectorRequirement, ObjectMeta, OwnerReference, Time,
    };

    use super::*;

    fn now() -> Timestamp {
        "2026-10-05T12:00:00Z".parse().unwrap()
    }

    fn pod(name: &str, phase: &str, ready: bool, created: &str) -> Pod {
        Pod {
            metadata: ObjectMeta {
                name: Some(name.to_owned()),
                namespace: Some("default".to_owned()),
                creation_timestamp: Some(Time(created.parse().unwrap())),
                ..ObjectMeta::default()
            },
            spec: Some(PodSpec::default()),
            status: Some(PodStatus {
                phase: Some(phase.to_owned()),
                conditions: Some(vec![PodCondition {
                    type_: "Ready".to_owned(),
                    status: if ready { "True" } else { "False" }.to_owned(),
                    ..PodCondition::default()
                }]),
                ..PodStatus::default()
            }),
        }
    }

    fn ready(name: &str, created: &str) -> Pod {
        pod(name, "Running", true, created)
    }

    fn terminating(mut pod: Pod) -> Pod {
        pod.metadata.deletion_timestamp = Some(Time("2026-10-05T11:59:00Z".parse().unwrap()));
        pod
    }

    fn owned(mut pod: Pod, kind: &str, owner: &str, hash: Option<&str>) -> Pod {
        pod.metadata.owner_references = Some(vec![OwnerReference {
            kind: kind.to_owned(),
            name: owner.to_owned(),
            controller: Some(true),
            ..OwnerReference::default()
        }]);
        if let Some(hash) = hash {
            pod.metadata.labels = Some(BTreeMap::from([(
                "pod-template-hash".to_owned(),
                hash.to_owned(),
            )]));
        }
        pod
    }

    fn svc_target() -> Target {
        Target::parse("svc/api").unwrap()
    }

    const EARLY: &str = "2026-10-05T10:00:00Z";
    const LATE: &str = "2026-10-05T11:00:00Z";

    #[test]
    fn health_reads_ready_unready_terminating_finished_and_pending() {
        assert_eq!(health(&ready("a", EARLY), now()), Health::Ready);
        assert_eq!(
            health(&pod("a", "Running", false, EARLY), now()),
            Health::Unready
        );
        assert_eq!(
            health(&terminating(ready("a", EARLY)), now()),
            Health::Terminating
        );
        assert_eq!(
            health(&pod("a", "Failed", false, EARLY), now()),
            Health::Finished("Failed".to_owned())
        );
        assert_eq!(
            health(&pod("a", "Pending", false, EARLY), now()),
            Health::NotStarted("Pending".to_owned())
        );
    }

    #[test]
    fn a_finished_pod_that_is_also_being_deleted_reads_as_finished() {
        assert_eq!(
            health(&terminating(pod("a", "Succeeded", false, EARLY)), now()),
            Health::Finished("Succeeded".to_owned())
        );
    }

    #[test]
    fn a_pod_with_no_status_has_not_started() {
        let mut pod = ready("a", EARLY);
        pod.status = None;
        assert!(matches!(health(&pod, now()), Health::NotStarted(_)));
    }

    #[test]
    fn the_oldest_ready_pod_is_chosen() {
        let pods = [
            ready("api-b", LATE),
            pod("api-old", "Running", false, "2026-10-05T09:00:00Z"),
            ready("api-a", EARLY),
        ];
        assert_eq!(choose(&pods, None, now()).unwrap().name_any(), "api-a");
    }

    #[test]
    fn the_current_pod_is_kept_while_it_is_ready() {
        let pods = [ready("api-a", EARLY), ready("api-b", LATE)];
        assert_eq!(
            choose(&pods, Some("api-b"), now()).unwrap().name_any(),
            "api-b"
        );
    }

    #[test]
    fn no_ready_pod_chooses_nothing() {
        let pods = [pod("api-a", "Pending", false, EARLY)];
        assert_eq!(choose(&pods, None, now()), None);
        assert_eq!(choose(&[], None, now()), None);
    }

    #[test]
    fn a_ready_current_pod_is_kept() {
        let pods = [ready("api-a", EARLY), ready("api-b", LATE)];
        assert_eq!(
            after_set(&svc_target(), Some("api-b"), &pods, now()),
            Move::Keep
        );
    }

    #[test]
    fn a_deleted_pod_moves_the_forward_to_another_ready_one() {
        let pods = [ready("api-b", LATE)];
        assert_eq!(
            after_set(&svc_target(), Some("api-a"), &pods, now()),
            Move::Switch {
                to: "api-b".to_owned(),
                why: "pod api-a is gone".to_owned()
            }
        );
    }

    #[test]
    fn a_terminating_pod_is_left_for_a_ready_one_during_a_rollout() {
        let pods = [terminating(ready("api-a", EARLY)), ready("api-b", LATE)];
        assert_eq!(
            after_set(&svc_target(), Some("api-a"), &pods, now()),
            Move::Switch {
                to: "api-b".to_owned(),
                why: "pod api-a is shutting down".to_owned()
            }
        );
    }

    #[test]
    fn an_unready_pod_is_left_for_a_ready_one() {
        let pods = [pod("api-a", "Running", false, EARLY), ready("api-b", LATE)];
        let Move::Switch { why, .. } = after_set(&svc_target(), Some("api-a"), &pods, now()) else {
            panic!("expected a switch");
        };
        assert_eq!(why, "pod api-a is not ready");
    }

    #[test]
    fn with_nothing_better_an_unready_or_terminating_pod_is_kept() {
        for pods in [
            vec![pod("api-a", "Running", false, EARLY)],
            vec![terminating(ready("api-a", EARLY))],
        ] {
            assert_eq!(
                after_set(&svc_target(), Some("api-a"), &pods, now()),
                Move::Keep
            );
        }
    }

    #[test]
    fn a_gone_pod_with_nothing_ready_behind_the_service_waits() {
        let pods = [pod("api-b", "Pending", false, LATE)];
        assert_eq!(
            after_set(&svc_target(), Some("api-a"), &pods, now()),
            Move::Wait(
                "pod api-a is gone, and no other pod behind svc/api is ready yet".to_owned()
            )
        );
    }

    #[test]
    fn a_finished_pod_with_nothing_ready_waits() {
        let pods = [pod("api-a", "Failed", false, EARLY)];
        let Move::Wait(why) = after_set(&svc_target(), Some("api-a"), &pods, now()) else {
            panic!("expected to wait");
        };
        assert!(why.starts_with("pod api-a has finished (Failed)"), "{why}");
    }

    #[test]
    fn waiting_ends_when_a_pod_is_ready_and_keeps_waiting_quietly_until_then() {
        assert_eq!(
            after_set(
                &svc_target(),
                None,
                &[pod("api-b", "Pending", false, LATE)],
                now()
            ),
            Move::Keep
        );
        assert_eq!(
            after_set(&svc_target(), None, &[ready("api-b", LATE)], now()),
            Move::Switch {
                to: "api-b".to_owned(),
                why: "a pod behind svc/api is ready".to_owned()
            }
        );
    }

    #[test]
    fn the_switch_and_wait_lines_are_sentences() {
        assert_eq!(
            switched(&svc_target(), "pod api-a is gone", "api-b"),
            "pod api-a is gone; svc/api now forwards to pod api-b."
        );
        assert!(waiting("pod api-a is gone").ends_with("eks is watching for it."));
    }

    #[test]
    fn a_running_pod_named_directly_is_kept() {
        let last = ready("api-a", EARLY);
        assert_eq!(
            after_pod(Some(&last), &last, now(), Surface::Command),
            Verdict::Keep
        );
        let unready = pod("api-a", "Running", false, EARLY);
        assert_eq!(
            after_pod(Some(&unready), &last, now(), Surface::Command),
            Verdict::Keep
        );
    }

    #[test]
    fn a_deleted_pod_named_directly_stops_and_names_its_deployment() {
        let last = owned(
            ready("api-7d9f8c6b5-xk2pq", EARLY),
            "ReplicaSet",
            "api-7d9f8c6b5",
            Some("7d9f8c6b5"),
        );
        let Verdict::Stop(text) = after_pod(None, &last, now(), Surface::Command) else {
            panic!("expected to stop");
        };
        assert!(
            text.starts_with("pod api-7d9f8c6b5-xk2pq was deleted"),
            "{text}"
        );
        assert!(text.contains("`eks port-forward deploy/api`"), "{text}");
    }

    #[test]
    fn a_finished_pod_named_directly_stops() {
        let last = ready("job-1", EARLY);
        let now_finished = pod("job-1", "Succeeded", false, EARLY);
        let Verdict::Stop(text) = after_pod(Some(&now_finished), &last, now(), Surface::Command)
        else {
            panic!("expected to stop");
        };
        assert!(text.contains("has finished (Succeeded)"), "{text}");
        assert!(text.contains("no controller"), "{text}");
    }

    #[test]
    fn a_terminating_pod_named_directly_is_warned_about_but_kept() {
        let last = ready("api-a", EARLY);
        let going = terminating(ready("api-a", EARLY));
        assert!(matches!(
            after_pod(Some(&going), &last, now(), Surface::Command),
            Verdict::Warn(text) if text.contains("shutting down")
        ));
    }

    #[test]
    fn a_statefulset_pod_is_said_to_come_back_under_its_own_name() {
        let pod = owned(ready("db-0", EARLY), "StatefulSet", "db", None);
        assert!(follow_hint(&pod, Surface::Command).contains("same name"));
    }

    #[test]
    fn in_the_dashboard_a_statefulset_pod_is_started_again_with_f() {
        let pod = owned(ready("db-0", EARLY), "StatefulSet", "db", None);
        let text = follow_hint(&pod, Surface::Dashboard);
        assert!(text.contains("press f on its port again"), "{text}");
        assert!(!text.contains("command"), "{text}");
    }

    #[test]
    fn following_a_deployment_names_the_command_on_either_surface() {
        let pod = owned(
            ready("api-7d9f8c6b5-xk2pq", EARLY),
            "ReplicaSet",
            "api-7d9f8c6b5",
            Some("7d9f8c6b5"),
        );
        assert_eq!(
            follow_hint(&pod, Surface::Dashboard),
            follow_hint(&pod, Surface::Command)
        );
    }

    #[test]
    fn in_the_dashboard_a_pod_not_yet_running_points_at_its_events_on_screen() {
        let text = unusable(
            &pod("api-a", "Pending", false, EARLY),
            now(),
            Surface::Dashboard,
        )
        .unwrap();
        assert!(text.starts_with("pod api-a is Pending"), "{text}");
        assert!(text.contains("press f again"), "{text}");
        assert!(!text.contains("kubectl"), "{text}");
    }

    #[test]
    fn a_replicaset_without_the_hash_label_points_at_a_service() {
        let pod = owned(ready("x-1", EARLY), "ReplicaSet", "x-abc", None);
        assert!(follow_hint(&pod, Surface::Command).contains("ReplicaSet x-abc"));
    }

    #[test]
    fn a_pending_or_finished_pod_cannot_be_forwarded_to_at_all() {
        let text = unusable(
            &pod("api-a", "Pending", false, EARLY),
            now(),
            Surface::Command,
        )
        .unwrap();
        assert!(text.starts_with("pod api-a is Pending"), "{text}");
        assert!(
            unusable(
                &pod("api-a", "Failed", false, EARLY),
                now(),
                Surface::Command
            )
            .is_some()
        );
        assert_eq!(
            unusable(
                &pod("api-a", "Running", false, EARLY),
                now(),
                Surface::Command
            ),
            None
        );
    }

    fn service(selector: Option<&[(&str, &str)]>) -> Service {
        Service {
            metadata: ObjectMeta {
                name: Some("api".to_owned()),
                ..ObjectMeta::default()
            },
            spec: Some(ServiceSpec {
                selector: selector.map(|pairs| {
                    pairs
                        .iter()
                        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                        .collect()
                }),
                ..ServiceSpec::default()
            }),
            status: None,
        }
    }

    #[test]
    fn a_service_selector_is_its_labels_joined() {
        let svc = service(Some(&[("app", "api"), ("tier", "web")]));
        assert_eq!(service_selector(&svc).unwrap(), "app=api,tier=web");
    }

    #[test]
    fn a_service_without_a_selector_says_to_name_a_pod() {
        let text = service_selector(&service(None)).unwrap_err();
        assert!(text.contains("has no selector"), "{text}");
        let text = service_selector(&service(Some(&[]))).unwrap_err();
        assert!(text.contains("has no selector"), "{text}");
    }

    #[test]
    fn an_external_name_service_says_where_it_points() {
        let mut svc = service(None);
        let spec = svc.spec.as_mut().unwrap();
        spec.type_ = Some("ExternalName".to_owned());
        spec.external_name = Some("db.example.com".to_owned());
        let text = service_selector(&svc).unwrap_err();
        assert!(text.contains("ExternalName for db.example.com"), "{text}");
    }

    fn deployment(selector: LabelSelector) -> Deployment {
        Deployment {
            metadata: ObjectMeta {
                name: Some("api".to_owned()),
                ..ObjectMeta::default()
            },
            spec: Some(DeploymentSpec {
                selector,
                ..DeploymentSpec::default()
            }),
            status: None,
        }
    }

    #[test]
    fn a_deployment_selector_includes_its_expressions() {
        let deploy = deployment(LabelSelector {
            match_labels: Some(BTreeMap::from([("app".to_owned(), "api".to_owned())])),
            match_expressions: Some(vec![
                LabelSelectorRequirement {
                    key: "track".to_owned(),
                    operator: "NotIn".to_owned(),
                    values: Some(vec!["canary".to_owned(), "shadow".to_owned()]),
                },
                LabelSelectorRequirement {
                    key: "tier".to_owned(),
                    operator: "Exists".to_owned(),
                    values: None,
                },
                LabelSelectorRequirement {
                    key: "legacy".to_owned(),
                    operator: "DoesNotExist".to_owned(),
                    values: None,
                },
            ]),
        });
        assert_eq!(
            deployment_selector(&deploy).unwrap(),
            "app=api,track notin (canary,shadow),tier,!legacy"
        );
    }

    #[test]
    fn an_empty_deployment_selector_is_refused() {
        let text = deployment_selector(&deployment(LabelSelector::default())).unwrap_err();
        assert!(text.contains("empty selector"), "{text}");
    }

    #[test]
    fn no_pods_behind_a_deployment_suggests_it_is_scaled_to_zero() {
        let target = Target::parse("deploy/api").unwrap();
        let text = none_ready(&target, "app=api", &[], now());
        assert!(
            text.contains("no pods match deployment api's selector (app=api)"),
            "{text}"
        );
        assert!(text.contains("scaled to zero"), "{text}");
    }

    #[test]
    fn unready_pods_behind_a_service_are_listed_with_their_status() {
        let pods = [
            pod("api-a", "Pending", false, EARLY),
            pod("api-b", "Running", false, LATE),
        ];
        let text = none_ready(&svc_target(), "app=api", &pods, now());
        assert!(
            text.starts_with("none of the 2 pods behind svc/api is ready"),
            "{text}"
        );
        assert!(text.contains("api-a"), "{text}");
        assert!(text.contains("Pending"), "{text}");
        assert!(text.contains("`eks pods -l app=api`"), "{text}");
    }

    fn services(names: &[&str]) -> Vec<Service> {
        names
            .iter()
            .map(|name| Service {
                metadata: ObjectMeta {
                    name: Some((*name).to_owned()),
                    ..ObjectMeta::default()
                },
                ..Service::default()
            })
            .collect()
    }

    #[test]
    fn a_prefix_finds_a_service_and_an_exact_name_beats_a_longer_one() {
        let items = services(&["api", "api-canary", "web"]);
        assert!(matches!(find(&items, "we"), Found::One(s) if s.name_any() == "web"));
        assert!(matches!(find(&items, "api"), Found::One(s) if s.name_any() == "api"));
        assert!(matches!(find(&items, "api-"), Found::One(s) if s.name_any() == "api-canary"));
        assert!(matches!(find(&items, "a"), Found::Several(found) if found.len() == 2));
        assert!(matches!(find(&items, "db"), Found::None));
        assert!(matches!(find(&items, ""), Found::None));
        assert!(matches!(find::<Service>(&[], "api"), Found::None));
    }

    #[test]
    fn an_ambiguous_prefix_lists_the_names_with_an_example() {
        let text = ambiguous(Kind::Service, "a", &["api".to_owned(), "auth".to_owned()]);
        assert!(
            text.starts_with("2 services start with \"a\": api and auth."),
            "{text}"
        );
        assert!(text.contains("`svc/api`"), "{text}");
    }

    #[test]
    fn not_found_lists_what_the_namespace_has() {
        let text = not_found(
            Kind::Deployment,
            "db",
            "default",
            &["api".to_owned(), "web".to_owned()],
            &[],
        );
        assert!(
            text.contains("no deployment in namespace default is called \"db\""),
            "{text}"
        );
        assert!(
            text.contains("The deployments there are api and web."),
            "{text}"
        );
    }

    #[test]
    fn not_found_in_an_empty_namespace_says_it_is_empty() {
        let text = not_found(Kind::Service, "db", "default", &[], &[]);
        assert!(
            text.contains("no services in namespace default at all"),
            "{text}"
        );
    }

    #[test]
    fn not_found_names_the_namespace_that_has_it() {
        let text = not_found(Kind::Service, "db", "default", &[], &["data".to_owned()]);
        assert!(text.contains("pass `-n data`"), "{text}");
        let text = not_found(
            Kind::Service,
            "db",
            "default",
            &[],
            &["data".to_owned(), "legacy".to_owned()],
        );
        assert!(
            text.contains("Namespaces data and legacy have one"),
            "{text}"
        );
    }

    #[test]
    fn a_long_list_of_names_is_cut_short() {
        let names: Vec<String> = (1..=11).map(|i| format!("svc-{i}")).collect();
        let text = not_found(Kind::Service, "db", "default", &names, &[]);
        assert!(text.contains("svc-8, and 3 more."), "{text}");
        assert!(!text.contains("svc-9"), "{text}");
    }
}
