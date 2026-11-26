//! # graphrt-doc
//!
//! Text extraction from documents, as a library ([`extract`]) and as the
//! `document-extractor` workflow node ([`register`]).
//!
//! Everything is pure Rust. Formats and the features that enable them:
//!
//! | Format | Feature |
//! | --- | --- |
//! | txt, md, json, yaml, xml, csv, tsv, html, vtt, srt | always |
//! | pdf | `pdf` |
//! | docx, pptx | `office` |
//! | xlsx, xls, ods | `spreadsheet` |
//! | epub | `epub` |
//! | eml | `email` |
//!
//! The format is chosen from the MIME type, then the file extension, then the
//! content itself. Archives are read with entry-count and decompressed-size
//! limits (zip-bomb protection), and parsers that are known to panic on
//! malformed input are isolated with `catch_unwind` (effective only in builds
//! that unwind on panic).

use serde::{Deserialize, Serialize};

mod binary;
pub mod detect;
mod node;
#[cfg(feature = "pdf")]
mod pdf_budget;
pub mod text;
#[cfg(feature = "zip-formats")]
mod zipfmt;

pub use detect::Format;
pub use node::{DocLimits, DocumentExtractorNode, KIND, descriptor, register};

/// Shape of the extracted text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    /// Plain text; tables become tab-separated lines.
    #[default]
    Text,
    /// Markdown; tables become Markdown tables, headings keep their level.
    Markdown,
}

/// Extraction options and limits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub format: OutputFormat,
    /// Output longer than this many characters is truncated.
    pub max_output_chars: usize,
    /// Maximum PDF pages, presentation slides or EPUB chapters.
    pub max_pages: usize,
    /// Maximum spreadsheet sheets.
    pub max_sheets: usize,
    /// Maximum rows per spreadsheet sheet (and per CSV/TSV file).
    pub max_rows: usize,
    /// Maximum columns of any table (spreadsheet sheet, CSV/TSV, DOCX table).
    pub max_cols: usize,
    /// Maximum non-empty cells across a spreadsheet sheet. Spreadsheets whose
    /// used range would exceed this are rejected before they are loaded.
    pub max_cells: usize,
    /// Maximum entries in a zip container.
    pub max_zip_entries: usize,
    /// Maximum total decompressed bytes read from a zip container.
    pub max_decompressed_bytes: u64,
    /// Start every PDF page with a [`page_marker`] so indexers can tell which
    /// page text came from (off by default: the markers are not meant for readers).
    pub page_markers: bool,
}

/// First character of a page marker (Unicode private use, never in real text).
pub const PAGE_MARKER_START: char = '\u{E000}';
/// Last character of a page marker.
pub const PAGE_MARKER_END: char = '\u{E001}';

/// The marker that starts page `number` (1-based) with [`Options::page_markers`]:
/// [`PAGE_MARKER_START`], the number in decimal, [`PAGE_MARKER_END`].
pub fn page_marker(number: u32) -> String {
    format!("{PAGE_MARKER_START}{number}{PAGE_MARKER_END}")
}

impl Default for Options {
    fn default() -> Self {
        Self {
            format: OutputFormat::Text,
            max_output_chars: 5_000_000,
            max_pages: 2_000,
            max_sheets: 100,
            max_rows: 200_000,
            max_cols: 2_048,
            max_cells: 2_000_000,
            max_zip_entries: 10_000,
            max_decompressed_bytes: 256 * 1024 * 1024,
            page_markers: false,
        }
    }
}

impl Options {
    pub fn with_format(mut self, format: OutputFormat) -> Self {
        self.format = format;
        self
    }

    pub fn with_max_output_chars(mut self, max: usize) -> Self {
        self.max_output_chars = max;
        self
    }

    /// See [`Options::page_markers`].
    pub fn with_page_markers(mut self, page_markers: bool) -> Self {
        self.page_markers = page_markers;
        self
    }
}

/// Extraction failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DocError {
    /// The content type could not be identified or is not a document format.
    #[error("unsupported document format: {detected}")]
    Unsupported { detected: String },
    /// The format is known but its cargo feature is not enabled in this build.
    #[error("{format} documents are not supported by this build (enable the `{feature}` feature)")]
    FeatureDisabled { format: Format, feature: &'static str },
    /// The document could not be parsed.
    #[error("malformed {format} document: {message}")]
    Malformed { format: String, message: String },
    /// A size limit was exceeded.
    #[error("document exceeds limits: {0}")]
    LimitExceeded(String),
}

