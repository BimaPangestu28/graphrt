//! Format extraction tests with fixtures generated in-test.
//! Requires every format feature (the default feature set).

#![cfg(all(feature = "pdf", feature = "office", feature = "spreadsheet", feature = "epub", feature = "email"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod support;

use std::io::{Cursor, Write};

use graphrt_doc::{DocError, Format, Options, OutputFormat, extract, extract_detailed};
use support::{pdf_with_content, zip_of};

fn md() -> Options {
    Options::default().with_format(OutputFormat::Markdown)
}

const W: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;

fn docx() -> Vec<u8> {
    let doc = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><w:document {W}><w:body>
<w:p><w:r><w:t>Quarterly</w:t></w:r><w:r><w:t xml:space="preserve"> report &amp; notes</w:t></w:r></w:p>
<w:p><w:r><w:t>Line one</w:t><w:br/><w:t>Line two</w:t></w:r></w:p>
<w:tbl>
 <w:tr><w:tc><w:p><w:r><w:t>Item</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Qty</w:t></w:r></w:p></w:tc></w:tr>
 <w:tr><w:tc><w:p><w:r><w:t>Apple</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>3</w:t></w:r></w:p></w:tc></w:tr>
</w:tbl>
<w:p><w:r><w:t>After table</w:t></w:r></w:p>
</w:body></w:document>"#
    );
    zip_of(&[("[Content_Types].xml", "<Types/>"), ("word/document.xml", &doc)])
}

#[test]
fn docx_paragraphs_breaks_and_tables() {
    let bytes = docx();
    let text = extract(&bytes, None, Some("r.docx"), &Options::default()).unwrap();
    assert_eq!(text, "Quarterly report & notes\nLine one\nLine two\nItem\tQty\nApple\t3\nAfter table");
    let markdown = extract(&bytes, None, Some("r.docx"), &md()).unwrap();
    assert!(markdown.contains("| Item | Qty |\n| --- | --- |\n| Apple | 3 |"), "{markdown}");
    // Detected from content alone (zip container inspection).
    let detailed = extract_detailed(&bytes, None, None, &Options::default()).unwrap();
    assert_eq!(detailed.format, Format::Docx);
}

#[test]
fn pptx_slides_in_numeric_order() {
    let slide = |text: &str| {
        format!(
            r#"<p:sld xmlns:p="p" xmlns:a="a"><p:cSld><p:spTree><p:sp><p:txBody>
<a:p><a:r><a:t>{text}</a:t></a:r></a:p><a:p><a:r><a:t>bullet</a:t></a:r></a:p>
</p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
        )
    };
    let bytes = zip_of(&[
        ("ppt/presentation.xml", "<p:presentation/>"),
        ("ppt/slides/slide10.xml", &slide("Ten")),
        ("ppt/slides/slide2.xml", &slide("Two")),
    ]);
    let text = extract(&bytes, None, Some("deck.pptx"), &Options::default()).unwrap();
    assert_eq!(text, "Slide 2\nTwo\nbullet\n\nSlide 10\nTen\nbullet");
    let markdown = extract(&bytes, None, Some("deck.pptx"), &md()).unwrap();
    assert!(markdown.starts_with("## Slide 2\n"));
}

#[test]
fn epub_follows_spine_order() {
    let bytes = zip_of(&[
        ("mimetype", "application/epub+zip"),
        (
            "META-INF/container.xml",
            r#"<container><rootfiles><rootfile full-path="OEBPS/content.opf"/></rootfiles></container>"#,
        ),
        (
            "OEBPS/content.opf",
            r#"<package><manifest>
<item id="c1" href="ch%201.xhtml"/><item id="c2" href="ch2.xhtml"/></manifest>
<spine><itemref idref="c2"/><itemref idref="c1"/></spine></package>"#,
        ),
        ("OEBPS/ch 1.xhtml", "<html><body><h1>One</h1><p>first</p></body></html>"),
        ("OEBPS/ch2.xhtml", "<html><body><h1>Two</h1><p>second</p></body></html>"),
    ]);
    let text = extract(&bytes, None, None, &Options::default()).unwrap();
    assert_eq!(text, "Two\n\nsecond\n\nOne\n\nfirst");
}

fn xlsx() -> Vec<u8> {
    zip_of(&[
        (
            "[Content_Types].xml",
            r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>"#,
        ),
        (
            "_rels/.rels",
            r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#,
        ),
        (
            "xl/workbook.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="Stock" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#,
        ),
        (
            "xl/worksheets/sheet1.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>
<row r="1"><c r="A1" t="inlineStr"><is><t>Item</t></is></c><c r="B1" t="inlineStr"><is><t>Qty</t></is></c></row>
<row r="2"><c r="A2" t="inlineStr"><is><t>Pear</t></is></c><c r="B2"><v>7</v></c></row>
</sheetData></worksheet>"#,
        ),
    ])
}

