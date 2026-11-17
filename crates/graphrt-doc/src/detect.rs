//! Format detection: MIME type first, then file extension, then content sniffing.

use std::fmt;

/// A document format this crate knows about (whether or not its feature is enabled).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Format {
    PlainText,
    Markdown,
    Json,
    Yaml,
    Xml,
    Csv,
    Tsv,
    Html,
    Pdf,
    Docx,
    Pptx,
    Xlsx,
    Xls,
    Ods,
    Epub,
    Eml,
    Vtt,
    Srt,
}

impl Format {
    /// Short lowercase name, e.g. `pdf`.
    pub fn name(self) -> &'static str {
        match self {
            Format::PlainText => "text",
            Format::Markdown => "markdown",
            Format::Json => "json",
            Format::Yaml => "yaml",
            Format::Xml => "xml",
            Format::Csv => "csv",
            Format::Tsv => "tsv",
            Format::Html => "html",
            Format::Pdf => "pdf",
            Format::Docx => "docx",
            Format::Pptx => "pptx",
            Format::Xlsx => "xlsx",
            Format::Xls => "xls",
            Format::Ods => "ods",
            Format::Epub => "epub",
            Format::Eml => "eml",
            Format::Vtt => "vtt",
            Format::Srt => "srt",
        }
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Map a MIME type (parameters ignored) to a format.
pub fn from_mime(mime: &str) -> Option<Format> {
    let essence = mime.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    Some(match essence.as_str() {
        "text/plain" => Format::PlainText,
        "text/markdown" | "text/x-markdown" => Format::Markdown,
        "application/json" | "text/json" => Format::Json,
        "application/yaml" | "application/x-yaml" | "text/yaml" | "text/x-yaml" => Format::Yaml,
        "application/xml" | "text/xml" => Format::Xml,
        "text/csv" | "application/csv" => Format::Csv,
        "text/tab-separated-values" => Format::Tsv,
        "text/html" | "application/xhtml+xml" => Format::Html,
        "application/pdf" => Format::Pdf,
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => Format::Docx,
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => Format::Pptx,
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => Format::Xlsx,
        "application/vnd.ms-excel" => Format::Xls,
        "application/vnd.oasis.opendocument.spreadsheet" => Format::Ods,
        "application/epub+zip" => Format::Epub,
        "message/rfc822" => Format::Eml,
        "text/vtt" => Format::Vtt,
        "application/x-subrip" | "text/srt" => Format::Srt,
        _ => return None,
    })
}

/// Map a file extension (with or without the dot, any case) to a format.
pub fn from_extension(ext: &str) -> Option<Format> {
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    Some(match ext.as_str() {
        "txt" | "text" | "log" | "properties" | "ini" | "cfg" | "conf" | "toml" => Format::PlainText,
        "md" | "markdown" | "mdx" => Format::Markdown,
        "json" | "jsonl" | "ndjson" => Format::Json,
        "yaml" | "yml" => Format::Yaml,
        "xml" => Format::Xml,
        "csv" => Format::Csv,
        "tsv" => Format::Tsv,
        "html" | "htm" | "xhtml" => Format::Html,
        "pdf" => Format::Pdf,
        "docx" => Format::Docx,
        "pptx" => Format::Pptx,
        "xlsx" | "xlsm" => Format::Xlsx,
        "xls" => Format::Xls,
        "ods" => Format::Ods,
        "epub" => Format::Epub,
        "eml" => Format::Eml,
        "vtt" => Format::Vtt,
        "srt" => Format::Srt,
        _ => return None,
    })
}

/// Guess the format from the first bytes. Zip containers are not resolved here
/// (see [`crate::extract`]), because that requires opening the archive.
pub fn sniff(bytes: &[u8]) -> Option<Format> {
    let head = bytes.get(..bytes.len().min(512)).unwrap_or_default();
    if head.starts_with(b"%PDF-") {
        return Some(Format::Pdf);
    }
    if head.starts_with(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]) {
        // OLE compound file: legacy Office; only XLS is supported.
        return Some(Format::Xls);
    }
    if head.starts_with(b"WEBVTT") || head.starts_with("\u{feff}WEBVTT".as_bytes()) {
        return Some(Format::Vtt);
    }
    let text = String::from_utf8_lossy(head);
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    let lower = trimmed.get(..trimmed.len().min(64)).unwrap_or_default().to_ascii_lowercase();
    if lower.starts_with("<!doctype html") || lower.starts_with("<html") {
        return Some(Format::Html);
    }
    if lower.starts_with("<?xml") {
        return Some(Format::Xml);
    }
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return Some(Format::Json);
    }
    None
}

/// Whether the bytes start like a zip archive.
pub fn is_zip(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04")
}

/// Whether the bytes look like text (no NUL bytes in the first 8 KiB).
pub fn looks_textual(bytes: &[u8]) -> bool {
    let head = bytes.get(..bytes.len().min(8192)).unwrap_or_default();
    !head.contains(&0)
}

/// File extension of a file name, if any.
pub fn extension(filename: &str) -> Option<&str> {
    let base = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
    let (stem, ext) = base.rsplit_once('.')?;
    if stem.is_empty() || ext.is_empty() { None } else { Some(ext) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_and_extension() {
        assert_eq!(from_mime("application/pdf; charset=binary"), Some(Format::Pdf));
        assert_eq!(from_mime("application/octet-stream"), None);
        assert_eq!(from_extension(".DOCX"), Some(Format::Docx));
        assert_eq!(extension("a/b/report.final.xlsx"), Some("xlsx"));
        assert_eq!(extension(".bashrc"), None);
    }

    #[test]
    fn sniffing() {
        assert_eq!(sniff(b"%PDF-1.4\n"), Some(Format::Pdf));
        assert_eq!(sniff(b"  <!DOCTYPE html><p>x"), Some(Format::Html));
        assert_eq!(sniff(b"WEBVTT\n\n"), Some(Format::Vtt));
        assert_eq!(sniff(b"hello"), None);
        assert!(is_zip(b"PK\x03\x04rest"));
    }
}
