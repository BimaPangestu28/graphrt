//! Where in a [`serde_json::Value`] a deserialization error happened, found
//! after the fact from the error message.
//!
//! A path-tracking deserializer (`serde_path_to_error`) wraps every
//! deserializer call and is instantiated again for every type it parses (≈ 180
//! KiB of a gateway binary for the node configs alone). Errors are rare and
//! the input is at hand, so this module recovers the path once, without
//! generics: serde's standard messages name the offending field, variant or
//! value (`unknown field `x``, `missing field `x``, `unknown variant `v``,
//! `invalid type: string "s", expected …`, `invalid length 3, expected …`), and
//! the tree is searched for it in document order. When the message names
//! nothing searchable (a custom error, or a map or sequence that occurs more
//! than once) the path is the root, and the message alone still says what is
//! wrong.

use serde_json::Value;

use crate::source_map::DocPath;

/// Path of the value `error` (from deserializing `root`) is about; the root
/// when it cannot be told.
pub fn error_path(root: &Value, error: &serde_json::Error) -> DocPath {
    let message = error.to_string();
    find(root, &message).unwrap_or_default()
}

/// [`error_path`] in the `serde_path_to_error` notation: `.` for the root,
/// else `variables[0].value_selector`.
pub fn error_path_text(root: &Value, error: &serde_json::Error) -> String {
    let path = error_path(root, error);
    if path.is_root() { ".".to_owned() } else { path.to_string() }
}

/// The text between the first pair of backticks after `prefix`.
fn quoted<'a>(message: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = message.strip_prefix(prefix)?.strip_prefix('`')?;
    rest.split_once('`').map(|(name, _)| name)
}

fn find(root: &Value, message: &str) -> Option<DocPath> {
    if let Some(field) = quoted(message, "unknown field ") {
        return first(root, &mut |value, _| value.as_object().is_some_and(|object| object.contains_key(field)))
            .map(|path| path.key(field));
    }
    if let Some(field) = quoted(message, "missing field ") {
        return missing(root, field);
    }
    if let Some(variant) = quoted(message, "unknown variant ") {
        return first(root, &mut |value, _| match value {
            Value::String(text) => text == variant,
            Value::Object(object) => object.len() == 1 && object.contains_key(variant),
            _ => false,
        });
    }
    if let Some(length) = message.strip_prefix("invalid length ").and_then(|rest| rest.split_once(',')) {
        let length: usize = length.0.parse().ok()?;
        return unique(root, &mut |value| value.as_array().is_some_and(|items| items.len() == length));
    }
    let unexpected = message
        .strip_prefix("invalid type: ")
        .or_else(|| message.strip_prefix("invalid value: "))
        .and_then(|rest| rest.rsplit_once(", expected "))
        .map(|(unexpected, _)| unexpected)?;
    match unexpected {
        "map" => unique(root, &mut Value::is_object),
        "sequence" => unique(root, &mut Value::is_array),
        "null" | "unit value" => first(root, &mut |value, _| value.is_null()),
        _ => first(root, &mut |value, _| describes(unexpected, value)),
    }
}

/// Whether serde's `Unexpected` text (`string "s"`, `integer `5``, ...) describes `value`.
fn describes(unexpected: &str, value: &Value) -> bool {
    match value {
        Value::String(text) => unexpected.strip_prefix("string ").is_some_and(|quoted| quoted == format!("{text:?}")),
        Value::Bool(flag) => unexpected == format!("boolean `{flag}`"),
        Value::Number(number) => {
            let shown = number.to_string();
            [format!("integer `{shown}`"), format!("floating point `{shown}`")].iter().any(|text| text == unexpected)
                || number.as_f64().is_some_and(|float| unexpected == format!("floating point `{float}`"))
        }
        _ => false,
    }
}

/// Visits every value in document order with its path; stops when `visit` returns true.
fn walk(value: &Value, path: &DocPath, visit: &mut dyn FnMut(&Value, &DocPath) -> bool) -> bool {
    if visit(value, path) {
        return true;
    }
    match value {
        Value::Object(object) => object.iter().any(|(key, child)| walk(child, &path.key(key), visit)),
        Value::Array(items) => items.iter().enumerate().any(|(index, child)| walk(child, &path.index(index), visit)),
        _ => false,
    }
}

