//! The `documents` tool provider: an agent writes PDF, Word and Excel files
//! into its workspace.
//!
//! | Tool | What it does |
//! | --- | --- |
//! | `create_pdf` | A PDF from Markdown. |
//! | `create_docx` | A Word document from Markdown (feature `office`). |
//! | `create_xlsx` | An Excel workbook from rows of cells (feature `office`). |
//!
//! The files are written through a `graphrt-files` workspace, so its path
//! rules, quota, type check and user scopes apply. The tools only create:
//! they read nothing and reach nothing outside the workspace.

use futures::future::BoxFuture;
use graphrt_core::tool::{ToolCallContext, ToolError, ToolOutput, ToolRuntime, ToolSpec};
use graphrt_core::value::Value;
use graphrt_files::{SCOPE_CONTEXT_KEY, Workspace, WorkspacePath};
use serde::Deserialize;
use serde_json::json;

use crate::markdown;
use crate::pdf::{self, PdfOptions};

/// Name hosts register the provider under: tools are `documents/create_pdf`...
pub const PROVIDER: &str = "documents";

/// Settings of the tools.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentToolsConfig {
    /// Most characters of Markdown one document is made from.
    pub max_content_chars: usize,
    /// Most pages of a PDF.
    pub max_pdf_pages: usize,
    /// Most sheets of a workbook.
    pub max_sheets: usize,
    /// Most cells of a workbook.
    pub max_cells: usize,
    /// Take the workspace scope from the agent's `tool_context` (key `scope`)
    /// and refuse calls without one, like `graphrt_files::FileTools::scoped_by_context`.
    pub require_scope: bool,
}

impl Default for DocumentToolsConfig {
    fn default() -> Self {
        Self {
            max_content_chars: 400_000,
            max_pdf_pages: 200,
            max_sheets: 20,
            max_cells: 200_000,
            require_scope: false,
        }
    }
}

/// The `documents` tools over one [`Workspace`].
#[derive(Debug)]
pub struct DocumentTools {
    workspace: Workspace,
    config: DocumentToolsConfig,
}

impl DocumentTools {
    /// Tools with the default limits.
    pub fn new(workspace: Workspace) -> Self {
        Self { workspace, config: DocumentToolsConfig::default() }
    }

    pub fn with_config(mut self, config: DocumentToolsConfig) -> Self {
        self.config = config;
        self
    }

    /// One private directory per user: the scope comes from the agent node's
    /// `tool_context: { documents: { scope: ... } }`, and calls without one are refused.
    pub fn scoped_by_context(mut self) -> Self {
        self.config.require_scope = true;
        self
    }

    fn view_for(&self, context: &ToolCallContext) -> Result<Workspace, String> {
        if !self.config.require_scope {
            return Ok(self.workspace.clone());
        }
        match context.secret(SCOPE_CONTEXT_KEY).filter(|scope| !scope.trim().is_empty()) {
            Some(scope) => Ok(self.workspace.scoped(scope)),
            None => Err(format!(
                "this workspace is per user: the agent node must set `tool_context: {{ {PROVIDER}: {{ {SCOPE_CONTEXT_KEY}: ... }} }}`"
            )),
        }
    }

    fn specs(&self) -> Vec<ToolSpec> {
        // A workspace that cannot be written to has nothing to offer here.
        if self.workspace.limits().read_only {
            return Vec::new();
        }
        let spec = |name: &str, description: &str, parameters: serde_json::Value| ToolSpec {
            name: name.to_owned(),
            description: description.to_owned(),
            parameters,
        };
        let overwrite = json!({ "type": "boolean", "description": "Replace an existing file (default false)." });
        let title = json!({ "type": "string", "description": "Title in the document properties." });
        let content = json!({
            "type": "string",
            "description": "The document as Markdown: # headings, paragraphs, - and 1. lists, | tables |, ``` code blocks, --- rules, **bold**, *italic*, `code`, and a line `\\newpage` for a page break."
        });
        let text_document = |extension: &str| {
            json!({
                "type": "object", "additionalProperties": false, "required": ["path", "content"],
                "properties": {
                    "path": { "type": "string", "description": format!("Path of the new file in the workspace, ending in `.{extension}`.") },
                    "content": content,
                    "title": title,
                    "overwrite": overwrite
                }
            })
        };
        #[allow(unused_mut)]
        let mut specs = vec![spec(
            "create_pdf",
            "Create a PDF file from Markdown and save it in the workspace. Western European characters only: others are written as `?`.",
            text_document("pdf"),
        )];
        #[cfg(feature = "office")]
        {
            specs.push(spec(
                "create_docx",
                "Create a Word document (.docx) from Markdown and save it in the workspace.",
                text_document("docx"),
            ));
            specs.push(spec(
                "create_xlsx",
                "Create an Excel workbook (.xlsx) from rows of cells and save it in the workspace. Text is stored as text, never as a formula.",
                json!({
                    "type": "object", "additionalProperties": false, "required": ["path", "sheets"],
                    "properties": {
                        "path": { "type": "string", "description": "Path of the new file in the workspace, ending in `.xlsx`." },
                        "sheets": {
                            "type": "array", "minItems": 1,
                            "items": {
                                "type": "object", "additionalProperties": false, "required": ["rows"],
                                "properties": {
                                    "name": { "type": "string", "description": "Name of the sheet." },
                                    "rows": {
                                        "type": "array",
                                        "description": "Rows of cells. A cell is a string, a number, true, false or null.",
                                        "items": { "type": "array" }
                                    },
                                    "header": { "type": "boolean", "description": "The first row is a header: bold and kept in view (default true)." }
                                }
                            }
                        },
                        "overwrite": overwrite
                    }
                }),
            ));
        }
        specs
    }

