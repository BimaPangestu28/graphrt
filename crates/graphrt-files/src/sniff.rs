//! Media type detection from content.
//!
//! A file name and a declared MIME type are claims made by whoever sent the
//! file. [`sniff`] reads the first bytes instead, and the rest of the crate
//! decides what a file is from that answer only.

use graphrt_core::value::FileKind;

/// How many leading bytes [`sniff`] looks at.
pub const SNIFF_BYTES: usize = 8192;

/// What a file is, judged from its content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MediaClass {
    /// Plain text that is safe to show as text (UTF-8, no markup that runs).
    Text,
    /// Text a browser or webview would execute or render actively: HTML, SVG, XHTML.
    ActiveText,
    /// PNG, JPEG, GIF, WebP, BMP or TIFF.
    Image,
    /// PDF.
    Pdf,
    /// A zip container (also DOCX, XLSX, PPTX, EPUB, ODS) or a legacy Office file.
    Document,
    /// A compressed archive that is not a known document container.
    Archive,
    /// Native code or a script the operating system would run.
    Executable,
    /// Anything else.
    Binary,
}

/// The detected type of a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaType {
    /// MIME type, for example `image/png`.
    pub mime: &'static str,
    pub class: MediaClass,
}

impl MediaType {
    const fn new(mime: &'static str, class: MediaClass) -> Self {
        Self { mime, class }
    }

    /// The coarse graphrt file kind for this type.
    pub fn file_kind(self) -> FileKind {
        match self.class {
            MediaClass::Image => FileKind::Image,
            MediaClass::Text | MediaClass::ActiveText | MediaClass::Pdf | MediaClass::Document => FileKind::Document,
            MediaClass::Archive | MediaClass::Executable | MediaClass::Binary => FileKind::Custom,
        }
    }

    /// Whether a user interface may show this content inline.
    ///
    /// `false` for HTML and SVG (they run scripts in a webview), executables
    /// and unknown binaries. Hosts should offer those as downloads only, and
    /// serve them with `Content-Disposition: attachment` and
    /// `X-Content-Type-Options: nosniff`.
    pub fn is_inline_safe(self) -> bool {
        matches!(self.class, MediaClass::Text | MediaClass::Image | MediaClass::Pdf)
    }
}

/// Detect the media type of `bytes` (only the first [`SNIFF_BYTES`] are read).
///
/// `extension` refines two cases where the content alone is ambiguous: a zip
/// container (`docx`, `xlsx`, `pptx`, `epub`, `ods`) and plain text (`csv`,
/// `md`, `json`...). It never overrides what the bytes say: a PNG named
/// `notes.txt` is `image/png`, and a Windows program named `photo.png` is an
/// executable.
///
/// ```
/// use graphrt_files::{MediaClass, sniff};
///
/// assert_eq!(sniff(b"\x89PNG\r\n\x1a\n....", Some("txt")).mime, "image/png");
/// assert_eq!(sniff(b"MZ\x90\x00", Some("png")).class, MediaClass::Executable);
/// assert_eq!(sniff(b"name,qty\napple,3\n", Some("csv")).mime, "text/csv");
/// assert_eq!(sniff(b"<svg xmlns='http://www.w3.org/2000/svg'>", Some("txt")).class, MediaClass::ActiveText);
/// ```
pub fn sniff(bytes: &[u8], extension: Option<&str>) -> MediaType {
    let head = bytes.get(..bytes.len().min(SNIFF_BYTES)).unwrap_or_default();
    if let Some(found) = sniff_binary(head, extension) {
        return found;
    }
    if !looks_like_text(head) {
        return MediaType::new("application/octet-stream", MediaClass::Binary);
    }
    if let Some(active) = sniff_active_text(head) {
        return active;
    }
    if head.starts_with(b"#!") {
        return MediaType::new("text/x-shellscript", MediaClass::Executable);
    }
    MediaType::new(text_mime(extension), MediaClass::Text)
}

