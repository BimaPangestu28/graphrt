//! What the agent step forgives in the tool calls small models write.
//!
//! Measured with llama3.2 (3B) in graphrt Desktop (resizing a picture, reading a plugin), the
//! model picks the right tool and still fails the call:
//!
//! - **Arguments in the wrong type**: `{"size": "400", "overwrite": "false"}` for an integer
//!   and a boolean, `width` for a tool's `max_width`, `"output": {}` for "not given". The
//!   tool refuses "invalid arguments" and the model gives up. [`fit_to_schema`] puts every
//!   argument into the type the tool's own schema asks for when the meaning is clear, and
//!   leaves anything else for the tool to refuse with its own message.
//! - **A call written as text**: the answer is `{"name": "resize", "parameters": {...}}`
//!   instead of a tool call. [`calls_written_as_text`] turns such an object into the call
//!   when its name is one of the offered tools.
//! - **A required argument left out**: [`missing_required`] names it, so the step asks the
//!   model once to call again with it, instead of running a call the tool can only refuse.

use std::collections::HashSet;

use serde_json::{Map, Value as Json};

use crate::ToolCall;

/// `value` with every part put into the type `schema` asks for, where the meaning is clear:
/// a number written as text for an `integer` or `number`, `"true"` / `"false"` for a
/// `boolean`, JSON written as text for an `array` or `object`, a number for a `string`, and a
/// whole number written as `400.0` for an `integer`. Argument names are matched to the
/// schema's (trimmed; `width` for the only not-given `..._width`), and `null`, `{}` or `[]`
/// given for a plain value is left out. Everything else stays as written.
pub(crate) fn fit_to_schema(value: Json, schema: &Json) -> Json {
    let wanted = schema.get("type").and_then(Json::as_str).unwrap_or_default();
    let value = match (wanted, value) {
        ("integer", Json::String(text)) => match text.trim().parse::<i64>() {
            Ok(number) => Json::from(number),
            Err(_) => whole_number(text.trim().parse::<f64>().ok()).unwrap_or(Json::String(text)),
        },
        ("integer", Json::Number(number)) if !number.is_i64() && !number.is_u64() => {
            whole_number(number.as_f64()).unwrap_or(Json::Number(number))
        }
        ("number", Json::String(text)) => {
            match text.trim().parse::<f64>().ok().and_then(serde_json::Number::from_f64) {
                Some(number) => Json::Number(number),
                None => Json::String(text),
            }
        }
        ("boolean", Json::String(text)) => match text.trim().to_ascii_lowercase().as_str() {
            "true" => Json::Bool(true),
            "false" => Json::Bool(false),
            _ => Json::String(text),
        },
        ("string", Json::Number(number)) => Json::String(number.to_string()),
        ("array" | "object", Json::String(text)) => match serde_json::from_str::<Json>(&text) {
            Ok(parsed) if (wanted == "array" && parsed.is_array()) || (wanted == "object" && parsed.is_object()) => {
                parsed
            }
            _ => Json::String(text),
        },
        (_, value) => value,
    };
    match value {
        Json::Object(fields) => {
            let properties = schema.get("properties").and_then(Json::as_object);
            Json::Object(fit_fields(fields, properties))
        }
        Json::Array(items) => match schema.get("items") {
            Some(item_schema) => Json::Array(items.into_iter().map(|item| fit_to_schema(item, item_schema)).collect()),
            None => Json::Array(items),
        },
        other => other,
    }
}

fn fit_fields(fields: Map<String, Json>, properties: Option<&Map<String, Json>>) -> Map<String, Json> {
    let Some(properties) = properties else { return fields };
    let given: Vec<String> = fields.keys().map(|name| name.trim().to_owned()).collect();
    fields
        .into_iter()
        .filter_map(|(name, value)| {
            let name = known_name(&name, properties, &given);
            match properties.get(&name) {
                // "Nothing" written for a plain value (`"output": {}`) means the argument was left out.
                Some(schema) if is_plain_type(schema) && is_nothing(&value) => None,
                Some(schema) => Some((name, fit_to_schema(value, schema))),
                None => Some((name, value)),
            }
        })
        .collect()
}

/// Whether `schema` asks for text, a number or a yes/no.
fn is_plain_type(schema: &Json) -> bool {
    matches!(schema.get("type").and_then(Json::as_str), Some("string" | "integer" | "number" | "boolean"))
}

/// `null`, `{}` or `[]`: what a model writes for an argument it does not mean to give.
fn is_nothing(value: &Json) -> bool {
    match value {
        Json::Null => true,
        Json::Object(fields) => fields.is_empty(),
        Json::Array(items) => items.is_empty(),
        _ => false,
    }
}

/// The schema's own name for an argument a model called `name`: `name` itself when the schema
/// knows it (also after trimming spaces), or the one name that ends in `_<name>` and was not
/// given as well (`width` for `max_width`). Anything else stays as written.
fn known_name(name: &str, properties: &Map<String, Json>, given: &[String]) -> String {
    let trimmed = name.trim();
    if properties.contains_key(trimmed) {
        return trimmed.to_owned();
    }
    let suffix = format!("_{trimmed}");
    let mut longer = properties.keys().filter(|known| known.ends_with(&suffix) && !given.contains(known));
    match (longer.next(), longer.next()) {
        (Some(only), None) if !trimmed.is_empty() => only.clone(),
        _ => name.to_owned(),
    }
}