    async fn dispatch(
        &self,
        name: &str,
        arguments: serde_json::Value,
        workspace: &Workspace,
    ) -> Result<ToolOutput, String> {
        match name {
            "create_pdf" => self.create_pdf(workspace, parse(arguments)?).await,
            #[cfg(feature = "office")]
            "create_docx" => self.create_docx(workspace, parse(arguments)?).await,
            #[cfg(feature = "office")]
            "create_xlsx" => self.create_xlsx(workspace, parse(arguments)?).await,
            _ => Err(format!("unknown tool `{name}`")),
        }
    }

    /// The target of a document, checked before any work is done for it.
    async fn target(
        &self,
        workspace: &Workspace,
        raw: &str,
        extension: &str,
        overwrite: bool,
    ) -> Result<WorkspacePath, String> {
        let path = WorkspacePath::parse(raw.trim()).map_err(|e| format!("invalid path `{raw}`: {e}"))?;
        if !path.extension().is_some_and(|found| found.eq_ignore_ascii_case(extension)) {
            return Err(format!("`{path}` must end in `.{extension}`"));
        }
        if !overwrite && workspace.stat(&path).await.map_err(|e| e.to_string())?.is_some() {
            return Err(format!("`{path}` already exists: choose another path or set `overwrite`"));
        }
        Ok(path)
    }

    fn check_content(&self, content: &str) -> Result<(), String> {
        let limit = self.config.max_content_chars;
        if content.trim().is_empty() {
            return Err("`content` is empty".to_owned());
        }
        if content.len() > limit {
            return Err(format!("`content` is {} bytes, above the limit of {limit}", content.len()));
        }
        Ok(())
    }

    async fn create_pdf(&self, workspace: &Workspace, args: TextArgs) -> Result<ToolOutput, String> {
        self.check_content(&args.content)?;
        let overwrite = args.overwrite.unwrap_or(false);
        let path = self.target(workspace, &args.path, "pdf", overwrite).await?;
        let options = PdfOptions { title: args.title, max_pages: self.config.max_pdf_pages };
        let content = args.content;
        // Layout is CPU work: keep it off the async workers.
        let document = graphrt_files::blocking(move || pdf::render(&markdown::parse(&content), &options))
            .await?
            .map_err(|e| e.to_string())?;
        let (pages, replaced) = (document.pages, document.replaced_chars);
        let mut note = format!("{pages} page{}", if pages == 1 { "" } else { "s" });
        if replaced > 0 {
            note.push_str(&format!(
                ", {replaced} character{} outside the Western European set written as `?` (use create_docx for other scripts)",
                if replaced == 1 { "" } else { "s" }
            ));
        }
        store(workspace, &path, document.bytes, overwrite, &note).await
    }

    #[cfg(feature = "office")]
    async fn create_docx(&self, workspace: &Workspace, args: TextArgs) -> Result<ToolOutput, String> {
        self.check_content(&args.content)?;
        let overwrite = args.overwrite.unwrap_or(false);
        let path = self.target(workspace, &args.path, "docx", overwrite).await?;
        let options = crate::docx::DocxOptions { title: args.title };
        let content = args.content;
        let bytes = graphrt_files::blocking(move || crate::docx::render(&markdown::parse(&content), &options))
            .await?
            .map_err(|e| e.to_string())?;
        store(workspace, &path, bytes, overwrite, "Word document").await
    }

