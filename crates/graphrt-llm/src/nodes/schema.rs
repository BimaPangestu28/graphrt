//! Lightweight structural check of model output against a JSON Schema.
//!
//! Supports the subset that matters for structured output: `type` (single or
//! list), `required`, `properties`, `items`, `enum` and `const`, applied
//! recursively. Keywords outside that subset are ignored rather than rejected,
//! so the check never fails valid output; it catches the common failure modes
//! (wrong top-level type, missing required fields, wrong field types).

use serde_json::Value as Json;

/// Maximum nesting depth checked.
const MAX_DEPTH: usize = 32;

/// Validate `value` against `schema`. Returns the first problem found, with a
/// JSON-pointer-like path.
pub fn validate(value: &Json, schema: &Json) -> Result<(), String> {
    check(value, schema, "$", 0)
}

/// Whether `value` matches the schema's top-level `type` (true when no type is given).
pub fn matches_top_level_type(value: &Json, schema: &Json) -> bool {
    match schema.get("type") {
        None => true,
        Some(t) => type_matches(value, t),
    }
}

fn check(value: &Json, schema: &Json, path: &str, depth: usize) -> Result<(), String> {
    if depth > MAX_DEPTH || !schema.is_object() {
        return Ok(());
    }
    if let Some(t) = schema.get("type") {
        if !type_matches(value, t) {
            return Err(format!("{path}: expected {}, got {}", type_label(t), kind(value)));
        }
    }
    if let Some(Json::Array(options)) = schema.get("enum") {
        if !options.contains(value) {
            return Err(format!("{path}: value {value} is not one of the allowed values"));
        }
    }
    if let Some(expected) = schema.get("const") {
        if expected != value {
            return Err(format!("{path}: expected constant {expected}"));
        }
    }
    match value {
        Json::Object(map) => {
            if let Some(Json::Array(required)) = schema.get("required") {
                for name in required.iter().filter_map(Json::as_str) {
                    if !map.contains_key(name) {
                        return Err(format!("{path}: missing required property `{name}`"));
                    }
                }
            }
            if let Some(Json::Object(properties)) = schema.get("properties") {
                for (name, sub) in properties {
                    if let Some(v) = map.get(name) {
                        check(v, sub, &format!("{path}.{name}"), depth + 1)?;
                    }
                }
            }
        }
        Json::Array(items) => {
            if let Some(item_schema) = schema.get("items").filter(|s| s.is_object()) {
                for (i, item) in items.iter().enumerate() {
                    check(item, item_schema, &format!("{path}[{i}]"), depth + 1)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn type_matches(value: &Json, ty: &Json) -> bool {
    match ty {
        Json::String(name) => single_type_matches(value, name),
        Json::Array(names) => names.iter().filter_map(Json::as_str).any(|n| single_type_matches(value, n)),
        _ => true,
    }
}

fn single_type_matches(value: &Json, name: &str) -> bool {
    match name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        "integer" => {
            value.as_i64().is_some() || value.as_u64().is_some() || value.as_f64().is_some_and(|f| f.fract() == 0.0)
        }
        _ => true, // unknown type names are not enforced
    }
}

fn type_label(ty: &Json) -> String {
    match ty {
        Json::String(s) => s.clone(),
        Json::Array(items) => items.iter().filter_map(Json::as_str).collect::<Vec<_>>().join(" or "),
        other => other.to_string(),
    }
}

fn kind(value: &Json) -> &'static str {
    match value {
        Json::Null => "null",
        Json::Bool(_) => "boolean",
        Json::Number(_) => "number",
        Json::String(_) => "string",
        Json::Array(_) => "array",
        Json::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Json {
        json!({
            "type": "object",
            "required": ["name", "tags"],
            "properties": {
                "name": {"type": "string"},
                "age": {"type": "integer"},
                "tags": {"type": "array", "items": {"type": "string"}},
                "kind": {"enum": ["a", "b"]},
                "maybe": {"type": ["string", "null"]}
            }
        })
    }

    #[test]
    fn accepts_valid_values() {
        assert!(
            validate(&json!({"name": "x", "tags": ["a"], "age": 3, "kind": "a", "maybe": null}), &schema()).is_ok()
        );
        assert!(validate(&json!({"name": "x", "tags": [], "extra": 1}), &schema()).is_ok());
    }

    #[test]
    fn reports_mismatches() {
        let err = validate(&json!([1]), &schema()).unwrap_err();
        assert!(err.contains("expected object"), "{err}");
        let err = validate(&json!({"name": "x"}), &schema()).unwrap_err();
        assert!(err.contains("`tags`"), "{err}");
        let err = validate(&json!({"name": "x", "tags": [1]}), &schema()).unwrap_err();
        assert!(err.contains("$.tags[0]"), "{err}");
        let err = validate(&json!({"name": "x", "tags": [], "age": 1.5}), &schema()).unwrap_err();
        assert!(err.contains("$.age"), "{err}");
        assert!(validate(&json!({"name": "x", "tags": [], "kind": "z"}), &schema()).is_err());
    }

    #[test]
    fn top_level_type() {
        assert!(matches_top_level_type(&json!({}), &schema()));
        assert!(!matches_top_level_type(&json!([1]), &schema()));
        assert!(matches_top_level_type(&json!([1]), &json!({})));
    }
}