/// `number` as a JSON integer when it has no fraction and fits one.
fn whole_number(number: Option<f64>) -> Option<Json> {
    let number = number.filter(|number| number.is_finite() && number.fract() == 0.0)?;
    // Parsed back from its own text: exact for every whole number an `i64` holds.
    format!("{number:.0}").parse::<i64>().ok().map(Json::from)
}

/// The required arguments of `schema` that `arguments` does not give (or gives as nothing),
/// each with its description when the schema has one: "`max_width` (Largest width in pixels)".
pub(crate) fn missing_required(arguments: &Map<String, Json>, schema: &Json) -> Vec<String> {
    let Some(required) = schema.get("required").and_then(Json::as_array) else { return Vec::new() };
    let properties = schema.get("properties").and_then(Json::as_object);
    required
        .iter()
        .filter_map(Json::as_str)
        .filter(|name| arguments.get(*name).is_none_or(is_nothing))
        .map(|name| {
            let description = properties
                .and_then(|properties| properties.get(name))
                .and_then(|property| property.get("description"))
                .and_then(Json::as_str)
                .map(str::trim)
                .filter(|description| !description.is_empty());
            match description {
                Some(description) => format!("`{name}` ({description})"),
                None => format!("`{name}`"),
            }
        })
        .collect()
}

/// Calls a model wrote as text instead of making them: JSON objects in `text` that name one
/// of `tools` (`"name"`, `"tool"` or `"function"`) and carry their arguments as an object
/// (`"parameters"`, `"arguments"`, `"args"` or `"input"`, also as JSON text). Returns the
/// calls, with ids from `id_prefix`, and `text` without those objects (and the code fences
/// around them); `None` when there are none.
///
/// A name matches a tool when it is the tool's name, or when it is the last part of exactly
/// one tool's name (`resize` for `image_resize` or `image.resize`).
pub(crate) fn calls_written_as_text(text: &str, tools: &[&str], id_prefix: &str) -> Option<(Vec<ToolCall>, String)> {
    let mut calls = Vec::new();
    let mut kept = String::new();
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        let candidate = &rest[open..];
        let mut objects = serde_json::Deserializer::from_str(candidate).into_iter::<Json>();
        match objects.next() {
            Some(Ok(Json::Object(object))) => {
                let length = objects.byte_offset();
                match call_in(&object, tools) {
                    Some((name, arguments)) => {
                        kept.push_str(&rest[..open]);
                        let id = format!("{id_prefix}{}", calls.len() + 1);
                        calls.push(ToolCall { id, name, arguments: Json::Object(arguments).to_string() });
                    }
                    None => kept.push_str(&rest[..open + length]),
                }
                rest = &candidate[length..];
            }
            _ => {
                kept.push_str(&rest[..=open]);
                rest = &rest[open + 1..];
            }
        }
    }
    kept.push_str(rest);
    if calls.is_empty() {
        return None;
    }
    Some((calls, without_empty_fences(&kept)))
}

/// The tool and arguments of a JSON object that is a call, if it is one.
fn call_in(object: &Map<String, Json>, tools: &[&str]) -> Option<(String, Map<String, Json>)> {
    let written_name = ["name", "tool", "function"].iter().find_map(|key| match object.get(*key) {
        Some(Json::String(name)) => Some(name.clone()),
        // OpenAI's shape: `{"function": {"name": ..., "arguments": ...}}`.
        Some(Json::Object(inner)) => inner.get("name").and_then(Json::as_str).map(str::to_owned),
        _ => None,
    })?;
    let holder = match object.get("function") {
        Some(Json::Object(inner)) => inner,
        _ => object,
    };
    let arguments = ["parameters", "arguments", "args", "input"].iter().find_map(|key| match holder.get(*key) {
        Some(Json::Object(arguments)) => Some(arguments.clone()),
        Some(Json::String(text)) => match serde_json::from_str::<Json>(text) {
            Ok(Json::Object(arguments)) => Some(arguments),
            _ => None,
        },
        _ => None,
    })?;
    Some((tool_named(&written_name, tools)?, arguments))
}

/// The offered tool a model meant by `written`.
fn tool_named(written: &str, tools: &[&str]) -> Option<String> {
    let written = written.trim();
    if tools.contains(&written) {
        return Some(written.to_owned());
    }
    if written.is_empty() {
        return None;
    }
    let ends_with_it =
        |tool: &&&str| tool.strip_suffix(written).is_some_and(|start| start.ends_with(['_', '.', '/', '-', ':']));
    let mut matching: HashSet<&str> = tools.iter().filter(ends_with_it).copied().collect();
    if matching.len() == 1 { matching.drain().next().map(str::to_owned) } else { None }
}