    #[cfg(feature = "office")]
    async fn create_xlsx(&self, workspace: &Workspace, args: SheetsArgs) -> Result<ToolOutput, String> {
        use crate::xlsx::{Cell, Sheet, XlsxLimits};
        let overwrite = args.overwrite.unwrap_or(false);
        let path = self.target(workspace, &args.path, "xlsx", overwrite).await?;
        let limits = XlsxLimits {
            max_sheets: self.config.max_sheets,
            max_cells: self.config.max_cells,
            ..XlsxLimits::default()
        };
        let cell = |value: serde_json::Value| match value {
            serde_json::Value::Null => Cell::Empty,
            serde_json::Value::Bool(value) => Cell::Bool(value),
            serde_json::Value::String(text) => Cell::Text(text),
            serde_json::Value::Number(number) => match (number.as_i64(), number.as_u64(), number.as_f64()) {
                (Some(whole), _, _) => Cell::integer(i128::from(whole)),
                (None, Some(whole), _) => Cell::integer(i128::from(whole)),
                (None, None, Some(float)) if float.is_finite() => Cell::Number(float),
                _ => Cell::Text(number.to_string()),
            },
            // A list or an object in a cell is shown as its JSON text.
            other => Cell::Text(other.to_string()),
        };
        let sheets: Vec<Sheet> = args
            .sheets
            .into_iter()
            .map(|sheet| Sheet {
                name: sheet.name.unwrap_or_default(),
                header: sheet.header.unwrap_or(true),
                rows: sheet.rows.into_iter().map(|row| row.into_iter().map(cell).collect()).collect(),
            })
            .collect();
        let (sheet_count, row_count) = (sheets.len(), sheets.iter().map(|sheet| sheet.rows.len()).sum::<usize>());
        let bytes =
            graphrt_files::blocking(move || crate::xlsx::render(&sheets, &limits)).await?.map_err(|e| e.to_string())?;
        let note = format!(
            "{sheet_count} sheet{}, {row_count} row{}",
            if sheet_count == 1 { "" } else { "s" },
            if row_count == 1 { "" } else { "s" }
        );
        store(workspace, &path, bytes, overwrite, &note).await
    }
}

/// Store a finished document and describe it. The file value in the result
/// reaches the `files` output of the node.
async fn store(
    workspace: &Workspace,
    path: &WorkspacePath,
    bytes: Vec<u8>,
    overwrite: bool,
    note: &str,
) -> Result<ToolOutput, String> {
    let stored = workspace.write(path, bytes, overwrite).await.map_err(|e| e.to_string())?;
    let text = format!("Created {path} ({}, {note}, {} bytes).", stored.media.mime, stored.entry.size);
    let data = Value::object([
        ("path", Value::from(stored.entry.path.as_str())),
        ("size", Value::Int(i64::try_from(stored.entry.size).unwrap_or(i64::MAX))),
        ("mime_type", Value::from(stored.media.mime)),
        ("file", Value::from(workspace.file_value(&stored))),
    ]);
    Ok(ToolOutput { text, data, ..ToolOutput::default() })
}

impl ToolRuntime for DocumentTools {
    fn list(&self) -> BoxFuture<'_, Result<Vec<ToolSpec>, ToolError>> {
        Box::pin(futures::future::ready(Ok(self.specs())))
    }

    fn call<'a>(
        &'a self,
        name: &'a str,
        arguments: serde_json::Value,
        context: &'a ToolCallContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            if !self.specs().iter().any(|spec| spec.name == name) {
                return Err(ToolError { tool: name.to_owned(), message: "unknown tool".to_owned() });
            }
            let result = match self.view_for(context) {
                Ok(workspace) => self.dispatch(name, arguments, &workspace).await,
                Err(reason) => Err(reason),
            };
            Ok(result.unwrap_or_else(ToolOutput::error))
        })
    }

    fn accepts_secrets(&self) -> bool {
        self.config.require_scope
    }
}

fn parse<T: serde::de::DeserializeOwned>(arguments: serde_json::Value) -> Result<T, String> {
    serde_json::from_value(arguments).map_err(|e| format!("invalid arguments: {e}"))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TextArgs {
    path: String,
    content: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    overwrite: Option<bool>,
}

#[cfg(feature = "office")]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SheetsArgs {
    path: String,
    sheets: Vec<SheetArgs>,
    #[serde(default)]
    overwrite: Option<bool>,
}

#[cfg(feature = "office")]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SheetArgs {
    #[serde(default)]
    name: Option<String>,
    rows: Vec<Vec<serde_json::Value>>,
    #[serde(default)]
    header: Option<bool>,
}
