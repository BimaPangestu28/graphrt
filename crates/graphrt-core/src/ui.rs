//! UI blocks: rich visual output (cards, charts, tables, stat tiles) that any node or
//! tool can emit for a host to render.
//!
//! A UI block is a plain object whose `"$ui"` key names its kind, for example
//! `{"$ui": "chart", "version": 1, ...}`. The engine treats blocks as ordinary values;
//! this module only finds them, so hosts and nodes agree on one envelope. The block
//! kinds, their schemas and their validation live in the `graphrt-ui` crate: hosts must
//! validate a block there before rendering it.

use crate::value::Value;

/// Key that marks an object as a UI block; its value is the block kind.
pub const UI_KEY: &str = "$ui";
/// Most blocks [`collect_ui_blocks`] returns for one value.
pub const MAX_COLLECTED_BLOCKS: usize = 32;
/// How deep [`collect_ui_blocks`] looks into arrays and objects (the value itself is depth 0).
const MAX_SCAN_DEPTH: usize = 3;

/// The kind of a UI block (`"chart"`, `"adaptive_card"`...), or `None` when `value` is not one.
///
/// ```
/// use graphrt_core::ui::ui_block_kind;
/// use graphrt_core::value::Value;
///
/// let chart = Value::from_json(serde_json::json!({"$ui": "chart", "version": 1}));
/// assert_eq!(ui_block_kind(&chart), Some("chart"));
/// assert_eq!(ui_block_kind(&Value::from("chart")), None);
/// ```
pub fn ui_block_kind(value: &Value) -> Option<&str> {
    value.as_object()?.get(UI_KEY)?.as_str()
}

/// UI blocks in `value`, in document order: the value itself when it is a block, otherwise
/// blocks found in its array items and object fields, at most three levels down (a node's
/// `data` output, a list of results, a `{"chart": {...}}` wrapper). The inside of a block
/// is not searched, and at most [`MAX_COLLECTED_BLOCKS`] blocks are returned.
///
/// ```
/// use graphrt_core::ui::collect_ui_blocks;
/// use graphrt_core::value::Value;
///
/// let outputs = Value::from_json(serde_json::json!({
///     "data": {"$ui": "stats", "version": 1, "items": []},
///     "text": "done",
/// }));
/// assert_eq!(collect_ui_blocks(&outputs).len(), 1);
/// ```
pub fn collect_ui_blocks(value: &Value) -> Vec<Value> {
    let mut blocks = Vec::new();
    collect_into(value, 0, &mut blocks);
    blocks
}

fn collect_into(value: &Value, depth: usize, blocks: &mut Vec<Value>) {
    if blocks.len() >= MAX_COLLECTED_BLOCKS {
        return;
    }
    if ui_block_kind(value).is_some() {
        blocks.push(value.clone());
        return;
    }
    if depth >= MAX_SCAN_DEPTH {
        return;
    }
    match value {
        Value::Array(items) => items.iter().for_each(|item| collect_into(item, depth + 1, blocks)),
        Value::Object(fields) => fields.values().for_each(|field| collect_into(field, depth + 1, blocks)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn value(json: serde_json::Value) -> Value {
        Value::from_json(json)
    }

    #[test]
    fn block_kind_needs_a_string_ui_key() {
        assert_eq!(ui_block_kind(&value(json!({"$ui": "table"}))), Some("table"));
        assert_eq!(ui_block_kind(&value(json!({"$ui": 3}))), None);
        assert_eq!(ui_block_kind(&value(json!({"ui": "table"}))), None);
        assert_eq!(ui_block_kind(&value(json!([{"$ui": "table"}]))), None);
    }

    #[test]
    fn collects_blocks_in_order_without_searching_inside_blocks() {
        let found = collect_ui_blocks(&value(json!({
            "first": {"$ui": "chart", "nested": {"$ui": "table"}},
            "list": [{"$ui": "stats"}, "text", {"wrapper": {"$ui": "adaptive_card"}}],
        })));
        let kinds: Vec<_> = found.iter().filter_map(ui_block_kind).collect();
        assert_eq!(kinds, ["chart", "stats", "adaptive_card"]);
    }

    #[test]
    fn stops_at_the_depth_and_count_limits() {
        assert_eq!(collect_ui_blocks(&value(json!({"a": {"b": {"c": {"$ui": "chart"}}}}))).len(), 1);
        let too_deep = value(json!({"a": {"b": {"c": {"d": {"$ui": "chart"}}}}}));
        assert!(collect_ui_blocks(&too_deep).is_empty());
        let many = value(json!((0..40).map(|_| json!({"$ui": "chart"})).collect::<Vec<_>>()));
        assert_eq!(collect_ui_blocks(&many).len(), MAX_COLLECTED_BLOCKS);
    }
}