/// `text` without code fences that held only a call, and without surrounding blank space.
fn without_empty_fences(text: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    let mut fence_start: Option<usize> = None;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            match fence_start {
                Some(start) if lines.get(start + 1..).unwrap_or_default().iter().all(|kept| kept.trim().is_empty()) => {
                    lines.truncate(start);
                    fence_start = None;
                    continue;
                }
                Some(_) => fence_start = None,
                None => fence_start = Some(lines.len()),
            }
        }
        lines.push(line);
    }
    lines.join("\n").trim().to_owned()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn resize_schema() -> Json {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "The picture to resize" },
                "max_width": { "type": "integer", "description": "Largest width in pixels" },
                "max_height": { "type": "integer" },
                "overwrite": { "type": "boolean" },
                "quality": { "type": "number" },
                "label": { "type": "string" },
                "sizes": { "type": "array", "items": { "type": "integer" } }
            },
            "required": ["path", "max_width"]
        })
    }

    #[test]
    fn arguments_written_as_text_become_the_types_the_schema_asks_for() {
        // What llama3.2 sent to an image tool.
        let fitted = fit_to_schema(
            json!({"path": "uploads/a.png", " width": "400", "overwrite": "false", "quality": "0.8", "label": 7,
                   "sizes": "[\"100\", 200]", "max_height": {}}),
            &resize_schema(),
        );
        assert_eq!(
            fitted,
            json!({"path": "uploads/a.png", "max_width": 400, "overwrite": false, "quality": 0.8, "label": "7",
                   "sizes": [100, 200]})
        );
    }

    #[test]
    fn what_does_not_fit_is_left_for_the_tool_to_refuse() {
        let fitted = fit_to_schema(
            json!({"path": 3.5, "max_width": "wide", "overwrite": "maybe", "unknown": "x", "max_height": 120.0}),
            &resize_schema(),
        );
        assert_eq!(
            fitted,
            json!({"path": "3.5", "max_width": "wide", "overwrite": "maybe", "unknown": "x", "max_height": 120})
        );
        // Text a tool wants stays text, however it looks.
        assert_eq!(fit_to_schema(json!({"path": "400"}), &resize_schema()), json!({"path": "400"}));
        // Two names end in `_size`: neither is guessed.
        let two =
            json!({"type": "object", "properties": {"min_size": {"type": "integer"}, "max_size": {"type": "integer"}}});
        assert_eq!(fit_to_schema(json!({"size": "1"}), &two), json!({"size": "1"}));
    }

    #[test]
    fn missing_required_arguments_are_named_with_their_description() {
        let given = json!({"path": "a.png", "max_height": 10}).as_object().unwrap().clone();
        assert_eq!(missing_required(&given, &resize_schema()), ["`max_width` (Largest width in pixels)"]);
        let nothing = json!({"path": null, "max_width": 400}).as_object().unwrap().clone();
        assert_eq!(missing_required(&nothing, &resize_schema()), ["`path` (The picture to resize)"]);
        assert!(missing_required(&Map::new(), &json!({"type": "object"})).is_empty());
    }

    #[test]
    fn a_call_written_as_text_becomes_the_call() {
        let tools = ["image_resize", "image_describe", "files_list"];
        let text = "I will resize it.\n```json\n{\"name\": \"resize\", \"parameters\": {\"path\": \"uploads/a.png\", \"max_width\": 400}}\n```";

        let (calls, rest) = calls_written_as_text(text, &tools, "text_call_1_").unwrap();

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "image_resize");
        assert_eq!(calls[0].id, "text_call_1_1");
        assert_eq!(calls[0].arguments_json().unwrap()["max_width"], 400);
        assert_eq!(rest, "I will resize it.");
    }

    #[test]
    fn other_shapes_of_written_calls_are_read_too() {
        let tools = ["currency_rates__get_pair_rate", "search"];
        let openai = r#"{"function": {"name": "currency_rates__get_pair_rate", "arguments": "{\"base\": \"USD\", \"quote\": \"IDR\"}"}}"#;
        let (calls, rest) = calls_written_as_text(openai, &tools, "t").unwrap();
        assert_eq!(calls[0].arguments_json().unwrap()["quote"], "IDR");
        assert_eq!(rest, "");
        let two = r#"{"tool": "search", "args": {"q": "a"}} and {"tool": "search", "input": {"q": "b"}}"#;
        let (calls, rest) = calls_written_as_text(two, &tools, "t").unwrap();
        assert_eq!(calls.iter().map(|call| call.id.as_str()).collect::<Vec<_>>(), ["t1", "t2"]);
        assert_eq!(rest, "and");
    }

    #[test]
    fn json_that_is_not_a_call_of_an_offered_tool_stays_text() {
        let tools = ["image_resize", "audio_resize"];
        for text in [
            r#"The result is {"width": 400, "height": 250}."#,
            r#"{"name": "shell", "parameters": {"command": "convert a.png -resize 400 b.png"}}"#,
            // Ends two tools' names: not guessed.
            r#"{"name": "resize", "parameters": {"path": "a.png"}}"#,
            r#"{"name": "image_resize", "parameters": "not json"}"#,
            "No JSON here {at all",
        ] {
            assert!(calls_written_as_text(text, &tools, "t").is_none(), "{text}");
        }
    }
}
