//! Output checks: equals, contains (and the -any / -all / case-insensitive
//! forms), regex, starts-with, is-json and json-schema.

use serde_json::Value as Json;

use super::{AssertionRegistry, Outcome, brief, only_keys, sync_assertion, transform_of};
use crate::config::AssertionSpec;
use crate::error::EvalError;

pub(super) fn register(registry: &mut AssertionRegistry) {
    registry.register("equals", equals);
    for (kind, mode) in [
        ("contains", Contains::One),
        ("icontains", Contains::One),
        ("contains-any", Contains::Any),
        ("icontains-any", Contains::Any),
        ("contains-all", Contains::All),
        ("icontains-all", Contains::All),
    ] {
        registry.register(kind, move |spec| contains(spec, mode, kind.starts_with('i')));
    }
    registry.register("starts-with", starts_with);
    registry.register("regex", regex);
    registry.register("is-json", |spec| json_check(spec, false));
    registry.register("json-schema", |spec| json_check(spec, true));
}

/// The subject as text: strings as is, other values as compact JSON.
fn as_text(value: &Json) -> String {
    match value {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The subject as JSON: strings holding JSON are parsed.
fn as_json(value: &Json) -> Option<Json> {
    match value {
        Json::String(s) => serde_json::from_str(s.trim()).ok(),
        other => Some(other.clone()),
    }
}

/// JSON equality where numbers compare by value (`1 == 1.0`).
fn json_eq(a: &Json, b: &Json) -> bool {
    match (a, b) {
        (Json::Number(x), Json::Number(y)) => x.as_f64() == y.as_f64(),
        (Json::Array(x), Json::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(a, b)| json_eq(a, b)),
        (Json::Object(x), Json::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| json_eq(v, w)))
        }
        _ => a == b,
    }
}

fn required_value(spec: &AssertionSpec) -> Result<Json, EvalError> {
    spec.value.clone().ok_or_else(|| EvalError::invalid("`value` is required"))
}

fn missing(path: Option<&crate::path::JsonPath>) -> Outcome {
    let path = path.map(|p| p.as_str()).unwrap_or("output");
    Outcome::check(false, format!("`{path}` not found in the run result"))
}

