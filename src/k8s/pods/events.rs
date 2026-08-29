//! Events recorded against one pod, reduced to rows for the pod-detail pane.
//!
//! `kubectl describe pod` ends with the events the API server recorded
//! against it — `FailedScheduling`, `BackOff`, `Pulled` — and they are
//! frequently the only account of *why* a pod is in the state the rest of the
//! view already describes. An event is not scoped to a pod the way everything
//! else in [`super`] is: it is fetched by listing a namespace's events and
//! filtering by `involvedObject`, via [`fetch`], and a cluster — or an event
//! source older than server-side `EventSeries` batching — can repeat the same
//! complaint as several distinct objects rather than incrementing one. This
//! module collapses those the way `kubectl` reads them: one row per `(reason,
//! message)`, carrying the total count and the most recent occurrence.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::Event;
use k8s_openapi::jiff::Timestamp;
use kube::Client;
use kube::api::{Api, ListParams};

use crate::format;
use crate::k8s::page;

/// Kubernetes' own default retention for the events API —
/// `kube-controller-manager`'s `--event-ttl`, an hour unless a cluster
/// operator overrode it. Nothing this tool reads reports the running value,
/// so this is a documented assumption rather than a measured fact; see
/// [`empty_note`].
const RETENTION_SECS: i64 = 3600;

/// One kind of event a pod has produced, after collapsing repeats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRow {
    pub reason: String,
    pub message: String,
    /// `type: Warning` in the API; anything else — almost always `Normal` —
    /// reads as unremarkable, the same distinction `ui::logs::LogsState::
    /// Unavailable` draws between information and a failure.
    pub warning: bool,
    /// The total occurrences collapsed into this row: an `EventSeries`'s own
    /// count where the server batched them, the legacy `count` field where it
    /// did not, or `1` for an event with neither.
    pub count: i64,
    /// The most recent occurrence, rounded the way [`format::human_duration`]
    /// rounds every other age in this tool. `None` only for an event with no
    /// timestamp of any kind, which the API server does not actually produce
    /// but which nothing here should assume either.
    pub last_seen_age: Option<String>,
    /// The instant [`Self::last_seen_age`] was rounded from, carried alongside
    /// it the way [`super::row::PodRow::last_restart`] sits beside its own
    /// rounded `restart_age` — kept for [`from_events`]'s own sort rather than
    /// read back by anything downstream today.
    pub last_seen: Option<Timestamp>,
}

/// The field selector for one pod's own events.
///
/// Namespaced on both ends when [`fetch`] uses this: [`Api::namespaced`]
/// already scopes the listing to `namespace`, and naming it again here is
/// belt and braces against a same-named pod in another namespace, since
/// `involvedObject.namespace` is an indexed field precisely so a query like
/// this one does not have to scan every event in the cluster. `pod` and
/// `namespace` are safe to interpolate unquoted: both come from an object the
/// API server itself already returned a name for, and a Kubernetes name
/// cannot contain the characters the field-selector grammar treats specially.
fn field_selector(namespace: &str, pod: &str) -> String {
    format!("involvedObject.name={pod},involvedObject.namespace={namespace}")
}

/// Ask the API server for one pod's own events.
pub async fn fetch(
    client: Client,
    namespace: &str,
    pod: &str,
    budget: page::Budget,
) -> Result<Vec<Event>, page::Error> {
    let api: Api<Event> = Api::namespaced(client, namespace);
    let params = ListParams::default().fields(&field_selector(namespace, pod));
    page::collect(&api, &params, budget).await
}

/// Group a pod's events the way `kubectl describe pod` presents them: one row
/// per `(reason, message)`, with the count and the latest occurrence folded
/// across every object that matched.
#[must_use]
pub fn from_events(events: &[Event], now: Timestamp) -> Vec<EventRow> {
    let mut grouped: BTreeMap<(String, String), (bool, i64, Option<Timestamp>)> = BTreeMap::new();

    for event in events {
        let reason = event.reason.clone().unwrap_or_default();
        let message = event.message.clone().unwrap_or_default();
        let warning = event.type_.as_deref() == Some("Warning");
        let count = i64::from(
            event
                .series
                .as_ref()
                .and_then(|series| series.count)
                .or(event.count)
                .unwrap_or(1),
        );
        let seen = last_seen(event);

        let entry = grouped.entry((reason, message)).or_insert((false, 0, None));
        entry.0 |= warning;
        entry.1 = entry.1.saturating_add(count);
        entry.2 = newest(entry.2, seen);
    }

    let mut rows: Vec<EventRow> = grouped
        .into_iter()
        .map(
            |((reason, message), (warning, count, last_seen))| EventRow {
                reason,
                message,
                warning,
                count,
                last_seen_age: last_seen.map(|at| format::human_duration(now.duration_since(at))),
                last_seen,
            },
        )
        .collect();

    // Most recent first, matching `kubectl describe pod`'s own order — the
    // question a reader brings to this list is "what happened most
    // recently", not the alphabet. An event with no timestamp at all sorts
    // last rather than first, the tail rule every other ordering in this
    // tool gives a row it cannot rank.
    rows.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
    rows
}

