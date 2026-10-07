//! Turning a control-plane log event into the line `eks control-plane-logs`
//! prints.
//!
//! An audit event is a kilobyte or two of JSON per request, and the question
//! somebody brings to it — who deleted my pod? — is answered by five of its
//! fields. [`summarise`] reads those five; [`line()`] prints them as
//!
//! ```text
//! 2026-10-07T06:21:02Z  Admin/alice  delete  pods shop/api-7f9c  200
//! ```
//!
//! with the user's ARN shortened to its role and session, and the response
//! code inked as a severity when it is a refusal or a server error. The other
//! four types are text the control-plane component wrote, and print as it
//! wrote them behind the CloudWatch timestamp. `--json` prints every event
//! whole, an audit event's message as the object it is ([`json_line`]).
//!
//! An event that does not read as an audit event — truncated, or something
//! EKS wrote into the stream that is not one — is printed as it came rather
//! than dropped. Losing the one line that mattered because it was unusual is
//! the failure that would make this worse than the raw log.

use serde::{Deserialize, Serialize};

use crate::aws::cli::short_principal;
use crate::aws::logs::{Event, LogType, event_time};
use crate::theme::{Palette, Severity};

/// The five things an audit line says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// Who, with any impersonation spelled out: `alice as system:admin`.
    pub user: String,
    /// The Kubernetes verb, `delete`.
    pub verb: String,
    /// What it was done to: `pods shop/api-7f9c`, or the request path for a
    /// request that is not about an object.
    pub resource: String,
    /// The response code, absent for an event logged before there was one.
    pub code: Option<u16>,
}

#[derive(Debug, Deserialize)]
struct AuditEvent {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    verb: Option<String>,
    #[serde(default)]
    user: Option<User>,
    #[serde(rename = "impersonatedUser", default)]
    impersonated: Option<User>,
    #[serde(rename = "objectRef", default)]
    object: Option<ObjectRef>,
    #[serde(rename = "requestURI", default)]
    uri: Option<String>,
    #[serde(rename = "responseStatus", default)]
    status: Option<Status>,
}

