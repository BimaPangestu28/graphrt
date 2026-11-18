//! Mapping between MCP wire types and graphrt tool types.

use graphrt_core::tool::{ToolImage, ToolOutput, ToolSpec};
use graphrt_core::value::Value;
use rmcp::model::{CallToolResult, ContentBlock, ResourceContents, Tool};

/// Convert an MCP tool description.
pub fn tool_spec(tool: &Tool) -> ToolSpec {
    let description = tool.description.as_deref().or(tool.title.as_deref()).unwrap_or_default().to_owned();
    ToolSpec {
        name: tool.name.to_string(),
        description,
        parameters: serde_json::Value::Object((*tool.input_schema).clone()),
    }
}

/// Convert a tool call result.
///
/// - `text`: all text blocks (and text resources) joined by newlines; binary
///   blocks appear as short placeholders such as `[image: image/png]`.
/// - `data`: the server's structured content when present; otherwise, when the
///   result contains non-text blocks, the raw content blocks as JSON.
/// - `is_error`: the server's `isError` flag.
/// - `images`: the image blocks, for models that can see them (the agent node
///   checks them against the tool image limits).
pub fn tool_output(result: CallToolResult) -> ToolOutput {
    let mut texts: Vec<String> = Vec::with_capacity(result.content.len());
    let mut has_binary = false;
    let mut images: Vec<ToolImage> = Vec::new();
    for block in &result.content {
        match block {
            ContentBlock::Text(t) => texts.push(t.text.clone()),
            ContentBlock::Image(img) => {
                has_binary = true;
                texts.push(format!("[image: {}]", img.mime_type));
                images.push(ToolImage::Base64 { media_type: img.mime_type.clone(), data_base64: img.data.clone() });
            }
            ContentBlock::Audio(audio) => {
                has_binary = true;
                texts.push(format!("[audio: {}]", audio.mime_type));
            }
            ContentBlock::Resource(res) => match &res.resource {
                ResourceContents::TextResourceContents { text, .. } => texts.push(text.clone()),
                ResourceContents::BlobResourceContents { uri, .. } => {
                    has_binary = true;
                    texts.push(format!("[resource: {uri}]"));
                }
                _ => has_binary = true,
            },
            ContentBlock::ResourceLink(link) => {
                has_binary = true;
                texts.push(format!("[resource: {}]", link.uri));
            }
            _ => has_binary = true,
        }
    }
    let data = match result.structured_content {
        Some(structured) => Value::from_json(structured),
        None if has_binary => serde_json::to_value(&result.content).map(Value::from_json).unwrap_or(Value::Null),
        None => Value::Null,
    };
    ToolOutput { text: texts.join("\n"), data, is_error: result.is_error.unwrap_or(false), images }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn maps_text_blocks() {
        let r = CallToolResult::success(vec![ContentBlock::text("a"), ContentBlock::text("b")]);
        let out = tool_output(r);
        assert_eq!(out.text, "a\nb");
        assert!(out.data.is_null());
        assert!(!out.is_error);
        assert!(out.images.is_empty());
    }

    #[test]
    fn maps_images_and_structured_content() {
        let r = CallToolResult::success(vec![ContentBlock::image("AAAA", "image/png")]);
        let out = tool_output(r);
        assert_eq!(out.text, "[image: image/png]");
        assert_eq!(out.images, vec![ToolImage::png("AAAA")]);
        let first = out.data.get_path(&["0", "mimeType"]);
        assert_eq!(first, Some(Value::from("image/png")));

        let mut r = CallToolResult::success(vec![ContentBlock::text("{}")]);
        r.structured_content = Some(serde_json::json!({"temp": 21}));
        let out = tool_output(r);
        assert_eq!(out.data.get_path(&["temp"]), Some(Value::Int(21)));
    }

    #[test]
    fn maps_error_flag() {
        let out = tool_output(CallToolResult::error(vec![ContentBlock::text("bad")]));
        assert!(out.is_error);
        assert_eq!(out.text, "bad");
    }

    #[test]
    fn maps_tool_spec() {
        let schema = serde_json::json!({"type": "object", "properties": {"q": {"type": "string"}}});
        let serde_json::Value::Object(map) = schema.clone() else { panic!("object") };
        let tool = Tool::new("search", "Search the web", Arc::new(map));
        let spec = tool_spec(&tool);
        assert_eq!(spec.name, "search");
        assert_eq!(spec.description, "Search the web");
        assert_eq!(spec.parameters, schema);
    }
}
