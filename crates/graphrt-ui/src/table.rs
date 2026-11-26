//! `table`: columns and rows.
//!
//! ```json
//! {"$ui": "table", "version": 1, "title": "Top products",
//!  "columns": [{"key": "name", "label": "Product"},
//!              {"key": "revenue", "label": "Revenue", "format": "currency", "currency": "IDR"},
//!              {"key": "status", "label": "Status", "format": "status"}],
//!  "rows": [{"name": "Kopi Susu", "revenue": 12500000, "status": "In stock"}]}
//! ```
//!
//! - `columns`: 1 to 20, each with a unique `key` and a `label`; optional `align`
//!   (`left` | `center` | `right`; numbers default to right) and `format`: `text`, a number
//!   format (see [`crate::chart::NUMBER_FORMATS`]), `date` (ISO 8601 text) or `status`
//!   (shown as a pill; `done`, `failed`... are coloured by the host).
//! - `rows`: up to 1000 objects; each value is a string, number, boolean or `null`.
//!   Keys without a column are not shown.

use crate::UiError;
use crate::chart::NUMBER_FORMATS;
use crate::checks::{
    Fields, MAX_LONG_TEXT, MAX_SHORT_TEXT, array, child, index, object, only_known, optional_choice, optional_currency,
    optional_text, required_name,
};

pub const MAX_COLUMNS: usize = 20;
pub const MAX_ROWS: usize = 1000;
const ALIGNMENTS: &[&str] = &["left", "center", "right"];

const FIELDS: &[&str] = &["title", "subtitle", "fallback_text", "columns", "rows"];

/// Column formats: the number formats plus `text`, `date` and `status`.
pub fn column_formats() -> Vec<&'static str> {
    let mut formats = vec!["text", "date", "status"];
    formats.extend_from_slice(NUMBER_FORMATS);
    formats
}

pub(crate) fn validate(fields: &Fields) -> Result<(), UiError> {
    only_known(fields, "", FIELDS)?;
    optional_text(fields, "title", "", MAX_SHORT_TEXT)?;
    optional_text(fields, "subtitle", "", MAX_SHORT_TEXT)?;
    optional_text(fields, "fallback_text", "", MAX_LONG_TEXT)?;
    let formats = column_formats();
    let columns = array(fields, "columns", "", 1, MAX_COLUMNS)?;
    let mut keys = Vec::with_capacity(columns.len());
    for (i, column) in columns.iter().enumerate() {
        let path = index("/columns", i);
        let column = object(column, &path)?;
        only_known(column, &path, &["key", "label", "align", "format", "currency"])?;
        let key = required_name(column, "key", &path)?;
        if keys.contains(&key) {
            return Err(UiError::new(&child(&path, "key"), format!("`{key}` is used by another column")));
        }
        keys.push(key);
        required_name(column, "label", &path)?;
        optional_choice(column, "align", &path, ALIGNMENTS)?;
        optional_choice(column, "format", &path, &formats)?;
        optional_currency(column, "currency", &path)?;
    }
    for (i, row) in array(fields, "rows", "", 0, MAX_ROWS)?.iter().enumerate() {
        let path = index("/rows", i);
        for (key, value) in object(row, &path)? {
            let ok = match value {
                serde_json::Value::String(s) => s.chars().count() <= MAX_LONG_TEXT,
                serde_json::Value::Number(_) | serde_json::Value::Bool(_) | serde_json::Value::Null => true,
                _ => false,
            };
            if !ok {
                return Err(UiError::new(&child(&path, key), "must be a string, number, boolean or null"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::validate;
    use serde_json::json;

    #[test]
    fn checks_columns_and_cells() {
        let table = |columns: serde_json::Value, rows: serde_json::Value| {
            validate(&json!({"$ui": "table", "version": 1, "columns": columns, "rows": rows})).map(|_| ())
        };
        let name = json!([{"key": "name", "label": "Name"}, {"key": "n", "label": "N", "format": "compact", "align": "Right"}]);
        assert!(table(name.clone(), json!([{"name": "a", "n": 1, "extra": true}])).is_ok());
        assert!(table(name.clone(), json!([])).is_ok());
        let duplicate = json!([{"key": "a", "label": "A"}, {"key": "a", "label": "B"}]);
        assert!(table(duplicate, json!([])).unwrap_err().message.contains("another column"));
        assert!(table(name.clone(), json!([{"name": {"nested": 1}}])).unwrap_err().path == "/rows/0/name");
        assert!(table(json!([{"key": "a", "label": "A", "format": "html"}]), json!([])).is_err());
        assert!(table(json!([]), json!([])).is_err());
    }
}