#[derive(Debug, Deserialize)]
struct User {
    #[serde(default)]
    username: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ObjectRef {
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    subresource: Option<String>,
    #[serde(default)]
    namespace: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(rename = "apiGroup", default)]
    group: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Status {
    #[serde(default)]
    code: Option<u16>,
}

/// Read an audit event's message, or `None` if it is not one.
#[must_use]
pub fn summarise(message: &str) -> Option<Summary> {
    let event: AuditEvent = serde_json::from_str(message).ok()?;
    if event.kind.as_deref() != Some("Event") {
        return None;
    }

    let name = |user: Option<User>| {
        user.and_then(|user| user.username)
            .filter(|name| !name.is_empty())
            .map(|name| short_principal(&name))
    };
    let mut user = name(event.user).unwrap_or_else(|| "-".to_owned());
    if let Some(as_whom) = name(event.impersonated) {
        user = format!("{user} as {as_whom}");
    }

    let resource = event
        .object
        .and_then(object)
        .or(event.uri)
        .unwrap_or_else(|| "-".to_owned());

    Some(Summary {
        user,
        verb: event.verb.unwrap_or_else(|| "-".to_owned()),
        resource,
        code: event.status.and_then(|status| status.code),
    })
}

/// `pods shop/api`, `deployments.apps shop/api`, `pods/log shop/api`,
/// `nodes ip-10-0-0-1`, `pods in shop`, or `pods` across the cluster.
fn object(object: ObjectRef) -> Option<String> {
    let mut kind = object.resource.filter(|resource| !resource.is_empty())?;
    if let Some(group) = object.group.filter(|group| !group.is_empty()) {
        kind = format!("{kind}.{group}");
    }
    if let Some(sub) = object.subresource.filter(|sub| !sub.is_empty()) {
        kind = format!("{kind}/{sub}");
    }
    let namespace = object.namespace.filter(|namespace| !namespace.is_empty());
    let name = object.name.filter(|name| !name.is_empty());
    Some(match (namespace, name) {
        (Some(namespace), Some(name)) => format!("{kind} {namespace}/{name}"),
        (None, Some(name)) => format!("{kind} {name}"),
        (Some(namespace), None) => format!("{kind} in {namespace}"),
        (None, None) => kind,
    })
}

/// How alarming a response code is: a refusal is worth a second look, a
/// server error more so.
#[must_use]
pub fn severity(code: Option<u16>) -> Severity {
    match code {
        None => Severity::Unknown,
        Some(500..) => Severity::Critical,
        Some(400..500) => Severity::Warn,
        Some(_) => Severity::Ok,
    }
}

/// The line one event prints as.
#[must_use]
pub fn line(kind: LogType, event: &Event, palette: Palette) -> String {
    let time = event_time(event.timestamp);
    let message = event.message.trim_end();
    if kind == LogType::Audit
        && let Some(summary) = summarise(message)
    {
        let code = summary
            .code
            .map_or_else(|| "-".to_owned(), |code| code.to_string());
        return format!(
            "{time}  {}  {}  {}  {}",
            summary.user,
            summary.verb,
            summary.resource,
            palette.paint(&code, severity(summary.code)),
        );
    }
    if message.is_empty() {
        return time;
    }
    format!("{time}  {message}")
}

/// One event as a line of JSON, for `--json`.
#[derive(Debug, Serialize)]
struct Whole<'a> {
    /// RFC 3339, to the millisecond CloudWatch keeps.
    time: String,
    #[serde(rename = "type")]
    kind: &'static str,
    stream: &'a str,
    id: &'a str,
    /// The audit event as an object; anything else as the text it is.
    message: Message<'a>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Message<'a> {
    Object(serde_json::Value),
    Text(&'a str),
}

/// One event as one line of JSON.
///
/// JSON Lines rather than one document, unlike the listings' `--json`:
/// `--follow` never ends, and a document that never closes is one no tool
/// can read. One spelling for both, so a script written against a bounded
/// read keeps working when `--follow` is added.
pub fn json_line(kind: LogType, event: &Event) -> Result<String, serde_json::Error> {
    let message = event.message.trim_end();
    let parsed = if kind == LogType::Audit {
        serde_json::from_str::<serde_json::Value>(message)
            .ok()
            .filter(serde_json::Value::is_object)
    } else {
        None
    };
    serde_json::to_string(&Whole {
        time: k8s_openapi::jiff::Timestamp::from_millisecond(event.timestamp)
            .map_or_else(|_| event.timestamp.to_string(), |at| format!("{at:.3}")),
        kind: kind.eks_name(),
        stream: &event.stream,
        id: &event.id,
        message: parsed.map_or(Message::Text(message), Message::Object),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::theme::Theme;

    /// A real EKS audit event's shape, trimmed of the fields nobody reads.
    const DELETE: &str = r#"{"kind":"Event","apiVersion":"audit.k8s.io/v1","level":"Metadata","auditID":"5c1f","stage":"ResponseComplete","requestURI":"/api/v1/namespaces/shop/pods/api-7f9c","verb":"delete","user":{"username":"arn:aws:sts::111122223333:assumed-role/Admin/alice","uid":"aws-iam-authenticator:111122223333:AROA","groups":["system:authenticated"],"extra":{"sessionName":["alice"]}},"sourceIPs":["203.0.113.7"],"userAgent":"kubectl/v1.31.0","objectRef":{"resource":"pods","namespace":"shop","name":"api-7f9c","apiVersion":"v1"},"responseStatus":{"metadata":{},"code":200},"requestReceivedTimestamp":"2026-10-07T06:21:02.101Z","stageTimestamp":"2026-10-07T06:21:02.123Z"}"#;

    fn event(message: &str) -> Event {
        Event {
            stream: "kube-apiserver-audit-0a".to_owned(),
            timestamp: 1_791_354_062_123,
            message: message.to_owned(),
            id: "3780".to_owned(),
        }
    }

    #[test]
    fn an_audit_event_reads_as_who_did_what_to_which_object_and_how_it_went() {
        assert_eq!(
            summarise(DELETE),
            Some(Summary {
                user: "Admin/alice".to_owned(),
                verb: "delete".to_owned(),
                resource: "pods shop/api-7f9c".to_owned(),
                code: Some(200),
            })
        );
        assert_eq!(
            line(LogType::Audit, &event(DELETE), Palette::Plain),
            "2026-10-07T06:21:02Z  Admin/alice  delete  pods shop/api-7f9c  200"
        );
    }

    #[test]
    fn a_resource_names_its_group_and_subresource_and_a_listing_its_namespace() {
        let summary = |object: &str| {
            summarise(&format!(
                r#"{{"kind":"Event","verb":"get","user":{{"username":"u"}},"objectRef":{object}}}"#
            ))
            .unwrap()
            .resource
        };

        assert_eq!(
            summary(
                r#"{"resource":"deployments","apiGroup":"apps","namespace":"shop","name":"api"}"#
            ),
            "deployments.apps shop/api"
        );
        assert_eq!(
            summary(r#"{"resource":"pods","subresource":"exec","namespace":"shop","name":"api"}"#),
            "pods/exec shop/api"
        );
        assert_eq!(
            summary(r#"{"resource":"nodes","name":"ip-10-0-0-1"}"#),
            "nodes ip-10-0-0-1"
        );
        assert_eq!(
            summary(r#"{"resource":"pods","namespace":"shop"}"#),
            "pods in shop"
        );
        assert_eq!(summary(r#"{"resource":"pods"}"#), "pods");
    }

    #[test]
    fn a_request_about_no_object_is_named_by_its_path() {
        let summary = summarise(
            r#"{"kind":"Event","verb":"get","requestURI":"/livez","user":{"username":"system:anonymous"},"responseStatus":{"code":403}}"#,
        )
        .unwrap();
        assert_eq!(summary.resource, "/livez");
        assert_eq!(summary.user, "system:anonymous");
        assert_eq!(summary.code, Some(403));
    }

    #[test]
    fn impersonation_names_both_users() {
        let summary = summarise(
            r#"{"kind":"Event","verb":"patch","user":{"username":"arn:aws:iam::111122223333:user/bob"},"impersonatedUser":{"username":"system:admin"},"objectRef":{"resource":"configmaps","namespace":"kube-system","name":"aws-auth"}}"#,
        )
        .unwrap();
        assert_eq!(summary.user, "bob as system:admin");
    }

    #[test]
    fn missing_fields_read_as_dashes_not_as_a_dropped_line() {
        let summary = summarise(r#"{"kind":"Event"}"#).unwrap();
        assert_eq!(
            summary,
            Summary {
                user: "-".to_owned(),
                verb: "-".to_owned(),
                resource: "-".to_owned(),
                code: None,
            }
        );
        assert_eq!(
            line(
                LogType::Audit,
                &event(r#"{"kind":"Event"}"#),
                Palette::Plain
            ),
            "2026-10-07T06:21:02Z  -  -  -  -"
        );
    }

    #[test]
    fn a_message_that_is_not_an_audit_event_prints_as_it_came() {
        for message in ["{truncated", r#"{"kind":"Status"}"#, "plain text", ""] {
            assert_eq!(summarise(message), None, "{message}");
            assert_eq!(
                line(LogType::Audit, &event(message), Palette::Plain),
                format!("2026-10-07T06:21:02Z  {message}").trim_end(),
            );
        }
    }

    #[test]
    fn other_types_print_their_own_text_behind_the_timestamp() {
        let klog =
            "I1007 06:21:02.123456      10 controller.go:615] quota admission added evaluator\n";
        assert_eq!(
            line(LogType::Api, &event(klog), Palette::Plain),
            "2026-10-07T06:21:02Z  I1007 06:21:02.123456      10 controller.go:615] quota admission added evaluator"
        );
        // Even one that happens to be audit-shaped: the type decides.
        assert!(
            line(LogType::Authenticator, &event(DELETE), Palette::Plain).contains("\"auditID\"")
        );
    }

    #[test]
    fn refusals_and_server_errors_are_inked_and_successes_are_not() {
        assert_eq!(severity(Some(200)), Severity::Ok);
        assert_eq!(severity(Some(301)), Severity::Ok);
        assert_eq!(severity(Some(403)), Severity::Warn);
        assert_eq!(severity(Some(503)), Severity::Critical);
        assert_eq!(severity(None), Severity::Unknown);

        let colour = Palette::Colour(Theme::dark());
        let refused = DELETE.replace("\"code\":200", "\"code\":403");
        let painted = line(LogType::Audit, &event(&refused), colour);
        assert!(painted.ends_with(&format!("{}", colour.paint("403", Severity::Warn))));
        assert!(painted.contains("\x1b["), "{painted:?}");
        assert!(!line(LogType::Audit, &event(DELETE), colour).contains("\x1b["));
    }

    #[test]
    fn json_prints_an_audit_event_whole_as_an_object() {
        let line = json_line(LogType::Audit, &event(DELETE)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();

        assert!(!line.contains('\n'));
        assert_eq!(value["time"], "2026-10-07T06:21:02.123Z");
        assert_eq!(value["type"], "audit");
        assert_eq!(value["stream"], "kube-apiserver-audit-0a");
        assert_eq!(value["id"], "3780");
        assert_eq!(value["message"]["auditID"], "5c1f");
        assert_eq!(value["message"]["user"]["extra"]["sessionName"][0], "alice");
    }

    #[test]
    fn json_prints_other_text_as_a_string() {
        let value: serde_json::Value = serde_json::from_str(
            &json_line(LogType::Scheduler, &event("I1007 started\n")).unwrap(),
        )
        .unwrap();
        assert_eq!(value["type"], "scheduler");
        assert_eq!(value["message"], "I1007 started");

        // An audit message that is not JSON is still printed, as text.
        let broken: serde_json::Value =
            serde_json::from_str(&json_line(LogType::Audit, &event("{trunc")).unwrap()).unwrap();
        assert_eq!(broken["message"], "{trunc");
    }
}