impl DocError {
    #[cfg_attr(
        not(any(
            feature = "pdf",
            feature = "spreadsheet",
            feature = "zip-formats",
            feature = "email",
            feature = "tokio"
        )),
        allow(dead_code)
    )]
    pub(crate) fn malformed(format: &str, err: impl std::fmt::Display) -> Self {
        DocError::Malformed { format: format.to_owned(), message: err.to_string() }
    }

    /// Stable machine-readable category.
    pub fn kind(&self) -> &'static str {
        match self {
            DocError::Unsupported { .. } | DocError::FeatureDisabled { .. } => "unsupported_format",
            DocError::Malformed { .. } => "malformed_document",
            DocError::LimitExceeded(_) => "limit_exceeded",
        }
    }
}

/// Result of [`extract_detailed`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extraction {
    pub text: String,
    pub format: Format,
    /// Whether the text was cut at [`Options::max_output_chars`].
    pub truncated: bool,
}

/// Extract the text of a document.
///
/// `mime` and `filename` are hints; either may be `None`. Text longer than
/// [`Options::max_output_chars`] is cut; use [`extract_detailed`] to learn
/// whether that happened.
pub fn extract(bytes: &[u8], mime: Option<&str>, filename: Option<&str>, opts: &Options) -> Result<String, DocError> {
    extract_detailed(bytes, mime, filename, opts).map(|e| e.text)
}

/// Like [`extract`], also reporting the detected format and truncation.
pub fn extract_detailed(
    bytes: &[u8],
    mime: Option<&str>,
    filename: Option<&str>,
    opts: &Options,
) -> Result<Extraction, DocError> {
    let format = resolve_format(bytes, mime, filename, opts)?;
    let text = extract_as(format, bytes, opts)?;
    let (text, truncated) = truncate_chars(text, opts.max_output_chars);
    Ok(Extraction { text, format, truncated })
}

/// Determine the format: MIME type, then extension, then content sniffing.
pub fn resolve_format(
    bytes: &[u8],
    mime: Option<&str>,
    filename: Option<&str>,
    opts: &Options,
) -> Result<Format, DocError> {
    let claimed = mime
        .and_then(detect::from_mime)
        .or_else(|| filename.and_then(detect::extension).and_then(detect::from_extension));
    if let Some(f) = claimed {
        if !f.is_textual() {
            return Ok(f);
        }
        // A text label on binary content (a PDF uploaded as text/plain, a zip named
        // .txt) would otherwise "succeed" with garbage text.
        if detect::sniff(bytes).is_some_and(|s| !s.is_textual()) || detect::is_zip(bytes) {
            return sniffed_binary(bytes, opts);
        }
        if !bytes.is_empty() && !detect::looks_textual(bytes) {
            return Err(DocError::Unsupported { detected: format!("binary data labelled as {f}") });
        }
        return Ok(f);
    }
    if let Some(f) = detect::sniff(bytes) {
        if f == Format::Xls && !detect::ole_is_workbook(bytes) {
            return Err(DocError::Unsupported {
                detected: "legacy OLE document (.doc / .ppt / .msg) — only spreadsheets are supported".into(),
            });
        }
        return Ok(f);
    }
    if detect::is_zip(bytes) {
        #[cfg(feature = "zip-formats")]
        if let Some(f) = zipfmt::identify(bytes, opts)? {
            return Ok(f);
        }
        return Err(DocError::Unsupported { detected: "zip archive".into() });
    }
    let _ = opts;
    if !bytes.is_empty() && detect::looks_textual(bytes) {
        return Ok(Format::PlainText);
    }
    let detected = match (mime, filename.and_then(detect::extension)) {
        (Some(m), _) => m.to_owned(),
        (None, Some(ext)) => format!(".{ext}"),
        (None, None) if bytes.is_empty() => "empty file".to_owned(),
        (None, None) => "binary data".to_owned(),
    };
    Err(DocError::Unsupported { detected })
}

/// Resolve content that sniffing identified as binary, ignoring a text label.
fn sniffed_binary(bytes: &[u8], opts: &Options) -> Result<Format, DocError> {
    resolve_format(bytes, None, None, opts)
}

