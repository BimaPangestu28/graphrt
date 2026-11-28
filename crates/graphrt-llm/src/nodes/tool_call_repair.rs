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
//! - **"Nothing" or a made-up value written as text**: `"max_height": "null"`, and
//!   `"date": "latest"` copied from a description's "default: latest" (the plugin answered
//!   422 and the model then answered from memory). Such an optional argument is left out; a
//!   date is put into `YYYY-MM-DD` where its meaning is clear, `today` becomes today's date,
//!   and an optional date after today is left out too.
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
            let required: Vec<&str> = schema
                .get("required")
                .and_then(Json::as_array)
                .map(|names| names.iter().filter_map(Json::as_str).collect())
                .unwrap_or_default();
            Json::Object(fit_fields(fields, properties, &required))
        }
        Json::Array(items) => match schema.get("items") {
            Some(item_schema) => Json::Array(items.into_iter().map(|item| fit_to_schema(item, item_schema)).collect()),
            None => Json::Array(items),
        },
        other => other,
    }
}

fn fit_fields(
    fields: Map<String, Json>,
    properties: Option<&Map<String, Json>>,
    required: &[&str],
) -> Map<String, Json> {
    let Some(properties) = properties else { return fields };
    let given: Vec<String> = fields.keys().map(|name| name.trim().to_owned()).collect();
    fields
        .into_iter()
        .filter_map(|(name, value)| {
            let name = known_name(&name, properties, &given);
            match properties.get(&name) {
                // "Nothing" written for a plain value (`"output": {}`, `"max_height": "null"`)
                // means the argument was left out.
                Some(schema) if is_plain_type(schema) && (is_nothing(&value) || is_nothing_as_text(&value, schema)) => {
                    None
                }
                Some(schema) if is_date(schema) => {
                    fit_date(value, required.contains(&name.as_str())).map(|value| (name, value))
                }
                Some(schema) => Some((name, fit_to_schema(value, schema))),
                None => Some((name, value)),
            }
        })
        .collect()
}

/// `"null"`, `"nil"` or `"undefined"` (and `"none"` where no text is asked for): what a model
/// writes as text for an argument it does not mean to give, unless the schema lists it as a value.
fn is_nothing_as_text(value: &Json, schema: &Json) -> bool {
    let Json::String(text) = value else { return false };
    let text = text.trim().to_ascii_lowercase();
    let listed = schema.get("enum").and_then(Json::as_array).is_some_and(|values| {
        values.iter().any(|listed| listed.as_str().is_some_and(|listed| listed.eq_ignore_ascii_case(&text)))
    });
    let wants_text = schema.get("type").and_then(Json::as_str) == Some("string");
    !listed && (matches!(text.as_str(), "null" | "nil" | "undefined") || (!wants_text && text == "none"))
}

/// Whether `schema` asks for a calendar date: `format: date`, or a text whose description
/// spells the form out (`Date, YYYY-MM-DD (default: latest)`).
fn is_date(schema: &Json) -> bool {
    if schema.get("type").and_then(Json::as_str) != Some("string") {
        return false;
    }
    schema.get("format").and_then(Json::as_str) == Some("date")
        || schema
            .get("description")
            .and_then(Json::as_str)
            .is_some_and(|description| description.to_ascii_uppercase().contains("YYYY-MM-DD"))
}

/// A date argument the tool can read: `2026-11-28` as it is, `2026/11/28` or `2026-11-28T10:00Z`
/// as `2026-11-28`, and `today` or `now` as today's date (UTC). An optional date that is
/// none of these (llama3.2 sent `"date": "latest"`, copied from the description's
/// "default: latest", and the plugin answered 422), or that lies after today, is left out,
/// so the tool uses its default. A required one stays as written for the tool to refuse.
fn fit_date(value: Json, required: bool) -> Option<Json> {
    let Json::String(text) = &value else { return (required || !is_nothing(&value)).then_some(value) };
    let today = today();
    let written = text.trim();
    let date = match written.to_ascii_lowercase().as_str() {
        "today" | "now" => Some(today.clone()),
        _ => calendar_date(written),
    };
    match date {
        Some(date) if date <= today => Some(Json::String(date)),
        _ if required => Some(value),
        _ => None,
    }
}

