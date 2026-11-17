//! [`Extractor`] backed by `graphrt-doc`: ingest PDF, Office, spreadsheet,
//! EPUB, HTML, email and subtitle files, not only plain text.

use crate::error::RagError;
use crate::types::Extractor;

/// Extracts text from any format supported by `graphrt-doc`.
#[derive(Clone, Debug, Default)]
pub struct DocExtractor {
    options: graphrt_doc::Options,
}

impl DocExtractor {
    /// Extractor with default limits, emitting Markdown so headings and tables
    /// survive chunking.
    pub fn new() -> Self {
        let options =
            graphrt_doc::Options { format: graphrt_doc::OutputFormat::Markdown, ..graphrt_doc::Options::default() };
        Self { options }
    }

    /// Extractor with explicit options.
    pub fn with_options(options: graphrt_doc::Options) -> Self {
        Self { options }
    }
}

impl Extractor for DocExtractor {
    fn extract(&self, bytes: &[u8], mime_type: Option<&str>, file_name: &str) -> Result<String, RagError> {
        graphrt_doc::extract(bytes, mime_type, Some(file_name), &self.options)
            .map_err(|e| RagError::Input(format!("cannot extract `{file_name}`: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_html_as_text() {
        let html = b"<html><head><style>p{}</style></head><body><h1>Title</h1><p>Body text</p></body></html>";
        let text = DocExtractor::new().extract(html, Some("text/html"), "page.html").unwrap();
        assert!(text.contains("Title"));
        assert!(text.contains("Body text"));
        assert!(!text.contains("p{}"));
    }

    #[test]
    fn unsupported_format_is_an_error() {
        let err = DocExtractor::new().extract(&[0, 1, 2, 3], Some("application/x-unknown"), "blob.bin");
        assert!(err.is_err());
    }
}
