//! `chart`: a bar, line, area, pie or donut chart.
//!
//! ```json
//! {"$ui": "chart", "version": 1, "type": "line", "title": "Orders per day",
//!  "x": {"label": "Day", "values": ["Mon", "Tue", "Wed"]},
//!  "y": {"label": "Orders", "format": "number"},
//!  "series": [{"name": "Shop A", "values": [12, 18, null]}, {"name": "Shop B", "values": [7, 9, 11]}]}
//! ```
//!
//! - `type`: `bar` | `line` | `area` | `pie` | `donut`.
//! - `x.values`: the categories (strings or numbers), 1 to 10 000. Every series has one
//!   value per category: a number, or `null` for "no data".
//! - `series`: 1 to 20; pie and donut take exactly one series of non-negative values and
//!   at most 50 categories (the slices).
//! - `y.format`: `number` | `compact` (1.2K, 3.4M) | `percent` (12.5 means 12.5%) |
//!   `currency` (with `y.currency`, an ISO 4217 code). Optional `y.min` / `y.max`.
//! - `stacked` (bar and area) and `horizontal` (bar) are optional.
//!
//! Colours come from the host's theme, in series order. At most 50 000 values in all;
//! hosts downsample long lines to the pixels they have.

use crate::UiError;
use crate::checks::{
    Fields, MAX_LONG_TEXT, MAX_SHORT_TEXT, array, child, index, object, only_known, optional_bool, optional_choice,
    optional_currency, optional_number, optional_text, required_name, required_text,
};

pub const CHART_TYPES: &[&str] = &["bar", "line", "area", "pie", "donut"];
/// Number formats of charts, tables and stat tiles.
pub const NUMBER_FORMATS: &[&str] = &["number", "compact", "percent", "currency"];
pub const MAX_CATEGORIES: usize = 10_000;
pub const MAX_SERIES: usize = 20;
pub const MAX_SLICES: usize = 50;
pub const MAX_VALUES: usize = 50_000;

const FIELDS: &[&str] = &["title", "subtitle", "fallback_text", "type", "x", "y", "series", "stacked", "horizontal"];

pub(crate) fn validate(fields: &Fields) -> Result<(), UiError> {
    only_known(fields, "", FIELDS)?;
    optional_text(fields, "title", "", MAX_SHORT_TEXT)?;
    optional_text(fields, "subtitle", "", MAX_SHORT_TEXT)?;
    optional_text(fields, "fallback_text", "", MAX_LONG_TEXT)?;
    let chart_type = required_text(fields, "type", "", MAX_SHORT_TEXT)?;
    if !CHART_TYPES.contains(&chart_type) {
        return Err(UiError::new("/type", format!("`{chart_type}` is not one of: {}", CHART_TYPES.join(", "))));
    }
    let is_round = matches!(chart_type, "pie" | "donut");
    let categories = validate_x(fields)?;
    validate_y(fields)?;
    if optional_bool(fields, "stacked", "")? == Some(true) && !matches!(chart_type, "bar" | "area") {
        return Err(UiError::new("/stacked", "only bar and area charts can be stacked"));
    }
    if optional_bool(fields, "horizontal", "")? == Some(true) && chart_type != "bar" {
        return Err(UiError::new("/horizontal", "only bar charts can be horizontal"));
    }
    if is_round && categories > MAX_SLICES {
        return Err(UiError::new("/x/values", format!("a {chart_type} chart has at most {MAX_SLICES} slices")));
    }
    let series = array(fields, "series", "", 1, MAX_SERIES)?;
    if is_round && series.len() != 1 {
        return Err(UiError::new("/series", format!("a {chart_type} chart has exactly one series")));
    }
    if series.len().saturating_mul(categories) > MAX_VALUES {
        return Err(UiError::new("/series", format!("at most {MAX_VALUES} values in all")));
    }
    for (i, item) in series.iter().enumerate() {
        validate_series(item, &index("/series", i), categories, is_round)?;
    }
    Ok(())
}

/// Checks `x` and returns its number of categories.
fn validate_x(fields: &Fields) -> Result<usize, UiError> {
    let x = object(fields.get("x").ok_or_else(|| UiError::new("", "`x` is required"))?, "/x")?;
    only_known(x, "/x", &["label", "values"])?;
    optional_text(x, "label", "/x", MAX_SHORT_TEXT)?;
    let values = array(x, "values", "/x", 1, MAX_CATEGORIES)?;
    for (i, value) in values.iter().enumerate() {
        match value {
            serde_json::Value::String(s) if s.chars().count() <= MAX_SHORT_TEXT => {}
            serde_json::Value::Number(_) => {}
            _ => return Err(UiError::new(&index("/x/values", i), "must be a short string or a number")),
        }
    }
    Ok(values.len())
}