fn sniff_binary(head: &[u8], extension: Option<&str>) -> Option<MediaType> {
    let image = |mime| Some(MediaType::new(mime, MediaClass::Image));
    let executable = |mime| Some(MediaType::new(mime, MediaClass::Executable));
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        return image("image/png");
    }
    if head.starts_with(&[0xff, 0xd8, 0xff]) {
        return image("image/jpeg");
    }
    if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        return image("image/gif");
    }
    if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP") {
        return image("image/webp");
    }
    if head.starts_with(b"BM") && head.len() >= 26 && head.get(6..10) == Some(&[0, 0, 0, 0]) {
        return image("image/bmp");
    }
    if head.starts_with(b"II*\0") || head.starts_with(b"MM\0*") {
        return image("image/tiff");
    }
    if head.starts_with(b"%PDF-") {
        return Some(MediaType::new("application/pdf", MediaClass::Pdf));
    }
    if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        return Some(zip_type(extension));
    }
    if head.starts_with(&[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1]) {
        return Some(MediaType::new("application/x-ole-storage", MediaClass::Document));
    }
    if head.starts_with(&[0x1f, 0x8b]) {
        return Some(MediaType::new("application/gzip", MediaClass::Archive));
    }
    if head.starts_with(b"7z\xbc\xaf\x27\x1c") {
        return Some(MediaType::new("application/x-7z-compressed", MediaClass::Archive));
    }
    if head.starts_with(b"Rar!\x1a\x07") {
        return Some(MediaType::new("application/vnd.rar", MediaClass::Archive));
    }
    if head.starts_with(b"\x7fELF") {
        return executable("application/x-elf");
    }
    if head.starts_with(b"MZ") {
        return executable("application/vnd.microsoft.portable-executable");
    }
    if head.starts_with(b"\0asm") {
        return executable("application/wasm");
    }
    let mach_o: [[u8; 4]; 6] = [
        [0xfe, 0xed, 0xfa, 0xce],
        [0xfe, 0xed, 0xfa, 0xcf],
        [0xce, 0xfa, 0xed, 0xfe],
        [0xcf, 0xfa, 0xed, 0xfe],
        [0xca, 0xfe, 0xba, 0xbe],
        [0xbe, 0xba, 0xfe, 0xca],
    ];
    if mach_o.iter().any(|magic| head.starts_with(magic)) {
        return executable("application/x-mach-binary");
    }
    None
}