/// `YYYY-MM-DD` of a date written as `YYYY-MM-DD` or `YYYY/MM/DD`, optionally followed by a time.
fn calendar_date(text: &str) -> Option<String> {
    let head = text.get(..10)?;
    let rest = &text[10..];
    if !(rest.is_empty() || rest.starts_with(['T', 't', ' '])) {
        return None;
    }
    let parts: Vec<&str> = head.split(['-', '/']).collect();
    let [year, month, day] = parts.as_slice() else { return None };
    if year.len() != 4 || month.len() != 2 || day.len() != 2 {
        return None;
    }
    let (year, month, day): (i64, u32, u32) = (year.parse().ok()?, month.parse().ok()?, day.parse().ok()?);
    let days = days_from_civil(year, month, day);
    // A date that does not exist (`2026-02-30`) comes back as another day.
    (civil_from_days(days) == (year, month, day)).then(|| format!("{year:04}-{month:02}-{day:02}"))
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Hinnant's algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The date of a day number of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = u32::try_from(day_of_year - (153 * month_index + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if month_index < 10 { month_index + 3 } else { month_index - 9 }).unwrap_or(1);
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Today's date (UTC) as `YYYY-MM-DD`.
fn today() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX));
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    format!("{year:04}-{month:02}-{day:02}")
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

    fn rate_schema() -> Json {
        json!({
            "type": "object",
            "properties": {
                "base": { "type": "string" },
                "date": { "type": "string", "description": "Date, YYYY-MM-DD (default: latest)" },
                "on": { "type": "string", "format": "date" },
                "max_height": { "type": "integer" },
                "mode": { "type": "string", "enum": ["none", "all"] },
                "output": { "type": "string" }
            },
            "required": ["base", "on"]
        })
    }

    #[test]
    fn nothing_written_as_text_is_left_out_unless_the_schema_lists_it() {
        let fitted = fit_to_schema(
            json!({"base": "USD", "on": "2026-01-02", "max_height": "null", "output": "undefined", "mode": "none"}),
            &rate_schema(),
        );
        assert_eq!(fitted, json!({"base": "USD", "on": "2026-01-02", "mode": "none"}));
        let fitted = fit_to_schema(json!({"base": "USD", "on": "2026-01-02", "max_height": "None"}), &rate_schema());
        assert_eq!(fitted, json!({"base": "USD", "on": "2026-01-02"}));
    }

    #[test]
    fn a_date_is_put_into_its_form_and_a_made_up_optional_one_is_left_out() {
        let fit = |date: Json| fit_to_schema(json!({"base": "USD", "on": "2026-01-02", "date": date}), &rate_schema());
        // What llama3.2 sent to `currency-rates.get_pair_rate`.
        for made_up in ["latest", ".latest", "nil", "March 2024", "2026-02-30", "9999-01-01"] {
            assert_eq!(fit(json!(made_up)), json!({"base": "USD", "on": "2026-01-02"}), "{made_up}");
        }
        assert_eq!(fit(json!("2024/03/01"))["date"], json!("2024-03-01"));
        assert_eq!(fit(json!("2024-03-01T10:00:00Z"))["date"], json!("2024-03-01"));
        assert_eq!(fit(json!(" 2024-03-01 "))["date"], json!("2024-03-01"));
        assert_eq!(fit(json!("today"))["date"], json!(today()));
        // A required date that cannot be read stays for the tool to refuse; `now` is today.
        let required = |on: &str| fit_to_schema(json!({"base": "USD", "on": on}), &rate_schema());
        assert_eq!(required("latest")["on"], json!("latest"));
        assert_eq!(required("now")["on"], json!(today()));
    }

    #[test]
    fn day_numbers_and_dates_agree() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(days_from_civil(2026, 11, 28)), (2026, 11, 28));
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(today().len(), 10);
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