fn validate_y(fields: &Fields) -> Result<(), UiError> {
    let Some(y) = fields.get("y") else { return Ok(()) };
    let y = object(y, "/y")?;
    only_known(y, "/y", &["label", "format", "currency", "min", "max"])?;
    optional_text(y, "label", "/y", MAX_SHORT_TEXT)?;
    optional_choice(y, "format", "/y", NUMBER_FORMATS)?;
    optional_currency(y, "currency", "/y")?;
    let (min, max) = (optional_number(y, "min", "/y")?, optional_number(y, "max", "/y")?);
    if matches!((min, max), (Some(min), Some(max)) if min >= max) {
        return Err(UiError::new("/y", "`min` must be below `max`"));
    }
    Ok(())
}

fn validate_series(item: &serde_json::Value, path: &str, categories: usize, is_round: bool) -> Result<(), UiError> {
    let series = object(item, path)?;
    only_known(series, path, &["name", "values"])?;
    required_name(series, "name", path)?;
    let values = array(series, "values", path, 0, MAX_CATEGORIES)?;
    if values.len() != categories {
        return Err(UiError::new(
            &child(path, "values"),
            format!(
                "has {} values but `x.values` has {categories}: give one value (or null) per category",
                values.len()
            ),
        ));
    }
    for (i, value) in values.iter().enumerate() {
        let ok = match value.as_f64() {
            Some(number) => number.is_finite() && (!is_round || number >= 0.0),
            None => value.is_null() && !is_round,
        };
        if !ok {
            let expected = if is_round { "a number of 0 or more" } else { "a number or null" };
            return Err(UiError::new(&index(&child(path, "values"), i), format!("must be {expected}")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::validate;
    use serde_json::{Value, json};

    fn chart(extra: Value) -> Value {
        let mut block = json!({
            "$ui": "chart", "version": 1, "type": "bar",
            "x": {"values": ["Jan", "Feb"]},
            "series": [{"name": "Sales", "values": [1, 2]}],
        });
        if let (Some(b), Some(e)) = (block.as_object_mut(), extra.as_object()) {
            b.extend(e.clone());
        }
        block
    }

    fn error(extra: Value) -> String {
        validate(&chart(extra)).unwrap_err().to_string()
    }

    #[test]
    fn accepts_a_minimal_and_a_full_chart() {
        assert!(validate(&chart(json!({}))).is_ok());
        let full = chart(json!({
            "type": "area", "title": "T", "subtitle": "S", "stacked": true,
            "y": {"label": "Rp", "format": "currency", "currency": "IDR", "min": 0, "max": 10},
            "series": [{"name": "A", "values": [1, null]}, {"name": "B", "values": [2, 3]}],
        }));
        assert!(validate(&full).is_ok(), "{:?}", validate(&full));
    }

    #[test]
    fn series_must_match_the_categories() {
        assert!(error(json!({"series": [{"name": "A", "values": [1]}]})).contains("one value (or null) per category"));
        assert!(error(json!({"series": [{"name": "A", "values": [1, "2"]}]})).contains("/series/0/values/1"));
        assert!(error(json!({"series": []})).contains("at least 1"));
    }

    #[test]
    fn pie_charts_have_one_series_of_non_negative_values() {
        assert!(validate(&chart(json!({"type": "pie"}))).is_ok());
        let two = json!([{"name": "A", "values": [1, 2]}, {"name": "B", "values": [1, 2]}]);
        assert!(error(json!({"type": "donut", "series": two})).contains("exactly one series"));
        assert!(error(json!({"type": "pie", "series": [{"name": "A", "values": [1, -2]}]})).contains("0 or more"));
        assert!(error(json!({"type": "pie", "series": [{"name": "A", "values": [1, null]}]})).contains("0 or more"));
    }

    #[test]
    fn options_must_fit_the_type() {
        assert!(error(json!({"type": "line", "stacked": true})).contains("stacked"));
        assert!(error(json!({"type": "line", "horizontal": true})).contains("horizontal"));
        assert!(error(json!({"type": "radar"})).contains("not one of"));
        assert!(error(json!({"y": {"format": "currency", "currency": "rupiah"}})).contains("ISO 4217"));
        assert!(error(json!({"y": {"min": 5, "max": 1}})).contains("below"));
        assert!(error(json!({"colors": ["red"]})).contains("unknown field `colors`"));
    }
}