/// Extract text from bytes known to be in `format`.
pub fn extract_as(format: Format, bytes: &[u8], opts: &Options) -> Result<String, DocError> {
    #[allow(unused_variables)]
    let disabled = |feature: &'static str| DocError::FeatureDisabled { format, feature };
    match format {
        Format::PlainText | Format::Markdown | Format::Json | Format::Yaml | Format::Xml => {
            Ok(text::decode(bytes).trim_end().to_owned())
        }
        Format::Csv | Format::Tsv => {
            let delimiter = if format == Format::Csv { ',' } else { '\t' };
            let rows = text::parse_delimited(&text::decode(bytes), delimiter);
            if rows.len() > opts.max_rows {
                return Err(DocError::LimitExceeded(format!("{} rows (limit {})", rows.len(), opts.max_rows)));
            }
            text::render_table(&rows, opts)
        }
        Format::Html => Ok(text::html_to_text(&text::decode(bytes), opts.format)),
        Format::Vtt | Format::Srt => Ok(text::subtitles(&text::decode(bytes))),
        Format::Pdf => {
            #[cfg(feature = "pdf")]
            return binary::pdf(bytes, opts);
            #[cfg(not(feature = "pdf"))]
            return Err(disabled("pdf"));
        }
        Format::Docx => {
            #[cfg(feature = "office")]
            return zipfmt::docx(bytes, opts);
            #[cfg(not(feature = "office"))]
            return Err(disabled("office"));
        }
        Format::Pptx => {
            #[cfg(feature = "office")]
            return zipfmt::pptx(bytes, opts);
            #[cfg(not(feature = "office"))]
            return Err(disabled("office"));
        }
        Format::Epub => {
            #[cfg(feature = "epub")]
            return zipfmt::epub(bytes, opts);
            #[cfg(not(feature = "epub"))]
            return Err(disabled("epub"));
        }
        Format::Xlsx | Format::Xls | Format::Ods => {
            #[cfg(feature = "spreadsheet")]
            return binary::spreadsheet(bytes, format, opts);
            #[cfg(not(feature = "spreadsheet"))]
            return Err(disabled("spreadsheet"));
        }
        Format::Eml => {
            #[cfg(feature = "email")]
            return binary::email(bytes, opts);
            #[cfg(not(feature = "email"))]
            return Err(disabled("email"));
        }
    }
}

/// Cut `text` to at most `max` characters (on a char boundary).
fn truncate_chars(mut text: String, max: usize) -> (String, bool) {
    match text.char_indices().nth(max) {
        Some((byte_index, _)) => {
            text.truncate(byte_index);
            (text, true)
        }
        None => (text, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_respects_char_boundaries() {
        let (t, cut) = truncate_chars("héllo".to_owned(), 2);
        assert_eq!((t.as_str(), cut), ("hé", true));
        let (t, cut) = truncate_chars("hi".to_owned(), 2);
        assert_eq!((t.as_str(), cut), ("hi", false));
    }

    #[test]
    fn format_resolution_order() {
        let opts = Options::default();
        // MIME wins over extension.
        assert_eq!(resolve_format(b"x", Some("text/csv"), Some("a.json"), &opts), Ok(Format::Csv));
        // Generic MIME falls through to the extension.
        assert_eq!(resolve_format(b"x", Some("application/octet-stream"), Some("a.md"), &opts), Ok(Format::Markdown));
        // Sniffing, then plain-text fallback.
        assert_eq!(resolve_format(b"%PDF-1.7", None, None, &opts), Ok(Format::Pdf));
        assert_eq!(resolve_format(b"just words", None, None, &opts), Ok(Format::PlainText));
        assert!(matches!(resolve_format(&[0, 1, 2, 3], None, Some("x.bin"), &opts), Err(DocError::Unsupported { .. })));
    }

    #[test]
    fn csv_markdown_and_text() {
        let csv = b"name,qty\napple,3\n";
        let md = extract(csv, Some("text/csv"), None, &Options::default().with_format(OutputFormat::Markdown));
        assert_eq!(md.as_deref(), Ok("| name | qty |\n| --- | --- |\n| apple | 3 |"));
        let txt = extract(csv, None, Some("f.csv"), &Options::default());
        assert_eq!(txt.as_deref(), Ok("name\tqty\napple\t3"));
    }
}
