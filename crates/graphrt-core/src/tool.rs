//! Tool contract shared by the `tool` node, the `agent` node and tool
//! providers (native Rust tools, MCP servers, HTTP tools...).

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::value::Value;

/// Description of a callable tool, exposed to models for tool calling.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Unique name within a [`ToolRuntime`] (e.g. `search` or `github.create_issue`).
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments object.
    pub parameters: serde_json::Value,
}

/// Result of a tool call.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolOutput {
    /// Text handed back to a model.
    pub text: String,
    /// Structured result, if the tool returns one.
    pub data: Value,
    /// Whether the tool reported an error (the agent loop may still continue).
    pub is_error: bool,
    /// Images shown to the model next to `text` (see [`check_images`] for limits).
    pub images: Vec<ToolImage>,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: text.into(), ..Self::default() }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self { text: text.into(), is_error: true, ..Self::default() }
    }

    /// Attach an image.
    pub fn with_image(mut self, image: ToolImage) -> Self {
        self.images.push(image);
        self
    }
}

/// Most images one tool result may carry.
pub const MAX_TOOL_IMAGES: usize = 4;
/// Largest decoded size of one tool result image, in bytes (5 MiB).
pub const MAX_TOOL_IMAGE_BYTES: usize = 5 * 1024 * 1024;
/// Media types accepted for tool result images (supported by every built-in provider).
pub const TOOL_IMAGE_MEDIA_TYPES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];

/// An image returned by a tool.
///
/// Serialized as `{"media_type", "data_base64"}` or `{"url"}` (the plugin
/// protocol's `tool.call` `images` entries).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolImage {
    /// Inline bytes, base64 encoded (standard alphabet).
    Base64 { media_type: String, data_base64: String },
    /// A remote image (`https://` only).
    Url { url: String },
}

impl ToolImage {
    /// An inline PNG from base64 text.
    pub fn png(data_base64: impl Into<String>) -> Self {
        Self::Base64 { media_type: "image/png".into(), data_base64: data_base64.into() }
    }

    /// An inline JPEG from base64 text.
    pub fn jpeg(data_base64: impl Into<String>) -> Self {
        Self::Base64 { media_type: "image/jpeg".into(), data_base64: data_base64.into() }
    }

    /// The image as a URL: `data:<media_type>;base64,<data>` or the remote URL.
    pub fn to_url(&self) -> String {
        match self {
            ToolImage::Base64 { media_type, data_base64 } => format!("data:{media_type};base64,{data_base64}"),
            ToolImage::Url { url } => url.clone(),
        }
    }

    /// Check this image against the media type, encoding and size limits.
    pub fn check(&self) -> Result<(), ToolImageError> {
        match self {
            ToolImage::Url { url } => {
                if url.len() > 8 && url.get(..8).is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://")) {
                    Ok(())
                } else {
                    Err(ToolImageError::new("image URLs must start with https:// (send inline images as base64)"))
                }
            }
            ToolImage::Base64 { media_type, data_base64 } => {
                if !TOOL_IMAGE_MEDIA_TYPES.contains(&media_type.as_str()) {
                    return Err(ToolImageError::new(format!(
                        "image media type `{media_type}` is not supported (use {})",
                        TOOL_IMAGE_MEDIA_TYPES.join(", ")
                    )));
                }
                let decoded = base64_decoded_len(data_base64)
                    .ok_or_else(|| ToolImageError::new("image data is not valid base64"))?;
                if decoded == 0 {
                    return Err(ToolImageError::new("image data is empty"));
                }
                if decoded > MAX_TOOL_IMAGE_BYTES {
                    return Err(ToolImageError::new(format!(
                        "image is {decoded} bytes, above the {MAX_TOOL_IMAGE_BYTES}-byte limit"
                    )));
                }
                Ok(())
            }
        }
    }
}

/// A tool result image that breaks a limit.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ToolImageError(pub String);

impl ToolImageError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Check the images of one tool result: at most [`MAX_TOOL_IMAGES`], each
/// passing [`ToolImage::check`].
pub fn check_images(images: &[ToolImage]) -> Result<(), ToolImageError> {
    if images.len() > MAX_TOOL_IMAGES {
        return Err(ToolImageError::new(format!(
            "{} images in one tool result, above the limit of {MAX_TOOL_IMAGES}",
            images.len()
        )));
    }
    for (index, image) in images.iter().enumerate() {
        image.check().map_err(|e| ToolImageError::new(format!("image #{index}: {e}")))?;
    }
    Ok(())
}

/// Decoded length of standard base64 text (padding optional), or `None` when
/// the text is not base64. Validates without allocating.
fn base64_decoded_len(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let body = bytes.strip_suffix(b"==").or_else(|| bytes.strip_suffix(b"=")).unwrap_or(bytes);
    let alphabet = |b: &u8| b.is_ascii_alphanumeric() || *b == b'+' || *b == b'/';
    if !body.iter().all(alphabet) {
        return None;
    }
    let padded = body.len() != bytes.len();
    if padded && !bytes.len().is_multiple_of(4) {
        return None;
    }
    match body.len() % 4 {
        1 => None,
        rest => Some(body.len() / 4 * 3 + rest.saturating_sub(1)),
    }
}

