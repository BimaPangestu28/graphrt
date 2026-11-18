//! The `document-extractor` node.

use std::sync::Arc;

use futures::future::BoxFuture;
use serde::Deserialize;

use graphrt_core::catalog::{NodeCategory, NodeDescriptor, OutputDescriptor};
use graphrt_core::dsl::NodeDoc;
use graphrt_core::file::{DataUrlResolver, FileError, FileResolver};
use graphrt_core::node::{ConfigError, Node, NodeContext, NodeError, NodeOutput, NodeResult, Registry, parse_config};
use graphrt_core::value::File;
use graphrt_core::{Selector, Value, ValueType};

use crate::{DocError, Extraction, Options, OutputFormat};

/// Node kind handled by this crate.
pub const KIND: &str = "document-extractor";

/// Host-level limits for document extraction. Register in
/// [`graphrt_core::Services`] to override the defaults; workflow config can only
/// lower them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DocLimits {
    /// Maximum size of one input file in bytes.
    pub max_file_bytes: u64,
    /// Maximum characters of extracted text per file (longer text is truncated).
    pub max_output_chars: usize,
    /// Maximum PDF pages / slides / EPUB chapters.
    pub max_pages: usize,
    /// Maximum spreadsheet sheets.
    pub max_sheets: usize,
    /// Maximum rows per sheet or CSV file.
    pub max_rows: usize,
    /// Maximum columns of any table.
    pub max_cols: usize,
    /// Maximum non-empty cells per spreadsheet sheet.
    pub max_cells: usize,
    /// Maximum zip entries.
    pub max_zip_entries: usize,
    /// Maximum total decompressed bytes from a zip container.
    pub max_decompressed_bytes: u64,
}

impl Default for DocLimits {
    fn default() -> Self {
        let opts = Options::default();
        Self {
            max_file_bytes: 50 * 1024 * 1024,
            max_output_chars: opts.max_output_chars,
            max_pages: opts.max_pages,
            max_sheets: opts.max_sheets,
            max_rows: opts.max_rows,
            max_cols: opts.max_cols,
            max_cells: opts.max_cells,
            max_zip_entries: opts.max_zip_entries,
            max_decompressed_bytes: opts.max_decompressed_bytes,
        }
    }
}