fn equals(spec: &AssertionSpec) -> Result<Box<dyn super::Assertion>, EvalError> {
    only_keys(spec, &[])?;
    let expected = required_value(spec)?;
    let transform = transform_of(spec)?;
    Ok(sync_assertion("equals", move |ctx| {
        let Some(subject) = ctx.subject(transform.as_ref()) else { return missing(transform.as_ref()) };
        let pass = match &expected {
            Json::String(text) => as_text(&subject) == *text,
            other => as_json(&subject).is_some_and(|s| json_eq(&s, other)),
        };
        let reason = if pass {
            format!("equals {}", brief(&expected))
        } else {
            format!("expected {}, got {}", brief(&expected), brief(&subject))
        };
        Outcome::check(pass, reason)
    }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Contains {
    One,
    Any,
    All,
}

/// Needles of a contains assertion: a string (or number), a list, or (for the
/// -any / -all forms) a comma-separated string as in Promptfoo.
fn needles(value: &Json, mode: Contains) -> Result<Vec<String>, EvalError> {
    let scalar = |v: &Json| match v {
        Json::String(s) => Ok(s.clone()),
        Json::Number(_) | Json::Bool(_) => Ok(v.to_string()),
        _ => Err(EvalError::invalid("`value` items must be strings")),
    };
    match (value, mode) {
        (Json::Array(items), Contains::Any | Contains::All) => items.iter().map(scalar).collect(),
        (Json::String(s), Contains::Any | Contains::All) => {
            Ok(s.split(',').map(|p| p.trim().to_owned()).filter(|p| !p.is_empty()).collect())
        }
        (Json::Array(_), Contains::One) => Err(EvalError::invalid("`value` must be a string; use contains-all")),
        (other, Contains::One) => Ok(vec![scalar(other)?]),
        _ => Err(EvalError::invalid("`value` must be a string or a list of strings")),
    }
}

fn contains(spec: &AssertionSpec, mode: Contains, insensitive: bool) -> Result<Box<dyn super::Assertion>, EvalError> {
    only_keys(spec, &[])?;
    let mut wanted = needles(&required_value(spec)?, mode)?;
    if wanted.is_empty() {
        return Err(EvalError::invalid("`value` must not be empty"));
    }
    if insensitive {
        wanted = wanted.iter().map(|w| w.to_lowercase()).collect();
    }
    let transform = transform_of(spec)?;
    Ok(sync_assertion("contains", move |ctx| {
        let Some(subject) = ctx.subject(transform.as_ref()) else { return missing(transform.as_ref()) };
        let mut haystack = as_text(&subject);
        if insensitive {
            haystack = haystack.to_lowercase();
        }
        let found: Vec<&String> = wanted.iter().filter(|w| haystack.contains(w.as_str())).collect();
        let pass = match mode {
            Contains::One | Contains::All => found.len() == wanted.len(),
            Contains::Any => !found.is_empty(),
        };
        let score = match mode {
            Contains::All => found.len() as f64 / wanted.len() as f64,
            _ => f64::from(u8::from(pass)),
        };
        let reason = if pass {
            format!("found {}", found.iter().map(|s| format!("{s:?}")).collect::<Vec<_>>().join(", "))
        } else {
            let absent: Vec<String> = wanted.iter().filter(|w| !found.contains(w)).map(|s| format!("{s:?}")).collect();
            format!("{} not found in {}", absent.join(", "), brief(&subject))
        };
        Outcome::graded(pass, score, reason)
    }))
}

fn starts_with(spec: &AssertionSpec) -> Result<Box<dyn super::Assertion>, EvalError> {
    only_keys(spec, &[])?;
    let prefix = needles(&required_value(spec)?, Contains::One)?.concat();
    let transform = transform_of(spec)?;
    Ok(sync_assertion("starts-with", move |ctx| {
        let Some(subject) = ctx.subject(transform.as_ref()) else { return missing(transform.as_ref()) };
        let pass = as_text(&subject).starts_with(&prefix);
        Outcome::check(
            pass,
            format!("{} {} with {prefix:?}", brief(&subject), if pass { "starts" } else { "does not start" }),
        )
    }))
}

fn regex(spec: &AssertionSpec) -> Result<Box<dyn super::Assertion>, EvalError> {
    only_keys(spec, &[])?;
    let Json::String(pattern) = required_value(spec)? else {
        return Err(EvalError::invalid("`value` must be a regular expression string"));
    };
    let compiled = regex::Regex::new(&pattern).map_err(|e| EvalError::invalid(format!("invalid regex: {e}")))?;
    let transform = transform_of(spec)?;
    Ok(sync_assertion("regex", move |ctx| {
        let Some(subject) = ctx.subject(transform.as_ref()) else { return missing(transform.as_ref()) };
        let pass = compiled.is_match(&as_text(&subject));
        Outcome::check(
            pass,
            format!("{} {} /{pattern}/", brief(&subject), if pass { "matches" } else { "does not match" }),
        )
    }))
}

/// `is-json` (optional schema in `value`) and `json-schema` (schema required).
fn json_check(spec: &AssertionSpec, schema_required: bool) -> Result<Box<dyn super::Assertion>, EvalError> {
    only_keys(spec, &[])?;
    let schema = match (&spec.value, schema_required) {
        (Some(schema @ Json::Object(_)), _) => Some(schema.clone()),
        (None | Some(Json::Null), false) => None,
        _ => return Err(EvalError::invalid("`value` must be a JSON Schema object")),
    };
    let transform = transform_of(spec)?;
    Ok(sync_assertion("json", move |ctx| {
        let Some(subject) = ctx.subject(transform.as_ref()) else { return missing(transform.as_ref()) };
        let Some(parsed) = as_json(&subject) else {
            return Outcome::check(false, format!("{} is not valid JSON", brief(&subject)));
        };
        match &schema {
            None => Outcome::check(true, "valid JSON"),
            Some(schema) => match graphrt_llm::nodes::schema::validate(&parsed, schema) {
                Ok(()) => Outcome::check(true, "matches the schema"),
                Err(problem) => Outcome::check(false, format!("does not match the schema: {problem}")),
            },
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use crate::capture::RunCapture;
    use serde_json::json;

    fn passes(spec: serde_json::Value, answer: &str) -> bool {
        eval(spec, &answered(answer)).pass
    }

    #[test]
    fn equality_and_containment() {
        assert!(passes(json!({"type": "equals", "value": "Hello, Ada."}), "Hello, Ada."));
        assert!(!passes(json!({"type": "equals", "value": "Hello"}), "Hello, Ada."));
        assert!(passes(json!({"type": "equals", "value": {"n": 1}}), "{\"n\": 1.0}"));
        assert!(passes(json!({"type": "contains", "value": "Ada"}), "Hello, Ada."));
        assert!(!passes(json!({"type": "contains", "value": "ada"}), "Hello, Ada."));
        assert!(passes(json!({"type": "icontains", "value": "ada"}), "Hello, Ada."));
        assert!(passes(json!({"type": "contains-any", "value": ["x", "Ada"]}), "Hello, Ada."));
        assert!(passes(json!({"type": "icontains-all", "value": "hello, ada"}), "Hello, Ada."));
        let partial = eval(json!({"type": "contains-all", "value": ["Hello", "Bob"]}), &answered("Hello, Ada."));
        assert!(!partial.pass && partial.score == 0.5 && partial.reason.contains("\"Bob\""), "{partial:?}");
        assert!(passes(json!({"type": "starts-with", "value": "Hello"}), "Hello, Ada."));
        assert!(!passes(json!({"type": "starts-with", "value": "Ada"}), "Hello, Ada."));
        assert!(passes(json!({"type": "regex", "value": "(?i)^hello, \\w+\\.$"}), "Hello, Ada."));
        assert!(!passes(json!({"type": "regex", "value": "^\\d+$"}), "Hello"));
        assert!(passes(json!({"type": "not-regex", "value": "^\\d+$"}), "Hello"));
        assert!(try_eval(json!({"type": "regex", "value": "("}), &answered("")).is_err());
        assert!(try_eval(json!({"type": "equals"}), &answered("")).is_err());
    }

    #[test]
    fn json_checks_and_paths() {
        let schema = json!({"type": "object", "required": ["score"], "properties": {"score": {"type": "number"}}});
        assert!(passes(json!({"type": "is-json"}), "{\"score\": 3}"));
        assert!(!passes(json!({"type": "is-json"}), "not json"));
        assert!(passes(json!({"type": "is-json", "value": schema}), "{\"score\": 3}"));
        assert!(passes(json!({"type": "json-schema", "value": schema}), "{\"score\": 3}"));
        let bad = eval(json!({"type": "json-schema", "value": schema}), &answered("{\"score\": \"high\"}"));
        assert!(!bad.pass && bad.reason.contains("schema"), "{bad:?}");
        assert!(try_eval(json!({"type": "json-schema"}), &answered("")).is_err());

        let mut capture = RunCapture::default();
        capture.outputs.insert("result".into(), json!({"label": "spam", "score": 0.9}));
        capture.outputs.insert("text".into(), json!("ok"));
        let ok = |spec| eval(spec, &capture).pass;
        assert!(ok(json!({"type": "equals", "transform": "outputs.result.label", "value": "spam"})));
        assert!(ok(json!({"type": "equals", "transform": "output.result.score", "value": 0.9})));
        assert!(ok(json!({"type": "equals", "transform": "outputs.text", "value": "ok"})));
        let missing = eval(json!({"type": "equals", "transform": "outputs.nope", "value": 1}), &capture);
        assert!(!missing.pass && missing.reason.contains("outputs.nope"));
        assert!(try_eval(json!({"type": "equals", "transform": "output.map(x => x)", "value": 1}), &capture).is_err());
    }
}