/// Most files [`collect_files`] returns for one tool result.
pub const MAX_TOOL_FILES: usize = 16;
/// How deep [`collect_files`] looks into arrays and objects (the value itself is depth 0).
const MAX_FILE_SCAN_DEPTH: usize = 3;

/// Files in a tool's structured result ([`ToolOutput::data`]), in document
/// order: the value itself when it is a file, otherwise files found in its
/// array items and object fields, at most three levels down
/// (`{"file": ...}`, `{"files": [...]}`). At most [`MAX_TOOL_FILES`].
///
/// Only native tools can return file values: JSON from plugins, MCP servers
/// and models is converted with [`Value::from_json`], which never creates one.
///
/// ```
/// use graphrt_core::tool::collect_files;
/// use graphrt_core::value::{File, Value};
///
/// let file = File { marker: true, name: "report.csv".into(), ..File::default() };
/// let data = Value::object([("path", Value::from("report.csv")), ("file", Value::from(file))]);
/// assert_eq!(collect_files(&data).len(), 1);
/// // Untrusted JSON that looks like a file is not one.
/// let forged = Value::from_json(serde_json::json!({"file": {"$file": true, "name": "x"}}));
/// assert!(collect_files(&forged).is_empty());
/// ```
pub fn collect_files(data: &Value) -> Vec<Arc<crate::value::File>> {
    let mut files = Vec::new();
    collect_files_into(data, 0, &mut files);
    files
}

fn collect_files_into(value: &Value, depth: usize, files: &mut Vec<Arc<crate::value::File>>) {
    if files.len() >= MAX_TOOL_FILES {
        return;
    }
    match value {
        Value::File(file) => files.push(Arc::clone(file)),
        Value::Array(items) if depth < MAX_FILE_SCAN_DEPTH => {
            items.iter().for_each(|item| collect_files_into(item, depth + 1, files));
        }
        Value::Object(fields) if depth < MAX_FILE_SCAN_DEPTH => {
            fields.values().for_each(|field| collect_files_into(field, depth + 1, files));
        }
        _ => {}
    }
}

/// Error calling a tool (transport, unknown tool...).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("tool `{tool}`: {message}")]
pub struct ToolError {
    pub tool: String,
    pub message: String,
}

/// Per-call values a tool receives next to its arguments, without the model
/// ever seeing them (e.g. the end user's JWT for a database tool).
///
/// The `agent` node fills it from its `tool_context` config, only for
/// providers whose [`ToolRuntime::accepts_secrets`] is `true`. The values are
/// plaintext: they are never written to events, outputs, snapshots or traces,
/// and `Debug` shows only the names. The type deliberately does not implement
/// `Serialize`, so it cannot end up in a recorded value by accident.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ToolCallContext {
    /// Secret name → plaintext value.
    pub secrets: BTreeMap<String, String>,
}

static EMPTY_TOOL_CALL_CONTEXT: ToolCallContext = ToolCallContext { secrets: BTreeMap::new() };

impl ToolCallContext {
    /// A shared empty context, for callers that have nothing to pass.
    pub fn empty() -> &'static ToolCallContext {
        &EMPTY_TOOL_CALL_CONTEXT
    }

    /// Whether there is nothing in the context.
    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    /// The plaintext of secret `name`.
    pub fn secret(&self, name: &str) -> Option<&str> {
        self.secrets.get(name).map(String::as_str)
    }
}

impl fmt::Debug for ToolCallContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolCallContext").field("secrets", &self.secrets.keys().collect::<Vec<_>>()).finish()
    }
}

/// A source of tools.
pub trait ToolRuntime: Send + Sync + fmt::Debug {
    /// Tools currently available.
    fn list(&self) -> BoxFuture<'_, Result<Vec<ToolSpec>, ToolError>>;

    /// Invoke a tool with a JSON arguments object and a per-call context.
    ///
    /// `context` is empty unless this runtime [accepts secrets](ToolRuntime::accepts_secrets)
    /// and the caller configured some. Runtimes that cannot forward it ignore it.
    fn call<'a>(
        &'a self,
        name: &'a str,
        arguments: serde_json::Value,
        context: &'a ToolCallContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>>;

    /// Whether this runtime may receive plaintext secrets in a [`ToolCallContext`].
    ///
    /// `false` by default. The plugin runtime returns `true` only for plugins that
    /// request `permissions.secrets` and that the operator trusts.
    fn accepts_secrets(&self) -> bool {
        false
    }
}

/// Service registered in [`crate::node::Services`]: named tool providers
/// (e.g. `builtin`, `mcp:github`). Nodes reference tools as `provider/tool`.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    providers: indexmap::IndexMap<String, Arc<dyn ToolRuntime>>,
}