/// `kubectl`'s own precedence for "when did this last happen": a series'
/// batched occurrences first, then the legacy single-object fields, in the
/// order the API server itself prefers them.
fn last_seen(event: &Event) -> Option<Timestamp> {
    event
        .series
        .as_ref()
        .and_then(|series| series.last_observed_time.as_ref())
        .map(|time| time.0)
        .or_else(|| event.last_timestamp.as_ref().map(|time| time.0))
        .or_else(|| event.event_time.as_ref().map(|time| time.0))
        .or_else(|| {
            event
                .metadata
                .creation_timestamp
                .as_ref()
                .map(|time| time.0)
        })
}

/// The later of two moments, tolerating either being absent — [`super::row`]'s
/// own `newest`, copied rather than shared: that one folds over a walk of
/// container statuses, this one over a map's values, and the two have never
/// needed to agree on more than the name.
fn newest(current: Option<Timestamp>, candidate: Option<Timestamp>) -> Option<Timestamp> {
    match (current, candidate) {
        (Some(current), Some(candidate)) => Some(current.max(candidate)),
        (current, candidate) => current.or(candidate),
    }
}

/// What to say about a pod with no events at all.
///
/// An empty [`from_events`] is not itself a failure, but "nothing has
/// happened" and "something happened and expired" are different claims, and
/// the API server's retention (`RETENTION_SECS`) means only one of them is
/// provable from an empty list. A pod younger than that window cannot have
/// lost anything to expiry — an empty list there really does mean nothing has
/// happened yet. An older pod with no events might have had something happen
/// and age out before anyone looked; the honest sentence says so rather than
/// the reassuring one.
#[must_use]
pub fn empty_note(pod_created_at: Option<Timestamp>, now: Timestamp) -> String {
    if let Some(created) = pod_created_at {
        let age = now.duration_since(created);
        if age.as_secs() < RETENTION_SECS {
            return format!(
                "No events yet — this pod is only {} old, and the API server has had nothing \
                 to record against it.",
                format::human_duration(age)
            );
        }
    }

    "No events in the last hour. The API server only keeps them for about that long, so an \
     older pod may simply have nothing left to show."
        .to_owned()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use k8s_openapi::api::core::v1::{EventSeries, ObjectReference};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta, Time};
    use k8s_openapi::jiff::SignedDuration;

    use super::*;

    fn now() -> Timestamp {
        "2024-01-01T00:00:00Z".parse().unwrap()
    }

    fn ago(minutes: i64) -> Timestamp {
        now() - SignedDuration::from_mins(minutes)
    }

    fn event(reason: &str, message: &str, event_type: &str) -> Event {
        Event {
            involved_object: ObjectReference::default(),
            reason: Some(reason.to_owned()),
            message: Some(message.to_owned()),
            type_: Some(event_type.to_owned()),
            ..Default::default()
        }
    }

    #[test]
    fn the_field_selector_names_both_the_pod_and_its_namespace() {
        assert_eq!(
            field_selector("payments", "api-7c9"),
            "involvedObject.name=api-7c9,involvedObject.namespace=payments"
        );
    }

    #[test]
    fn a_single_event_becomes_one_row_with_a_count_of_one() {
        let mut created = event("Pulled", "Successfully pulled image", "Normal");
        created.last_timestamp = Some(Time(ago(5)));

        let rows = from_events(&[created], now());

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].reason, "Pulled");
        assert_eq!(rows[0].message, "Successfully pulled image");
        assert!(!rows[0].warning);
        assert_eq!(rows[0].count, 1);
        assert_eq!(rows[0].last_seen_age.as_deref(), Some("5m"));
    }

    #[test]
    fn a_warning_event_is_distinguished_from_a_normal_one() {
        let rows = from_events(&[event("BackOff", "restarting", "Warning")], now());
        assert!(rows[0].warning);
    }

    #[test]
    fn repeated_events_with_the_same_reason_and_message_collapse_into_one_row() {
        // The same complaint recorded as three separate objects — an older
        // event source, or a controller that never adopted `EventSeries` —
        // reads as one row with the total count, exactly as `kubectl describe
        // pod` shows a single object whose own `count` incremented three
        // times.
        let mut first = event("BackOff", "Back-off restarting failed container", "Warning");
        first.last_timestamp = Some(Time(ago(20)));
        let mut second = event("BackOff", "Back-off restarting failed container", "Warning");
        second.last_timestamp = Some(Time(ago(10)));
        let mut third = event("BackOff", "Back-off restarting failed container", "Warning");
        third.last_timestamp = Some(Time(ago(2)));

        let rows = from_events(&[first, second, third], now());

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].count, 3);
        assert_eq!(rows[0].last_seen_age.as_deref(), Some("2m"));
    }

    #[test]
    fn an_events_own_series_count_is_used_instead_of_one_per_object() {
        let mut repeated = event("BackOff", "restarting", "Warning");
        repeated.series = Some(EventSeries {
            count: Some(12),
            last_observed_time: Some(MicroTime(ago(3))),
        });

        let rows = from_events(&[repeated], now());
        assert_eq!(rows[0].count, 12);
        assert_eq!(rows[0].last_seen_age.as_deref(), Some("3m"));
    }

    #[test]
    fn a_series_last_observed_time_wins_over_the_legacy_last_timestamp() {
        let mut repeated = event("BackOff", "restarting", "Warning");
        repeated.last_timestamp = Some(Time(ago(20)));
        repeated.series = Some(EventSeries {
            count: Some(2),
            last_observed_time: Some(MicroTime(ago(3))),
        });

        let rows = from_events(&[repeated], now());
        assert_eq!(rows[0].last_seen_age.as_deref(), Some("3m"));
    }

    #[test]
    fn the_creation_timestamp_is_the_last_resort_for_when_an_event_happened() {
        let mut dated = event("Scheduled", "assigned to a node", "Normal");
        dated.metadata = ObjectMeta {
            creation_timestamp: Some(Time(ago(30))),
            ..Default::default()
        };

        let rows = from_events(&[dated], now());
        assert_eq!(rows[0].last_seen_age.as_deref(), Some("30m"));
    }

    #[test]
    fn an_event_with_no_timestamp_at_all_has_no_age_rather_than_a_guess() {
        let rows = from_events(&[event("Pulled", "pulled image", "Normal")], now());
        assert_eq!(rows[0].last_seen_age, None);
    }

    #[test]
    fn rows_are_sorted_most_recently_seen_first() {
        let mut old = event("Scheduled", "assigned", "Normal");
        old.last_timestamp = Some(Time(ago(30)));
        let mut recent = event("Pulled", "pulled image", "Normal");
        recent.last_timestamp = Some(Time(ago(2)));

        let rows = from_events(&[old, recent], now());

        assert_eq!(rows[0].reason, "Pulled");
        assert_eq!(rows[1].reason, "Scheduled");
    }

    #[test]
    fn an_event_with_no_timestamp_sorts_after_every_dated_one() {
        let mut dated = event("Scheduled", "assigned", "Normal");
        dated.last_timestamp = Some(Time(ago(30)));
        let undated = event("Pulled", "pulled image", "Normal");

        let rows = from_events(&[undated, dated], now());

        assert_eq!(rows[0].reason, "Scheduled");
        assert_eq!(rows[1].reason, "Pulled");
    }

    #[test]
    fn an_empty_listing_produces_no_rows() {
        assert!(from_events(&[], now()).is_empty());
    }

    #[test]
    fn empty_note_reads_confidently_for_a_pod_younger_than_the_retention_window() {
        let note = empty_note(Some(ago(5)), now());
        assert!(note.contains("only 5m old"), "{note}");
        assert!(!note.contains("last hour"), "{note}");
    }

    #[test]
    fn empty_note_hedges_for_a_pod_older_than_the_retention_window() {
        let note = empty_note(Some(ago(120)), now());
        assert!(note.contains("last hour"), "{note}");
        assert!(note.contains("may simply have nothing left"), "{note}");
    }

    #[test]
    fn empty_note_hedges_when_the_pod_has_no_creation_timestamp_either() {
        // A hand-crafted object with no `creationTimestamp` should not be
        // misread as a brand-new pod — the API server always sets this, so
        // its absence is not something to be confident about either way.
        let note = empty_note(None, now());
        assert!(note.contains("last hour"), "{note}");
    }
}
