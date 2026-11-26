//! UI blocks: rich visual output that graphrt nodes and tools emit and hosts render.
//!
//! A block is a JSON object whose `"$ui"` key names its kind ([`graphrt_core::ui::UI_KEY`]):
//!
//! ```json
//! {"$ui": "chart", "version": 1, "type": "bar", "title": "Sales by month",
//!  "x": {"values": ["Jan", "Feb", "Mar"]},
//!  "series": [{"name": "2026", "values": [120, 150, 90]}]}
//! ```
//!
//! | kind            | what it shows                                   | module              |
//! |-----------------|-------------------------------------------------|---------------------|
//! | `adaptive_card` | an [Adaptive Card](https://adaptivecards.io) 1.x, with inputs and actions | [`adaptive_card`] |
//! | `chart`         | bar, line, area, pie or donut chart             | [`chart`]           |
//! | `table`         | columns and rows                                | [`table`]           |
//! | `stats`         | KPI tiles: a value, a change, a trend           | [`stats`]           |
//! | `diagram`       | a diagram written as Mermaid text               | [`diagram`]         |
//!
//! [`validate`] is the gate every host passes a block through before rendering it: it
//! checks the envelope, the kind's shape (unknown fields are errors), sizes, and the URL
//! policy (images only as `data:` raster images or `https`, links only `https` / `mailto`).
//! Every kind also has a JSON Schema ([`schema`]) for tool parameters and editors; the
//! schemas describe the same shapes, while [`validate`] also checks what a schema cannot
//! (equal series lengths, unique input ids, size limits).
//!
//! Blocks carry no colours, styles, HTML or scripts: hosts map them to their own theme.
//!
//! ## Where a block cannot be drawn
//! A chat on WhatsApp or SMS shows text, not cards. [`fallback::to_text`] turns any block
//! into tidy text: a card becomes its text plus numbered choices that
//! [`fallback::match_choice`] maps back to the card's submit actions, a table becomes
//! compact lines, a chart a short summary. With the `png` feature,
//! `png::chart_png` draws a chart as a PNG image for channels that send media.
//!
//! ## Adding a kind
//! Add a module with a `validate` function, a [`UiKind`] variant, its schema in `schemas/`,
//! and fixtures in `fixtures/`; the fixture tests then check the schema and the validator agree.

use serde_json::{Map, Value as Json};

pub mod adaptive_card;
pub mod chart;
mod checks;
pub mod diagram;
pub mod fallback;
#[cfg(feature = "png")]
pub mod png;
pub mod stats;
pub mod table;

pub use graphrt_core::ui::UI_KEY;

/// The only block format version so far.
pub const VERSION: u64 = 1;
/// Largest serialized block, in bytes (256 KiB).
pub const MAX_BLOCK_BYTES: usize = 256 * 1024;

/// A block kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum UiKind {
    AdaptiveCard,
    Chart,
    Table,
    Stats,
    Diagram,
}

impl UiKind {
    /// Every kind, in documentation order.
    pub const ALL: [UiKind; 5] = [UiKind::AdaptiveCard, UiKind::Chart, UiKind::Table, UiKind::Stats, UiKind::Diagram];

    /// The `"$ui"` value of this kind.
    pub fn as_str(self) -> &'static str {
        match self {
            UiKind::AdaptiveCard => "adaptive_card",
            UiKind::Chart => "chart",
            UiKind::Table => "table",
            UiKind::Stats => "stats",
            UiKind::Diagram => "diagram",
        }
    }

    /// The kind named `name`, if graphrt knows it.
    pub fn parse(name: &str) -> Option<UiKind> {
        UiKind::ALL.into_iter().find(|kind| kind.as_str() == name)
    }
}

impl std::fmt::Display for UiKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a block was rejected. `path` is a JSON Pointer into the block (`""` for the block itself).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{}{message}", if path.is_empty() { String::new() } else { format!("at {path}: ") })]
pub struct UiError {
    pub path: String,
    pub message: String,
}

impl UiError {
    pub(crate) fn new(path: &str, message: impl Into<String>) -> Self {
        Self { path: path.to_owned(), message: message.into() }
    }
}

/// A block that passed [`validate`]: its kind and the block itself, unchanged.
#[derive(Clone, Debug, PartialEq)]
pub struct UiBlock {
    kind: UiKind,
    value: Json,
}

impl UiBlock {
    pub fn kind(&self) -> UiKind {
        self.kind
    }

    /// The validated block, including `"$ui"`.
    pub fn as_json(&self) -> &Json {
        &self.value
    }

    pub fn into_json(self) -> Json {
        self.value
    }

    /// The block's `title`, when it has one.
    pub fn title(&self) -> Option<&str> {
        self.value.get("title").and_then(Json::as_str)
    }
}