#[test]
fn xlsx_sheets_as_tables() {
    let bytes = xlsx();
    let markdown = extract(&bytes, None, Some("stock.xlsx"), &md()).unwrap();
    assert_eq!(markdown, "## Stock\n| Item | Qty |\n| --- | --- |\n| Pear | 7 |");
    let text = extract(&bytes, None, None, &Options::default()).unwrap();
    assert_eq!(text, "Stock\nItem\tQty\nPear\t7");
}

/// A one-page PDF showing `text`.
fn pdf(text: &str) -> Vec<u8> {
    pdf_with_content(format!("BT /F1 24 Tf 72 720 Td ({text}) Tj ET").as_bytes())
}

#[test]
fn pdf_text() {
    let text = extract(&pdf("Hello PDF world"), None, None, &Options::default()).unwrap();
    assert_eq!(text.trim(), "Hello PDF world");
}

#[test]
fn pdf_page_limit_and_garbage() {
    let opts = Options { max_pages: 0, ..Options::default() };
    let err = extract(&pdf("x"), Some("application/pdf"), None, &opts).unwrap_err();
    assert!(matches!(err, DocError::LimitExceeded(_)), "{err:?}");
    let err = extract(b"%PDF-1.4\ngarbage that is not a pdf", None, None, &Options::default()).unwrap_err();
    assert_eq!(err.kind(), "malformed_document", "{err:?}");
}

#[test]
fn email_headers_and_body() {
    let eml = "From: Ann <ann@example.com>\r\nTo: bob@example.com\r\nSubject: Lunch?\r\n\
               Date: Tue, 1 Jul 2025 10:00:00 +0000\r\nContent-Type: text/plain\r\n\r\nNoon at the cafe.\r\n";
    let text = extract(eml.as_bytes(), None, Some("m.eml"), &Options::default()).unwrap();
    assert!(
        text.starts_with(
            "From: Ann <ann@example.com>\nTo: bob@example.com\nDate: 2025-07-01T10:00:00Z\nSubject: Lunch?"
        ),
        "{text}"
    );
    assert!(text.ends_with("\n\nNoon at the cafe."), "{text}");
}

#[test]
fn html_and_plain_formats() {
    let html = "<html><body><script>x()</script><p>Hi &lt;there&gt;</p></body></html>";
    assert_eq!(extract(html.as_bytes(), Some("text/html"), None, &Options::default()).unwrap(), "Hi <there>");
    assert_eq!(extract(b"{\"a\":1}\n", None, Some("a.json"), &Options::default()).unwrap(), "{\"a\":1}");
    let srt = "1\n00:00:01,000 --> 00:00:02,000\nHello\n";
    assert_eq!(extract(srt.as_bytes(), None, Some("s.srt"), &Options::default()).unwrap(), "Hello");
}

#[test]
fn zip_bomb_is_rejected() {
    let zeros = "0".repeat(4 * 1024 * 1024);
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    w.start_file("word/document.xml", opts).unwrap();
    w.write_all(zeros.as_bytes()).unwrap();
    let bytes = w.finish().unwrap().into_inner();
    assert!(bytes.len() < 100 * 1024, "fixture should compress well");
    let opts = Options { max_decompressed_bytes: 1024 * 1024, ..Options::default() };
    let err = extract(&bytes, None, Some("bomb.docx"), &opts).unwrap_err();
    assert!(matches!(err, DocError::LimitExceeded(_)), "{err:?}");
}

#[test]
fn too_many_zip_entries() {
    let names: Vec<String> = (0..20).map(|i| format!("f{i}.txt")).collect();
    let entries: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "x")).collect();
    let bytes = zip_of(&entries);
    let opts = Options { max_zip_entries: 10, ..Options::default() };
    let err = extract(&bytes, None, Some("a.docx"), &opts).unwrap_err();
    assert!(matches!(err, DocError::LimitExceeded(_)), "{err:?}");
}

#[test]
fn malformed_zip_formats_do_not_panic() {
    for name in ["a.docx", "a.pptx", "a.xlsx", "a.epub"] {
        let err = extract(b"PK\x03\x04 broken", None, Some(name), &Options::default()).unwrap_err();
        assert_eq!(err.kind(), "malformed_document", "{name}: {err:?}");
    }
    let err = extract(&zip_of(&[("x.txt", "hi")]), None, None, &Options::default()).unwrap_err();
    assert_eq!(err.kind(), "unsupported_format");
}

#[test]
fn output_is_truncated_to_limit() {
    let opts = Options::default().with_max_output_chars(5);
    let out = extract_detailed(b"hello world", None, Some("a.txt"), &opts).unwrap();
    assert_eq!(out.text, "hello");
    assert!(out.truncated);
}
