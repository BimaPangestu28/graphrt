//! `stats`: KPI tiles, each a label, a value and an optional change.
//!
//! ```json
//! {"$ui": "stats", "version": 1, "title": "This week",
//!  "items": [{"label": "Revenue", "value": 125000000, "format": "currency", "currency": "IDR",
//!             "delta": 12.5, "delta_format": "percent", "caption": "vs last week"},
//!            {"label": "Refunds", "value": 3, "delta": 2, "good": "down"}]}
//! ```
//!
//! - `items`: 1 to 12 tiles. `value` is a number (shown with `format`, see
//!   [`crate::chart::NUMBER_FORMATS`]) or a short string shown as is.
//! - `delta`: the change, a number; `delta_format` is `number` (default) or `percent`.
//!   `trend` (`up` | `down` | `flat`) defaults to the sign of `delta`. `good` (`up`, the
//!   default, or `down`) says which direction is good news, so hosts can colour it.

use crate::UiError;
use crate::chart::NUMBER_FORMATS;
use crate::checks::{
    Fields, MAX_LONG_TEXT, MAX_SHORT_TEXT, array, child, index, object, only_known, optional_choice, optional_currency,
    optional_number, optional_text, required_name,
};

pub const MAX_ITEMS: usize = 12;
const DIRECTIONS: &[&str] = &["up", "down"];
const TRENDS: &[&str] = &["up", "down", "flat"];
const ITEM_FIELDS: &[&str] =
    &["label", "value", "format", "currency", "delta", "delta_format", "trend", "good", "caption"];

pub(crate) fn validate(fields: &Fields) -> Result<(), UiError> {
    only_known(fields, "", &["title", "subtitle", "fallback_text", "items"])?;
    optional_text(fields, "title", "", MAX_SHORT_TEXT)?;
    optional_text(fields, "subtitle", "", MAX_SHORT_TEXT)?;
    optional_text(fields, "fallback_text", "", MAX_LONG_TEXT)?;
    for (i, item) in array(fields, "items", "", 1, MAX_ITEMS)?.iter().enumerate() {
        let path = index("/items", i);
        let item = object(item, &path)?;
        only_known(item, &path, ITEM_FIELDS)?;
        required_name(item, "label", &path)?;
        match item.get("value") {
            Some(serde_json::Value::Number(_)) => {}
            Some(serde_json::Value::String(s)) if s.chars().count() <= 60 => {}
            _ => {
                return Err(UiError::new(
                    &child(&path, "value"),
                    "must be a number or a string of at most 60 characters",
                ));
            }
        }
        optional_choice(item, "format", &path, NUMBER_FORMATS)?;
        optional_currency(item, "currency", &path)?;
        optional_number(item, "delta", &path)?;
        optional_choice(item, "delta_format", &path, &["number", "percent"])?;
        optional_choice(item, "trend", &path, TRENDS)?;
        optional_choice(item, "good", &path, DIRECTIONS)?;
        optional_text(item, "caption", &path, MAX_SHORT_TEXT)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::validate;
    use serde_json::json;

    #[test]
    fn checks_tiles() {
        let stats =
            |items: serde_json::Value| validate(&json!({"$ui": "stats", "version": 1, "items": items})).map(|_| ());
        assert!(stats(json!([{"label": "Orders", "value": 12, "delta": -3, "good": "up", "trend": "down"}])).is_ok());
        assert!(stats(json!([{"label": "Status", "value": "Healthy"}])).is_ok());
        assert!(stats(json!([{"label": "Orders"}])).unwrap_err().path == "/items/0/value");
        assert!(stats(json!([{"label": "Orders", "value": 1, "trend": "sideways"}])).is_err());
        assert!(stats(json!([])).is_err());
        let many: Vec<_> = (0..13).map(|i| json!({"label": format!("k{i}"), "value": i})).collect();
        assert!(stats(json!(many)).is_err());
    }
}