impl DocLimits {
    fn options(&self, format: OutputFormat) -> Options {
        Options {
            format,
            max_output_chars: self.max_output_chars,
            max_pages: self.max_pages,
            max_sheets: self.max_sheets,
            max_rows: self.max_rows,
            max_cols: self.max_cols,
            max_cells: self.max_cells,
            max_zip_entries: self.max_zip_entries,
            max_decompressed_bytes: self.max_decompressed_bytes,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct Config {
    variable_selector: Selector,
    #[serde(default)]
    max_bytes: Option<u64>,
    #[serde(default)]
    output_format: OutputFormat,
}

/// Extracts text from a file or a list of files.
///
/// Output `text` is a string for a single file and an array of strings (input
/// order) for a file list. Per-file details (format, characters, truncation) are
/// reported in the `files` metadata entry.
#[derive(Debug, Clone)]
pub struct DocumentExtractorNode {
    selector: Selector,
    max_bytes: Option<u64>,
    format: OutputFormat,
}

impl DocumentExtractorNode {
    /// Build from a node document.
    pub fn from_doc(doc: &NodeDoc) -> Result<Self, ConfigError> {
        let cfg: Config = parse_config(doc)?;
        if cfg.max_bytes == Some(0) {
            return Err(ConfigError::new(doc, "`max_bytes` must be greater than zero"));
        }
        Ok(Self { selector: cfg.variable_selector, max_bytes: cfg.max_bytes, format: cfg.output_format })
    }
}

impl Node for DocumentExtractorNode {
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(async move {
            let value = ctx
                .get(&self.selector)
                .filter(|v| !v.is_null())
                .ok_or_else(|| NodeError::invalid_input(format!("variable `{}` is not set", self.selector)))?;
            let (files, many) = match &value {
                Value::File(f) => (vec![Arc::clone(f)], false),
                Value::Array(items) => {
                    let mut files = Vec::with_capacity(items.len());
                    for (i, item) in items.iter().enumerate() {
                        match item {
                            Value::File(f) => files.push(Arc::clone(f)),
                            other => {
                                return Err(NodeError::invalid_input(format!(
                                    "`{}` item {i} is a {}, expected a file",
                                    self.selector,
                                    other.type_name()
                                )));
                            }
                        }
                    }
                    (files, true)
                }
                other => {
                    return Err(NodeError::invalid_input(format!(
                        "`{}` is a {}, expected a file or a list of files",
                        self.selector,
                        other.type_name()
                    )));
                }
            };

            let limits = ctx.service::<DocLimits>().copied().unwrap_or_default();
            let max_bytes = self.max_bytes.map_or(limits.max_file_bytes, |m| m.min(limits.max_file_bytes));
            let opts = limits.options(self.format);
            let resolver = ctx.service::<Arc<dyn FileResolver>>().cloned();

            let mut texts = Vec::with_capacity(files.len());
            let mut details = Vec::with_capacity(files.len());
            for file in &files {
                let extraction = extract_file(resolver.as_ref(), file, max_bytes, &opts).await?;
                details.push(Value::object([
                    ("name", Value::from(file.name.as_str())),
                    ("format", Value::from(extraction.format.name())),
                    ("chars", Value::Int(i64::try_from(extraction.text.chars().count()).unwrap_or(i64::MAX))),
                    ("truncated", Value::Bool(extraction.truncated)),
                ]));
                texts.push(Value::from(extraction.text));
            }

            let text = if many { Value::array(texts) } else { texts.into_iter().next().unwrap_or_default() };
            Ok(NodeOutput::new().with("text", text).meta("files", Value::array(details)).into())
        })
    }

    fn inputs(&self) -> Vec<Selector> {
        vec![self.selector.clone()]
    }
}

/// Read and extract one file.
async fn extract_file(
    resolver: Option<&Arc<dyn FileResolver>>,
    file: &File,
    max_bytes: u64,
    opts: &Options,
) -> Result<Extraction, NodeError> {
    let read = match resolver {
        Some(r) => r.read(file, max_bytes).await,
        None => DataUrlResolver.read(file, max_bytes).await,
    };
    let content = read.map_err(|e| file_error(file, e, resolver.is_none()))?;
    let mime = content.mime_type.or_else(|| file.mime_type.clone());
    let filename = match &file.extension {
        Some(ext) if crate::detect::extension(&file.name).is_none() => {
            format!("{}.{}", file.name, ext.trim_start_matches('.'))
        }
        _ => file.name.clone(),
    };
    let name = file.name.clone();
    run_extraction(content.bytes, mime, filename, opts.clone())
        .await
        .map_err(|e| NodeError::fatal(e.kind(), format!("file `{name}`: {e}")))
}

/// Extraction is CPU-bound: run it on Tokio's blocking pool when available.
async fn run_extraction(
    bytes: Vec<u8>,
    mime: Option<String>,
    filename: String,
    opts: Options,
) -> Result<Extraction, DocError> {
    let work = move || crate::extract_detailed(&bytes, mime.as_deref(), Some(&filename), &opts);
    #[cfg(feature = "tokio")]
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        return match handle.spawn_blocking(work).await {
            Ok(result) => result,
            Err(e) => Err(DocError::malformed("document", format!("extraction task failed: {e}"))),
        };
    }
    work()
}

fn file_error(file: &File, err: FileError, no_resolver: bool) -> NodeError {
    match err {
        FileError::Unsupported(_) if no_resolver => NodeError::fatal(
            "missing_service",
            format!("no file resolver configured to read file `{}` (only data URLs work without one)", file.name),
        ),
        FileError::Unsupported(_) => {
            NodeError::fatal("unsupported_file", format!("no resolver can read file `{}`", file.name))
        }
        FileError::TooLarge { name, limit } => {
            NodeError::fatal("file_too_large", format!("file `{name}` exceeds the {limit}-byte limit"))
        }
        e @ (FileError::NotFound { .. } | FileError::Denied { .. }) => {
            NodeError::fatal("file_unavailable", e.to_string())
        }
        e @ FileError::Io { .. } => NodeError::new("file_io", e.to_string()),
        e => NodeError::invalid_input(e.to_string()),
    }
}

/// Catalog descriptor for the node.
pub fn descriptor() -> NodeDescriptor {
    NodeDescriptor::new(KIND, "Document Extractor", NodeCategory::Knowledge)
        .describe(
            "Extracts text from PDF, Word, PowerPoint, Excel/ODS, EPUB, HTML, e-mail, subtitle, \
             CSV and plain-text files.",
        )
        .schema(serde_json::json!({
            "type": "object",
            "required": ["variable_selector"],
            "properties": {
                "variable_selector": {
                    "description": "Selector of a file or a list of files.",
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 2
                },
                "max_bytes": {
                    "description": "Per-file size limit in bytes (cannot exceed the host limit).",
                    "type": "integer",
                    "minimum": 1
                },
                "output_format": {
                    "description": "`markdown` renders tables and headings as Markdown.",
                    "type": "string",
                    "enum": ["text", "markdown"],
                    "default": "text"
                }
            }
        }))
        .output(
            OutputDescriptor::new("text", ValueType::Any)
                .describe("Extracted text: a string for one file, an array of strings for a file list."),
        )
        .icon("file-text")
        .source("graphrt-doc")
}

/// Register the `document-extractor` node.
pub fn register(registry: &mut Registry) {
    registry.register_lazily_described(KIND, descriptor, |doc| {
        Ok(Arc::new(DocumentExtractorNode::from_doc(doc)?) as Arc<dyn Node>)
    });
}