/// A zip container is named by its extension; without a known one it is an archive.
fn zip_type(extension: Option<&str>) -> MediaType {
    let document = |mime| MediaType::new(mime, MediaClass::Document);
    match extension.map(str::to_ascii_lowercase).as_deref() {
        Some("docx") => document("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
        Some("xlsx" | "xlsm") => document("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
        Some("pptx") => document("application/vnd.openxmlformats-officedocument.presentationml.presentation"),
        Some("ods") => document("application/vnd.oasis.opendocument.spreadsheet"),
        Some("epub") => document("application/epub+zip"),
        _ => MediaType::new("application/zip", MediaClass::Archive),
    }
}

/// Text is valid UTF-8 (a cut multi-byte character at the end is fine) without
/// NUL bytes. UTF-16 with a byte order mark counts as text too.
fn looks_like_text(head: &[u8]) -> bool {
    if head.starts_with(&[0xff, 0xfe]) || head.starts_with(&[0xfe, 0xff]) {
        return true;
    }
    if head.contains(&0) {
        return false;
    }
    match std::str::from_utf8(head) {
        Ok(_) => true,
        // The window may end inside a character: only an error at the very end is fine.
        Err(error) => error.error_len().is_none() && head.len() - error.valid_up_to() < 4,
    }
}

/// SVG and HTML: a document that starts as HTML, or any text holding a tag
/// that loads or runs something (`<script`, `<iframe`, `<object`, `<embed`).
fn sniff_active_text(head: &[u8]) -> Option<MediaType> {
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();
    if text.contains("<svg") {
        return Some(MediaType::new("image/svg+xml", MediaClass::ActiveText));
    }
    let start = text.trim_start_matches('\u{feff}').trim_start();
    let is_html_document = start.starts_with("<!doctype html") || start.starts_with("<html");
    let runs_something = ["<script", "<iframe", "<object", "<embed"].iter().any(|tag| text.contains(tag));
    if is_html_document || runs_something {
        return Some(MediaType::new("text/html", MediaClass::ActiveText));
    }
    None
}

/// MIME type of plain text, from the extension.
fn text_mime(extension: Option<&str>) -> &'static str {
    match extension.map(str::to_ascii_lowercase).as_deref() {
        Some("md" | "markdown") => "text/markdown",
        Some("csv") => "text/csv",
        Some("tsv") => "text/tab-separated-values",
        Some("json" | "jsonl" | "ndjson") => "application/json",
        Some("yaml" | "yml") => "application/yaml",
        Some("xml") => "application/xml",
        _ => "text/plain",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_and_documents_by_magic_bytes() {
        let cases: [(&[u8], &str); 8] = [
            (b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR", "image/png"),
            (&[0xff, 0xd8, 0xff, 0xe0, 0, 16], "image/jpeg"),
            (b"GIF89a\x01\0\x01\0", "image/gif"),
            (b"RIFF\x24\0\0\0WEBPVP8 ", "image/webp"),
            (b"II*\0\x08\0\0\0", "image/tiff"),
            (b"%PDF-1.7\n", "application/pdf"),
            (&[0x1f, 0x8b, 8, 0], "application/gzip"),
            (b"PK\x03\x04\x14\0", "application/zip"),
        ];
        for (bytes, mime) in cases {
            assert_eq!(sniff(bytes, None).mime, mime);
        }
        assert_eq!(sniff(b"PK\x03\x04", Some("DOCX")).class, MediaClass::Document);
        assert_eq!(sniff(b"PK\x03\x04", Some("xlsx")).file_kind(), FileKind::Document);
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n", None).file_kind(), FileKind::Image);
    }

    #[test]
    fn the_extension_never_overrides_the_content() {
        // A program called photo.png is a program.
        for (bytes, _name) in [
            (&b"MZ\x90\0\x03\0"[..], "pe"),
            (&b"\x7fELF\x02\x01\x01"[..], "elf"),
            (&[0xcf, 0xfa, 0xed, 0xfe, 7, 0][..], "mach-o"),
            (&b"\0asm\x01\0\0\0"[..], "wasm"),
            (&b"#!/bin/sh\nrm -rf ~\n"[..], "script"),
        ] {
            assert_eq!(sniff(bytes, Some("png")).class, MediaClass::Executable);
            assert_eq!(sniff(bytes, Some("txt")).class, MediaClass::Executable);
        }
        // An image called notes.txt is an image; a zip called data.csv is an archive.
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n", Some("txt")).mime, "image/png");
        assert_eq!(sniff(b"PK\x03\x04", Some("csv")).class, MediaClass::Archive);
    }

    #[test]
    fn html_and_svg_are_active_whatever_they_are_called() {
        let active: [&[u8]; 7] = [
            b"<!DOCTYPE html><p>hi",
            b"  \n<HTML><body onload=alert(1)>",
            b"<?xml version='1.0'?>\n<svg xmlns='http://www.w3.org/2000/svg'><script>alert(1)</script></svg>",
            b"hello <script>fetch('//evil')</script>",
            b"# title\n\n<iframe src='//evil'></iframe>",
            b"\xef\xbb\xbf<svg onload='x()'/>",
            b"<p>x</p><object data='x'>",
        ];
        for bytes in active {
            for extension in [None, Some("txt"), Some("md"), Some("png"), Some("csv")] {
                let found = sniff(bytes, extension);
                assert_eq!(found.class, MediaClass::ActiveText, "{:?}", String::from_utf8_lossy(bytes));
                assert!(!found.is_inline_safe());
            }
        }
        assert_eq!(sniff(b"<svg/>", Some("svg")).mime, "image/svg+xml");
    }

    #[test]
    fn plain_text_takes_its_flavour_from_the_extension() {
        assert_eq!(sniff(b"# Title\n", Some("md")).mime, "text/markdown");
        assert_eq!(sniff(b"a,b\n1,2\n", Some("CSV")).mime, "text/csv");
        assert_eq!(sniff(b"{\"a\":1}", Some("json")).mime, "application/json");
        assert_eq!(sniff(b"hello", None).mime, "text/plain");
        assert_eq!(sniff(b"", Some("txt")).class, MediaClass::Text);
        assert_eq!(sniff("héllo wörld".as_bytes(), None).class, MediaClass::Text);
        assert!(sniff(b"hello", None).is_inline_safe());
    }

    #[test]
    fn a_window_cut_inside_a_character_is_still_text() {
        let mut text = "a".repeat(SNIFF_BYTES - 1).into_bytes();
        text.extend_from_slice("é".as_bytes());
        assert_eq!(sniff(&text, None).class, MediaClass::Text);
        // Invalid UTF-8 in the middle is not text.
        assert_eq!(sniff(&[b'a', 0xff, 0xfe, b'b', b'c', b'd', b'e'], None).class, MediaClass::Binary);
        assert_eq!(sniff(&[1, 2, 0, 3], None).class, MediaClass::Binary);
    }
}