fn first(root: &Value, matches: &mut dyn FnMut(&Value, &DocPath) -> bool) -> Option<DocPath> {
    let mut found = None;
    walk(root, &DocPath::root(), &mut |value, path| {
        let hit = matches(value, path);
        if hit {
            found = Some(path.clone());
        }
        hit
    });
    found
}

/// The only non-root value that matches, if exactly one does.
fn unique(root: &Value, matches: &mut dyn FnMut(&Value) -> bool) -> Option<DocPath> {
    let mut found = Vec::new();
    walk(root, &DocPath::root(), &mut |value, path| {
        if !path.is_root() && matches(value) {
            found.push(path.clone());
        }
        found.len() > 1
    });
    if found.len() == 1 { found.pop() } else { None }
}

/// The object a required field is missing from: an element of an array whose
/// other elements have it, else a nested object that lacks it while the root
/// has it, else the root.
fn missing(root: &Value, field: &str) -> Option<DocPath> {
    let lacks = |value: &Value| value.as_object().is_some_and(|object| !object.contains_key(field));
    let in_array = first(root, &mut |value, _| {
        value.as_array().is_some_and(|items| {
            items.iter().any(lacks) && items.iter().any(|item| item.as_object().is_some_and(|o| o.contains_key(field)))
        })
    });
    if let Some(array) = in_array {
        let items = lookup(root, &array)?.as_array()?;
        let index = items.iter().position(lacks)?;
        return Some(array.index(index));
    }
    if lacks(root) {
        return None;
    }
    unique(root, &mut |value| lacks(value))
}

fn lookup<'a>(root: &'a Value, path: &DocPath) -> Option<&'a Value> {
    use crate::source_map::PathSegment;
    path.segments().iter().try_fold(root, |value, segment| match segment {
        PathSegment::Key(key) => value.get(key),
        PathSegment::Index(index) => value.get(index),
    })
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use serde_json::json;

    use super::*;

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct Variable {
        name: String,
        #[serde(default)]
        value_selector: Vec<String>,
        #[serde(default)]
        kind: Option<Kind>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum Kind {
        Text,
        Number,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct Config {
        #[serde(default)]
        variables: Vec<Variable>,
        #[serde(default)]
        timeout_secs: u64,
        #[serde(default)]
        pair: Option<(u32, u32)>,
    }

    fn path_of(value: Value) -> String {
        let error = serde_json::from_value::<Config>(value.clone()).unwrap_err();
        error_path_text(&value, &error)
    }

    #[test]
    fn names_the_offending_path_like_a_tracking_deserializer() {
        assert_eq!(path_of(json!({ "variables": [{ "name": "a" }, { "name": "b", "valu": 1 }] })), "variables[1].valu");
        assert_eq!(path_of(json!({ "variables": [{ "name": "a" }, { "value_selector": [] }] })), "variables[1]");
        assert_eq!(
            path_of(json!({ "variables": [{ "name": "a", "value_selector": ["x", 5] }] })),
            "variables[0].value_selector[1]"
        );
        assert_eq!(path_of(json!({ "variables": [{ "name": "a", "kind": "date" }] })), "variables[0].kind");
        assert_eq!(path_of(json!({ "timeout_secs": "soon" })), "timeout_secs");
        assert_eq!(path_of(json!({ "timeout_secs": -1 })), "timeout_secs");
        assert_eq!(path_of(json!({ "timeout_secs": 1.5 })), "timeout_secs");
        assert_eq!(path_of(json!({ "pair": [1, 2, 3] })), "pair");
        assert_eq!(path_of(json!({ "variables": [{ "name": true }] })), "variables[0].name");
        assert_eq!(path_of(json!({ "extra": 1 })), "extra");
    }

    #[test]
    fn falls_back_to_the_root() {
        assert_eq!(path_of(json!({ "variables": {} })), "variables");
        let error = serde_json::Error::io(std::io::Error::other("custom"));
        assert!(error_path(&json!({}), &error).is_root());
    }
}