/// Checks that `value` is a well-formed UI block of a known kind.
///
/// ```
/// use serde_json::json;
///
/// let block = graphrt_ui::validate(&json!({
///     "$ui": "stats", "version": 1,
///     "items": [{"label": "Revenue", "value": 125000000, "format": "currency", "currency": "IDR"}],
/// }))
/// .unwrap();
/// assert_eq!(block.kind(), graphrt_ui::UiKind::Stats);
///
/// let error = graphrt_ui::validate(&json!({"$ui": "stats", "version": 1, "items": [], "colour": "red"})).unwrap_err();
/// assert!(error.to_string().contains("colour"), "{error}");
/// ```
pub fn validate(value: &Json) -> Result<UiBlock, UiError> {
    let fields = value.as_object().ok_or_else(|| UiError::new("", "a UI block must be a JSON object"))?;
    let kind_name = fields
        .get(UI_KEY)
        .and_then(Json::as_str)
        .ok_or_else(|| UiError::new("", format!("a UI block needs a string `{UI_KEY}` naming its kind")))?;
    let kind = UiKind::parse(kind_name).ok_or_else(|| {
        let known: Vec<&str> = UiKind::ALL.iter().map(|k| k.as_str()).collect();
        UiError::new(
            &format!("/{}", checks::escape_pointer(UI_KEY)),
            format!("unknown kind `{kind_name}` (known: {})", known.join(", ")),
        )
    })?;
    match fields.get("version") {
        Some(Json::Number(n)) if n.as_u64() == Some(VERSION) => {}
        _ => return Err(UiError::new("/version", format!("`version` must be {VERSION}"))),
    }
    let size = serde_json::to_vec(value).map(|bytes| bytes.len()).unwrap_or(usize::MAX);
    if size > MAX_BLOCK_BYTES {
        return Err(UiError::new("", format!("the block is {size} bytes; at most {MAX_BLOCK_BYTES} are allowed")));
    }
    let body = without_envelope(fields);
    match kind {
        UiKind::AdaptiveCard => adaptive_card::validate(&body)?,
        UiKind::Chart => chart::validate(&body)?,
        UiKind::Table => table::validate(&body)?,
        UiKind::Stats => stats::validate(&body)?,
        UiKind::Diagram => diagram::validate(&body)?,
    }
    Ok(UiBlock { kind, value: value.clone() })
}

/// Validates every block in `value` (see [`graphrt_core::ui::collect_ui_blocks`]).
pub fn validate_all(value: &graphrt_core::value::Value) -> Vec<Result<UiBlock, UiError>> {
    graphrt_core::ui::collect_ui_blocks(value).iter().map(|block| validate(&block.to_json())).collect()
}

/// The JSON Schema (draft 2020-12) of `kind`, as JSON text.
pub fn schema(kind: UiKind) -> &'static str {
    match kind {
        UiKind::AdaptiveCard => include_str!("../schemas/adaptive_card.schema.json"),
        UiKind::Chart => include_str!("../schemas/chart.schema.json"),
        UiKind::Table => include_str!("../schemas/table.schema.json"),
        UiKind::Stats => include_str!("../schemas/stats.schema.json"),
        UiKind::Diagram => include_str!("../schemas/diagram.schema.json"),
    }
}

/// The block's fields without `"$ui"` and `version`, which [`validate`] checked.
fn without_envelope(fields: &Map<String, Json>) -> Map<String, Json> {
    fields
        .iter()
        .filter(|(key, _)| key.as_str() != UI_KEY && key.as_str() != "version")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn envelope_errors_name_the_problem() {
        let error = |value: Json| validate(&value).unwrap_err().to_string();
        assert!(error(json!([])).contains("JSON object"));
        assert!(error(json!({"version": 1})).contains("`$ui`"));
        assert!(error(json!({"$ui": "video", "version": 1})).contains("unknown kind `video`"));
        assert!(error(json!({"$ui": "chart", "version": 2})).contains("`version` must be 1"));
        assert!(error(json!({"$ui": "chart"})).contains("`version` must be 1"));
    }

    #[test]
    fn oversized_blocks_are_rejected() {
        let rows: Vec<Json> = (0..3000).map(|i| json!({"name": format!("row {i} {}", "x".repeat(80))})).collect();
        let block = json!({"$ui": "table", "version": 1, "columns": [{"key": "name", "label": "Name"}], "rows": rows});
        assert!(validate(&block).unwrap_err().message.contains("bytes"));
    }

    #[test]
    fn kinds_round_trip_and_have_parseable_schemas() {
        for kind in UiKind::ALL {
            assert_eq!(UiKind::parse(kind.as_str()), Some(kind));
            let schema: Json = serde_json::from_str(schema(kind)).expect("schema is JSON");
            assert_eq!(schema["properties"][UI_KEY]["const"], json!(kind.as_str()), "{kind}");
        }
    }
}