impl fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.providers.keys()).finish()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, provider: impl Into<String>, runtime: Arc<dyn ToolRuntime>) -> &mut Self {
        self.providers.insert(provider.into(), runtime);
        self
    }

    pub fn get(&self, provider: &str) -> Option<&Arc<dyn ToolRuntime>> {
        self.providers.get(provider)
    }

    pub fn providers(&self) -> impl Iterator<Item = (&str, &Arc<dyn ToolRuntime>)> {
        self.providers.iter().map(|(k, v)| (k.as_str(), v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_context_debug_hides_values() {
        assert!(ToolCallContext::empty().is_empty());
        let mut context = ToolCallContext::default();
        context.secrets.insert("user_jwt".into(), "eyJ-secret-token".into());
        assert_eq!(context.secret("user_jwt"), Some("eyJ-secret-token"));
        assert_eq!(context.secret("other"), None);
        let debug = format!("{context:?}");
        assert!(debug.contains("user_jwt") && !debug.contains("eyJ-secret-token"), "{debug}");
    }

    #[test]
    fn files_are_collected_in_order_up_to_the_cap() {
        let file =
            |name: &str| Value::from(crate::value::File { marker: true, name: name.into(), ..Default::default() });
        let names = |data: &Value| collect_files(data).iter().map(|f| f.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&file("a")), ["a"]);
        let nested = Value::object([
            ("file", file("a")),
            ("files", Value::array([file("b"), file("c")])),
            ("text", Value::from("x")),
        ]);
        assert_eq!(names(&nested), ["a", "b", "c"]);
        // Four levels down is out of reach.
        let deep = Value::array([Value::array([Value::array([Value::array([file("deep")])])])]);
        assert!(names(&deep).is_empty());
        let many = Value::array((0..MAX_TOOL_FILES + 5).map(|i| file(&i.to_string())));
        assert_eq!(collect_files(&many).len(), MAX_TOOL_FILES);
        assert!(collect_files(&Value::Null).is_empty());
    }

    #[test]
    fn constructors_have_no_images() {
        assert!(ToolOutput::text("a").images.is_empty());
        let err = ToolOutput::error("b");
        assert!(err.is_error && err.images.is_empty());
        let out = ToolOutput::text("shot").with_image(ToolImage::png("aGVsbG8="));
        assert_eq!(out.images.len(), 1);
    }

    #[test]
    fn image_urls() {
        assert_eq!(ToolImage::png("AAAA").to_url(), "data:image/png;base64,AAAA");
        assert_eq!(ToolImage::Url { url: "https://x/a.png".into() }.to_url(), "https://x/a.png");
    }

    #[test]
    fn serde_shapes() {
        let images: Vec<ToolImage> = serde_json::from_value(serde_json::json!([
            {"media_type": "image/png", "data_base64": "AAAA"},
            {"url": "https://x/a.png"}
        ]))
        .unwrap();
        assert_eq!(images[0], ToolImage::png("AAAA"));
        assert_eq!(images[1], ToolImage::Url { url: "https://x/a.png".into() });
        assert!(serde_json::from_value::<ToolImage>(serde_json::json!({"data": "AAAA"})).is_err());
    }

    #[test]
    fn base64_lengths() {
        assert_eq!(base64_decoded_len("aGVsbG8="), Some(5));
        assert_eq!(base64_decoded_len("aGVsbG8"), Some(5));
        assert_eq!(base64_decoded_len("aGVsbA=="), Some(4));
        assert_eq!(base64_decoded_len("aGVs"), Some(3));
        assert_eq!(base64_decoded_len("aGVsb"), None);
        assert_eq!(base64_decoded_len("aGV="), Some(2));
        assert_eq!(base64_decoded_len("aG V"), None);
        assert_eq!(base64_decoded_len("aGVsbG8=="), None);
        assert_eq!(base64_decoded_len(""), Some(0));
    }

    #[test]
    fn limits_are_enforced() {
        assert!(check_images(&[ToolImage::png("aGVsbG8="), ToolImage::jpeg("aGVs")]).is_ok());
        let too_many = vec![ToolImage::png("aGVs"); MAX_TOOL_IMAGES + 1];
        assert!(check_images(&too_many).unwrap_err().to_string().contains("limit of 4"));
        let bmp = ToolImage::Base64 { media_type: "image/bmp".into(), data_base64: "aGVs".into() };
        assert!(check_images(&[bmp]).unwrap_err().to_string().contains("image/bmp"));
        assert!(ToolImage::png("not base64!").check().is_err());
        assert!(ToolImage::png("").check().is_err());
        let big = "A".repeat((MAX_TOOL_IMAGE_BYTES / 3 + 1) * 4);
        assert!(ToolImage::png(big).check().unwrap_err().to_string().contains("limit"));
        let exact = "A".repeat(MAX_TOOL_IMAGE_BYTES / 3 * 4);
        assert!(ToolImage::png(exact).check().is_ok());
        assert!(ToolImage::Url { url: "http://x/a.png".into() }.check().is_err());
        assert!(ToolImage::Url { url: "data:image/png;base64,AAAA".into() }.check().is_err());
        assert!(ToolImage::Url { url: "HTTPS://x/a.png".into() }.check().is_ok());
    }
}
