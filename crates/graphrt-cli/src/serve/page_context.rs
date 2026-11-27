//! `sys.context`: what a web page tells the workflow about itself (the product
//! a visitor looks at, the page URL, the cart, a display name), sent by the
//! chat widget with every message.
//!
//! It comes from the browser, so anyone can write anything in it. It is
//! checked here for its shape and size only, and nothing in the server reads
//! it: who the caller is stays the verified token (`sys.user_id`), whatever the
//! context says. The limits are the ones of `checkChatContext` in the web
//! package.
//!
//! Part of the cargo feature `web-chat`, which sends it: a build without the
//! chat (the slim agents, `graphrt gateway`) carries none of this code.

use serde_json::Value;

/// The key in `sys`.
pub(crate) const CONTEXT_KEY: &str = "context";

/// The largest context, as compact JSON.
pub(crate) const MAX_CONTEXT_BYTES: usize = 8192;

/// The deepest nesting of objects and arrays: the context object is level 1.
pub(crate) const MAX_CONTEXT_DEPTH: usize = 5;

/// Check `sys.context` of a run request, when it has one.
///
/// @throws String - A message for a `400` response: the context is not a JSON
///   object, is nested deeper than [`MAX_CONTEXT_DEPTH`] or is larger than
///   [`MAX_CONTEXT_BYTES`].
pub(crate) fn check_page_context(sys: &serde_json::Map<String, Value>) -> Result<(), String> {
    let Some(context) = sys.get(CONTEXT_KEY) else {
        return Ok(());
    };
    if !context.is_object() {
        return Err("`sys.context` must be a JSON object".to_owned());
    }
    if depth_exceeds(context, 1) {
        return Err(format!("`sys.context` is nested deeper than {MAX_CONTEXT_DEPTH} levels"));
    }
    let bytes = serde_json::to_vec(context).map(|encoded| encoded.len()).unwrap_or(usize::MAX);
    if bytes > MAX_CONTEXT_BYTES {
        return Err(format!("`sys.context` is {bytes} bytes, more than {MAX_CONTEXT_BYTES}"));
    }
    Ok(())
}

/// Whether an object or array in `value`, which sits at `level`, is deeper than the limit.
fn depth_exceeds(value: &Value, level: usize) -> bool {
    let children: Box<dyn Iterator<Item = &Value>> = match value {
        Value::Object(map) => Box::new(map.values()),
        Value::Array(items) => Box::new(items.iter()),
        _ => return false,
    };
    if level > MAX_CONTEXT_DEPTH {
        return true;
    }
    children.into_iter().any(|child| depth_exceeds(child, level + 1))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sys(context: Value) -> serde_json::Map<String, Value> {
        let mut map = serde_json::Map::new();
        map.insert("query".to_owned(), json!("hi"));
        map.insert(CONTEXT_KEY.to_owned(), context);
        map
    }

    /// `levels` objects nested in each other, the innermost holding a string.
    fn nested(levels: usize) -> Value {
        let mut value = json!("leaf");
        for _ in 0..levels {
            value = json!({ "next": value });
        }
        value
    }

    #[test]
    fn no_context_is_fine() {
        let mut map = serde_json::Map::new();
        map.insert("query".to_owned(), json!("hi"));
        assert_eq!(check_page_context(&map), Ok(()));
    }

    #[test]
    fn a_page_context_is_accepted() {
        let context = json!({
            "product_id": "sku-1",
            "page": "https://shop.example/p/sku-1",
            "cart": { "items": [{ "id": "sku-2", "qty": 1 }], "total": 12.5 },
            "display_name": "Ana",
            "returning": true,
            "coupon": null
        });
        assert_eq!(check_page_context(&sys(context)), Ok(()));
        assert_eq!(check_page_context(&sys(json!({}))), Ok(()));
    }

    #[test]
    fn only_an_object_is_accepted() {
        for value in [json!("sku-1"), json!(42), json!(null), json!(true), json!(["a"])] {
            let refused = check_page_context(&sys(value.clone())).expect_err("not an object");
            assert!(refused.contains("JSON object"), "{value}: {refused}");
        }
    }

    #[test]
    fn depth_is_limited() {
        assert_eq!(check_page_context(&sys(nested(MAX_CONTEXT_DEPTH))), Ok(()));
        let refused = check_page_context(&sys(nested(MAX_CONTEXT_DEPTH + 1))).expect_err("too deep");
        assert!(refused.contains("nested deeper than 5"), "{refused}");
        // Arrays count as levels too.
        let arrays = json!({ "a": [[[["deep"]]]] });
        assert_eq!(check_page_context(&sys(arrays)), Ok(()));
        let deeper = json!({ "a": [[[[["deep"]]]]] });
        assert!(check_page_context(&sys(deeper)).is_err());
    }

    #[test]
    fn size_is_limited() {
        // `{"text":"..."}` is 11 bytes around the text.
        let fits = json!({ "text": "x".repeat(MAX_CONTEXT_BYTES - 11) });
        assert_eq!(serde_json::to_vec(&fits).unwrap().len(), MAX_CONTEXT_BYTES);
        assert_eq!(check_page_context(&sys(fits)), Ok(()));
        let too_large = json!({ "text": "x".repeat(MAX_CONTEXT_BYTES - 10) });
        let refused = check_page_context(&sys(too_large)).expect_err("too large");
        assert!(refused.contains("8193 bytes, more than 8192"), "{refused}");
        // Bytes, not characters: a multi-byte character counts as its UTF-8 length.
        let wide = json!({ "text": "é".repeat(MAX_CONTEXT_BYTES / 2) });
        assert!(check_page_context(&sys(wide)).is_err());
    }
}
