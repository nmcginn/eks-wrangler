//! Parsing and validating the label and field selectors a user types.
//!
//! `eks pods -l app=api` and `eks pods --field-selector status.phase!=Running`
//! push the filtering onto the API server, which is both faster than trimming a
//! full listing here and the only way to filter a listing too large to fetch.
//! The catch is that a selector is a request and not a guarantee: a malformed
//! one is answered with a `400` and a message about parse positions that says
//! nothing about what the user should have typed. So the grammar is
//! reimplemented here as a pure function that rejects a bad selector *before*
//! anything connects, quoting the offending text — the same bargain
//! [`crate::k8s::quantity`] strikes for resource quantities.
//!
//! Two grammars, because Kubernetes has two. A *label* selector is the richer
//! one: equality (`=`, `==`, `!=`), set membership (`in`, `notin`), and bare
//! existence (`key`, `!key`). A *field* selector is equality only, over dotted
//! field paths. Both are comma-separated, and both are re-emitted in a
//! canonical form so the string handed to the API server is one this module
//! vouched for rather than the raw input.

/// A selector that could not be parsed, carrying a sentence a user can act on.
///
/// The offending text is always quoted, so the message points at the part that
/// is wrong rather than repeating the whole selector back.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Error(String);

impl Error {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Validate a label selector and return its canonical form.
///
/// The canonical form is what is sent to the API server: whitespace normalised,
/// `==` folded to `=`, and each requirement re-emitted from the parsed value.
/// An empty or whitespace-only selector is a selector that matches everything,
/// which is returned as the empty string.
///
/// # Errors
/// Returns [`Error`] for anything Kubernetes would reject: an empty requirement
/// (a stray comma), a malformed key or value, a set operator with no
/// parenthesised list, or a `!` existence check with a value glued on.
pub fn label_selector(input: &str) -> Result<String, Error> {
    if input.trim().is_empty() {
        return Ok(String::new());
    }

    let rendered: Result<Vec<String>, Error> = split_requirements(input)
        .into_iter()
        .map(|requirement| parse_label_requirement(requirement).map(|r| r.render()))
        .collect();

    Ok(rendered?.join(","))
}

/// Validate a field selector and return its canonical form.
///
/// Field selectors are equality-only — `key=value`, `key==value`, `key!=value`
/// — over dotted field paths such as `status.phase` or `spec.nodeName`. There
/// is no set membership and no existence check; those are label-selector ideas
/// the API server does not accept here.
///
/// # Errors
/// Returns [`Error`] for an empty requirement, a requirement with no operator,
/// or an empty key.
pub fn field_selector(input: &str) -> Result<String, Error> {
    if input.trim().is_empty() {
        return Ok(String::new());
    }

    let rendered: Result<Vec<String>, Error> =
        input.split(',').map(parse_field_requirement).collect();

    Ok(rendered?.join(","))
}

/// One parsed label requirement, in the vocabulary the API server understands.
enum LabelReq {
    Equal(String, String),
    NotEqual(String, String),
    In(String, Vec<String>),
    NotIn(String, Vec<String>),
    Exists(String),
    DoesNotExist(String),
}

impl LabelReq {
    /// The canonical string for this requirement. Value order is preserved as
    /// the user wrote it, so a round-trip is recognisable rather than sorted.
    fn render(&self) -> String {
        match self {
            Self::Equal(k, v) => format!("{k}={v}"),
            Self::NotEqual(k, v) => format!("{k}!={v}"),
            Self::In(k, vs) => format!("{k} in ({})", vs.join(",")),
            Self::NotIn(k, vs) => format!("{k} notin ({})", vs.join(",")),
            Self::Exists(k) => k.clone(),
            Self::DoesNotExist(k) => format!("!{k}"),
        }
    }
}

/// Split a selector into its requirements on top-level commas, leaving the
/// commas inside a `(value,list)` alone.
fn split_requirements(input: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;

    for (index, ch) in input.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&input[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&input[start..]);
    parts
}

fn parse_label_requirement(requirement: &str) -> Result<LabelReq, Error> {
    let trimmed = requirement.trim();
    if trimmed.is_empty() {
        return Err(Error::new(
            "the label selector has an empty requirement — check for a stray or trailing comma",
        ));
    }

    // A parenthesis anywhere makes this set-based (`key in (a,b)`); nothing else
    // in the grammar uses one, so its presence settles which branch we are in.
    if trimmed.contains('(') {
        return parse_set_requirement(trimmed);
    }

    // Equality binds before existence: the first `=` in a requirement is always
    // part of its operator, because neither keys nor values may contain one.
    if let Some(op) = find_equality_operator(trimmed) {
        let (key, value) = trimmed.split_at(op.at);
        let value = &value[op.width..];
        let key = valid_label_key(key.trim(), trimmed)?;
        let value = valid_label_value(value.trim(), trimmed)?;
        return Ok(if op.negated {
            LabelReq::NotEqual(key, value)
        } else {
            LabelReq::Equal(key, value)
        });
    }

    // No operator: an existence check. `!key` is "must not exist", a bare key is
    // "must exist".
    if let Some(rest) = trimmed.strip_prefix('!') {
        let key = valid_label_key(rest.trim(), trimmed)?;
        return Ok(LabelReq::DoesNotExist(key));
    }

    // A bare key with an inner space is almost always a set operator whose value
    // list was forgotten (`app in`), so say that rather than "invalid key".
    if let Some((_, word)) = trimmed.split_once(char::is_whitespace) {
        let word = word.trim_start();
        if word == "in" || word.starts_with("in ") || word == "notin" || word.starts_with("notin ")
        {
            return Err(Error::new(format!(
                "the set selector {trimmed:?} is missing its parenthesised value list, e.g. `{} in (a, b)`",
                trimmed.split_whitespace().next().unwrap_or(trimmed)
            )));
        }
    }

    let key = valid_label_key(trimmed, trimmed)?;
    Ok(LabelReq::Exists(key))
}

fn parse_set_requirement(requirement: &str) -> Result<LabelReq, Error> {
    let open = requirement.find('(').unwrap_or(requirement.len());
    let head = requirement[..open].trim();
    let tail = requirement[open..].trim_end();

    let Some(values) = tail.strip_prefix('(').and_then(|s| s.strip_suffix(')')) else {
        return Err(Error::new(format!(
            "the set selector {requirement:?} is missing its closing `)`"
        )));
    };

    // `key in` / `key notin` — one key, one operator word, nothing else.
    let mut words = head.split_whitespace();
    let (Some(key), Some(op), None) = (words.next(), words.next(), words.next()) else {
        return Err(Error::new(format!(
            "the set selector {requirement:?} must read `key in (…)` or `key notin (…)`"
        )));
    };

    if key.starts_with('!') {
        return Err(Error::new(format!(
            "the set selector {requirement:?} cannot be negated with `!`; use `notin` instead"
        )));
    }
    let key = valid_label_key(key, requirement)?;

    // `key in ()`, `key in (a,)`, and `key in ( )` all leave an empty value the
    // API server rejects, so they are rejected here where the reason can be
    // named rather than surfacing as a `400`.
    let values = values.split(',').map(str::trim).collect::<Vec<_>>();
    if values.iter().any(|value| value.is_empty()) {
        return Err(Error::new(format!(
            "the set selector {requirement:?} has an empty value between its parentheses"
        )));
    }
    let values = values
        .into_iter()
        .map(|value| valid_label_value(value, requirement))
        .collect::<Result<Vec<_>, _>>()?;

    match op {
        "in" => Ok(LabelReq::In(key, values)),
        "notin" => Ok(LabelReq::NotIn(key, values)),
        other => Err(Error::new(format!(
            "{other:?} is not a set operator in {requirement:?}; use `in` or `notin`"
        ))),
    }
}

fn parse_field_requirement(requirement: &str) -> Result<String, Error> {
    let trimmed = requirement.trim();
    if trimmed.is_empty() {
        return Err(Error::new(
            "the field selector has an empty requirement — check for a stray or trailing comma",
        ));
    }

    let Some(op) = find_equality_operator(trimmed) else {
        return Err(Error::new(format!(
            "the field selector {trimmed:?} needs an operator: `=`, `==`, or `!=`, e.g. `status.phase=Running`"
        )));
    };

    let (key, value) = trimmed.split_at(op.at);
    let value = value[op.width..].trim();
    let key = key.trim();
    if key.is_empty() {
        return Err(Error::new(format!(
            "the field selector {trimmed:?} has no field name before its operator"
        )));
    }

    // Field values may be empty (`metadata.namespace=`), and field paths are not
    // constrained the way label keys are, so structure is all there is to check.
    Ok(if op.negated {
        format!("{key}!={value}")
    } else {
        format!("{key}={value}")
    })
}

/// Where an equality operator sits in a requirement, and which one it is.
struct Operator {
    /// Byte offset of the operator's first character.
    at: usize,
    /// Its length in bytes: 1 for `=`, 2 for `==` and `!=`.
    width: usize,
    negated: bool,
}

/// Find the operator that splits a requirement into key and value.
///
/// The first `=` decides it: `!=` when a `!` immediately precedes it, `==`
/// folded to a plain equal when another `=` immediately follows, otherwise `=`.
fn find_equality_operator(requirement: &str) -> Option<Operator> {
    let eq = requirement.find('=')?;
    let bytes = requirement.as_bytes();

    if eq > 0 && bytes[eq - 1] == b'!' {
        return Some(Operator {
            at: eq - 1,
            width: 2,
            negated: true,
        });
    }
    if bytes.get(eq + 1) == Some(&b'=') {
        return Some(Operator {
            at: eq,
            width: 2,
            negated: false,
        });
    }
    Some(Operator {
        at: eq,
        width: 1,
        negated: false,
    })
}

/// Validate a label key — an optional DNS-subdomain prefix and a name — and
/// return it unchanged. `requirement` is the surrounding text, quoted in errors
/// so the message points somewhere.
fn valid_label_key(key: &str, requirement: &str) -> Result<String, Error> {
    if key.is_empty() {
        return Err(Error::new(format!(
            "a requirement in {requirement:?} has no label key"
        )));
    }

    let (prefix, name) = match key.split_once('/') {
        Some((prefix, name)) => (Some(prefix), name),
        None => (None, key),
    };

    if let Some(prefix) = prefix
        && (key.matches('/').count() > 1 || !is_dns_subdomain(prefix))
    {
        return Err(Error::new(format!(
            "the label key {key:?} in {requirement:?} has an invalid prefix before its `/`"
        )));
    }

    if !is_qualified_name(name) {
        return Err(Error::new(format!(
            "the label key {key:?} in {requirement:?} is not a valid Kubernetes label name"
        )));
    }

    Ok(key.to_owned())
}

/// Validate a label value and return it unchanged. Empty is allowed — `key=`
/// selects the pods carrying `key` with an empty value.
fn valid_label_value(value: &str, requirement: &str) -> Result<String, Error> {
    if value.is_empty() || is_qualified_name(value) {
        return Ok(value.to_owned());
    }
    Err(Error::new(format!(
        "the value {value:?} in {requirement:?} is not a valid Kubernetes label value"
    )))
}

/// A qualified-name component: 1–63 chars, alphanumeric at each end, and
/// `[-_.]` allowed in between. This is Kubernetes' rule for both a label's name
/// part and a label value.
fn is_qualified_name(text: &str) -> bool {
    if text.is_empty() || text.len() > 63 {
        return false;
    }
    let alnum = |c: char| c.is_ascii_alphanumeric();
    let inner = |c: char| alnum(c) || matches!(c, '-' | '_' | '.');
    let mut chars = text.chars();
    let first = chars.next().is_some_and(alnum);
    let last = text.chars().next_back().is_some_and(alnum);
    first && last && text.chars().all(inner)
}

/// A DNS-1123 subdomain: dot-separated labels, each 1–63 chars of lowercase
/// alphanumerics and hyphens, alphanumeric at the ends, 253 chars overall. This
/// is the rule for the optional prefix before a label key's `/`.
fn is_dns_subdomain(text: &str) -> bool {
    if text.is_empty() || text.len() > 253 {
        return false;
    }
    text.split('.').all(is_dns_label)
}

fn is_dns_label(label: &str) -> bool {
    if label.is_empty() || label.len() > 63 {
        return false;
    }
    let alnum = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    let inner = |c: char| alnum(c) || c == '-';
    let first = label.chars().next().is_some_and(alnum);
    let last = label.chars().next_back().is_some_and(alnum);
    first && last && label.chars().all(inner)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn labels_ok(input: &str) -> String {
        label_selector(input).expect("selector should be valid")
    }

    fn labels_err(input: &str) -> String {
        label_selector(input)
            .expect_err("selector should be rejected")
            .to_string()
    }

    fn fields_ok(input: &str) -> String {
        field_selector(input).expect("selector should be valid")
    }

    fn fields_err(input: &str) -> String {
        field_selector(input)
            .expect_err("selector should be rejected")
            .to_string()
    }

    #[test]
    fn an_empty_selector_matches_everything() {
        assert_eq!(labels_ok(""), "");
        assert_eq!(labels_ok("   "), "");
        assert_eq!(fields_ok(""), "");
    }

    #[test]
    fn equality_is_the_common_case() {
        assert_eq!(labels_ok("app=api"), "app=api");
        assert_eq!(labels_ok("app!=api"), "app!=api");
    }

    #[test]
    fn double_equals_is_folded_to_a_single_one() {
        // `==` and `=` mean the same thing to Kubernetes; the canonical form
        // picks one so two spellings of one selector do not look different.
        assert_eq!(labels_ok("app==api"), "app=api");
    }

    #[test]
    fn surrounding_whitespace_is_normalised_away() {
        assert_eq!(
            labels_ok("  app = api ,  tier != canary "),
            "app=api,tier!=canary"
        );
    }

    #[test]
    fn an_empty_value_is_a_real_selector() {
        // `key=` matches pods that carry the label with an empty value, and is
        // different from the key not being present at all.
        assert_eq!(labels_ok("track="), "track=");
    }

    #[test]
    fn existence_and_absence_need_no_value() {
        assert_eq!(labels_ok("app"), "app");
        assert_eq!(labels_ok("!app"), "!app");
    }

    #[test]
    fn set_membership_keeps_its_values_in_order() {
        assert_eq!(labels_ok("env in (prod, staging)"), "env in (prod,staging)");
        assert_eq!(labels_ok("env notin (dev,test)"), "env notin (dev,test)");
    }

    #[test]
    fn a_prefixed_label_key_is_allowed() {
        assert_eq!(
            labels_ok("app.kubernetes.io/name=api"),
            "app.kubernetes.io/name=api"
        );
    }

    #[test]
    fn several_requirements_join_with_commas() {
        assert_eq!(
            labels_ok("app=api, tier in (web, api), !canary"),
            "app=api,tier in (web,api),!canary"
        );
    }

    #[test]
    fn a_trailing_comma_is_rejected_as_an_empty_requirement() {
        let message = labels_err("app=api,");
        assert!(message.contains("empty requirement"), "{message}");
        assert!(message.contains("comma"), "{message}");
    }

    #[test]
    fn a_set_operator_without_values_names_the_selector() {
        let message = labels_err("env in");
        assert!(message.contains("\"env in\""), "{message}");
        assert!(message.contains("value list"), "{message}");
    }

    #[test]
    fn an_unclosed_value_list_says_so() {
        let message = labels_err("env in (prod");
        assert!(message.contains("closing"), "{message}");
        assert!(message.contains("\"env in (prod\""), "{message}");
    }

    #[test]
    fn empty_parentheses_are_rejected() {
        let message = labels_err("env in ()");
        assert!(message.contains("empty value"), "{message}");
    }

    #[test]
    fn a_dangling_value_in_a_set_is_rejected() {
        // `env in (prod,)` — kubectl rejects the empty slot after the comma.
        let message = labels_err("env in (prod,)");
        assert!(message.contains("empty value"), "{message}");
    }

    #[test]
    fn a_negated_set_selector_is_pointed_at_notin() {
        let message = labels_err("!env in (prod)");
        assert!(message.contains("notin"), "{message}");
    }

    #[test]
    fn a_bad_set_operator_is_named() {
        let message = labels_err("env within (prod)");
        assert!(
            message.contains("in") && message.contains("notin"),
            "{message}"
        );
    }

    #[test]
    fn an_invalid_label_key_is_rejected_with_the_key_quoted() {
        let message = labels_err("-bad=api");
        assert!(message.contains("\"-bad\""), "{message}");
        assert!(message.contains("label name"), "{message}");
    }

    #[test]
    fn an_invalid_label_value_is_rejected_with_the_value_quoted() {
        // A value cannot contain a `/`, so this is caught locally rather than by
        // the API server.
        let message = labels_err("app=api/v2");
        assert!(message.contains("\"api/v2\""), "{message}");
        assert!(message.contains("label value"), "{message}");
    }

    #[test]
    fn an_over_long_label_value_is_rejected() {
        let long = "a".repeat(64);
        let message = labels_err(&format!("app={long}"));
        assert!(message.contains("label value"), "{message}");
    }

    #[test]
    fn a_bad_prefix_on_a_label_key_is_rejected() {
        let message = labels_err("UPPER.io/name=api");
        assert!(message.contains("prefix"), "{message}");
    }

    #[test]
    fn field_equality_is_supported_all_three_ways() {
        assert_eq!(fields_ok("status.phase=Running"), "status.phase=Running");
        assert_eq!(fields_ok("status.phase==Running"), "status.phase=Running");
        assert_eq!(fields_ok("status.phase!=Running"), "status.phase!=Running");
    }

    #[test]
    fn the_not_running_filter_from_the_roadmap_parses() {
        // "only the ones that are not Running" is the second thing anyone asks
        // for; it is a field selector, and it must survive round-tripping.
        assert_eq!(
            fields_ok(" status.phase != Running "),
            "status.phase!=Running"
        );
    }

    #[test]
    fn several_field_requirements_join_with_commas() {
        assert_eq!(
            fields_ok("status.phase=Running,spec.nodeName=ip-10-0-1-9"),
            "status.phase=Running,spec.nodeName=ip-10-0-1-9"
        );
    }

    #[test]
    fn a_field_value_may_be_empty() {
        assert_eq!(fields_ok("spec.nodeName="), "spec.nodeName=");
    }

    #[test]
    fn a_field_requirement_with_no_operator_is_rejected() {
        let message = fields_err("status.phase");
        assert!(message.contains("\"status.phase\""), "{message}");
        assert!(message.contains("operator"), "{message}");
    }

    #[test]
    fn a_field_requirement_with_no_key_is_rejected() {
        let message = fields_err("=Running");
        assert!(message.contains("no field name"), "{message}");
    }

    #[test]
    fn a_trailing_comma_in_a_field_selector_is_rejected() {
        let message = fields_err("status.phase=Running,");
        assert!(message.contains("empty requirement"), "{message}");
    }
}
