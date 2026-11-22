//! Malformed and hostile documents: every extraction must return (an error or
//! text) quickly, without a panic escaping, a stack overflow or a hang.
//! Requires every format feature (the default feature set).

#![cfg(all(feature = "pdf", feature = "office", feature = "spreadsheet", feature = "epub", feature = "email"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod support;

use std::time::{Duration, Instant};

use graphrt_doc::{Options, extract};
use support::{pdf_with_content, zip_of};

/// Extract `bytes` as each of the given MIME types and require a prompt result.
fn extract_quickly(what: &str, bytes: &[u8], mimes: &[Option<&str>]) {
    for mime in mimes {
        let started = Instant::now();
        let _ = extract(bytes, *mime, None, &Options::default());
        assert!(started.elapsed() < Duration::from_secs(10), "{what} as {mime:?} took {:?}", started.elapsed());
    }
}

#[test]
fn truncated_pdfs_fail_cleanly() {
    let pdf = pdf_with_content(b"BT /F1 24 Tf 72 720 Td (Hello) Tj ET");
    for len in (0..pdf.len()).step_by(7) {
        extract_quickly("truncated pdf", &pdf[..len], &[Some("application/pdf")]);
    }
}

#[test]
fn deeply_nested_pdf_objects_do_not_overflow_the_stack() {
    let depth = 100_000;
    let arrays = format!("BT {}{} TJ ET", "[".repeat(depth), "]".repeat(depth));
    let dicts = format!("BT {}{} ET", "<<".repeat(depth), ">>".repeat(depth));
    for content in [arrays, dicts] {
        extract_quickly("nested pdf content", &pdf_with_content(content.as_bytes()), &[Some("application/pdf")]);
    }
    let mut nested_object = pdf_with_content(b"BT ET");
    let at = nested_object.windows(8).position(|w| w == b"/Count 1").unwrap();
    let deep = format!("/Count 1 /Deep {}{}", "[".repeat(depth), "]".repeat(depth));
    nested_object.splice(at..at + 8, deep.bytes());
    extract_quickly("nested pdf object", &nested_object, &[Some("application/pdf")]);
}

#[test]
fn deeply_nested_email_parts_do_not_overflow_the_stack() {
    let depth = 20_000;
    let mut eml = String::from("From: a@example.com\r\nSubject: nest\r\n");
    for level in 0..depth {
        eml.push_str(&format!("Content-Type: multipart/mixed; boundary=\"b{level}\"\r\n\r\n--b{level}\r\n"));
    }
    eml.push_str("Content-Type: text/plain\r\n\r\nhello\r\n");
    extract_quickly("nested eml", eml.as_bytes(), &[Some("message/rfc822")]);
}

#[test]
fn deeply_nested_xml_and_html_are_linear() {
    let depth = 200_000;
    let html = format!("{}text{}", "<div>".repeat(depth), "</div>".repeat(depth));
    extract_quickly("nested html", html.as_bytes(), &[Some("text/html")]);
    let unclosed = "<script <!-- <a ".repeat(depth);
    extract_quickly("unclosed html", unclosed.as_bytes(), &[Some("text/html")]);
    let entities = "&".repeat(depth * 5);
    extract_quickly("entities", entities.as_bytes(), &[Some("text/html")]);

    let body = format!(
        "{}<w:t>x</w:t>{}",
        "<w:tbl><w:tr><w:tc>".repeat(depth / 4),
        "</w:tc></w:tr></w:tbl>".repeat(depth / 4)
    );
    let document = format!("<w:document><w:body>{body}</w:body></w:document>");
    let docx = zip_of(&[("word/document.xml", &document)]);
    extract_quickly("nested docx", &docx, &[None]);
}

#[test]
fn garbage_with_every_format_label_fails_cleanly() {
    let mut garbage: Vec<u8> = (0..64 * 1024u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    let mimes = [
        Some("application/pdf"),
        Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
        Some("application/vnd.openxmlformats-officedocument.presentationml.presentation"),
        Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
        Some("application/vnd.oasis.opendocument.spreadsheet"),
        Some("application/vnd.ms-excel"),
        Some("application/epub+zip"),
        Some("message/rfc822"),
        Some("text/csv"),
        Some("text/html"),
        None,
    ];
    extract_quickly("garbage", &garbage, &mimes);
    // The same bytes behind each format's magic number.
    for magic in [&b"%PDF-1.7\n"[..], b"PK\x03\x04", b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1"] {
        garbage.splice(..magic.len(), magic.iter().copied());
        extract_quickly("garbage with magic", &garbage, &mimes);
    }
}
