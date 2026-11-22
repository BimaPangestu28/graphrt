//! Regression tests for resource limits on hostile input.
//! Requires every format feature (the default feature set).

#![cfg(all(feature = "pdf", feature = "office", feature = "spreadsheet", feature = "epub", feature = "email"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::io::{Cursor, Write};
use std::time::{Duration, Instant};

use graphrt_doc::{DocError, Format, Options, OutputFormat, extract, extract_detailed};

fn zip_of(entries: &[(&str, &str)]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    for (name, body) in entries {
        w.start_file(*name, opts).unwrap();
        w.write_all(body.as_bytes()).unwrap();
    }
    w.finish().unwrap().into_inner()
}

fn xlsx_with_sheet(sheet: &str) -> Vec<u8> {
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
<sheets><sheet name="Data" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#,
        ),
        ("xl/worksheets/sheet1.xml", sheet),
    ])
}

fn sheet_xml(cells: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><dimension ref="A1:XFD1048576"/><sheetData>{cells}</sheetData></worksheet>"#
    )
}

#[test]
fn xlsx_with_extreme_corner_cells_is_rejected_without_allocating() {
    // One cell at A1 and one at XFD1048576: a dense grid would need ~1.7e10 cells.
    let sheet =
        sheet_xml(r#"<row r="1"><c r="A1"><v>1</v></c></row><row r="1048576"><c r="XFD1048576"><v>2</v></c></row>"#);
    let start = Instant::now();
    let err = extract(&xlsx_with_sheet(&sheet), None, Some("bomb.xlsx"), &Options::default()).unwrap_err();
    assert!(matches!(err, DocError::LimitExceeded(_)), "{err:?}");
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn xlsx_sparse_cells_within_limits_render_only_used_rows() {
    let sheet = sheet_xml(
        r#"<row r="1"><c r="A1" t="inlineStr"><is><t>top</t></is></c></row>
<row r="150000"><c r="B150000" t="inlineStr"><is><t>bottom</t></is></c></row>"#,
    );
    let text = extract(&xlsx_with_sheet(&sheet), None, Some("sparse.xlsx"), &Options::default()).unwrap();
    assert_eq!(text, "Data\ntop\n\tbottom");
}

#[test]
fn xlsx_cell_count_limit() {
    let cells: String = (1..=50)
        .map(|r| format!(r#"<row r="{r}"><c r="A{r}"><v>{r}</v></c><c r="B{r}"><v>{r}</v></c></row>"#))
        .collect();
    let opts = Options { max_cells: 20, ..Options::default() };
    let err = extract(&xlsx_with_sheet(&sheet_xml(&cells)), None, Some("many.xlsx"), &opts).unwrap_err();
    assert!(matches!(err, DocError::LimitExceeded(_)), "{err:?}");
}

#[test]
fn xlsx_archive_size_is_checked_before_parsing() {
    // sharedStrings that inflates far beyond the budget.
    let big = format!("<sst>{}</sst>", "<si><t>x</t></si>".repeat(200_000));
    let mut bytes = xlsx_with_sheet(&sheet_xml(r#"<row r="1"><c r="A1"><v>1</v></c></row>"#));
    {
        let mut w = zip::ZipWriter::new_append(Cursor::new(&mut bytes)).unwrap();
        w.start_file("xl/sharedStrings.xml", zip::write::SimpleFileOptions::default()).unwrap();
        w.write_all(big.as_bytes()).unwrap();
        w.finish().unwrap();
    }
    let opts = Options { max_decompressed_bytes: 64 * 1024, ..Options::default() };
    let err = extract(&bytes, None, Some("big.xlsx"), &opts).unwrap_err();
    assert!(matches!(err, DocError::LimitExceeded(_)), "{err:?}");
}

fn ods_with_content(body: &str) -> Vec<u8> {
    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" office:version="1.2"><office:body><office:spreadsheet>{body}</office:spreadsheet></office:body></office:document-content>"#
    );
    zip_of(&[
        ("mimetype", "application/vnd.oasis.opendocument.spreadsheet"),
        (
            "META-INF/manifest.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?><manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0"><manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/></manifest:manifest>"#,
        ),
        ("content.xml", &content),
    ])
}

#[test]
fn ods_repeated_rows_and_columns_are_bounded() {
    // A value cell after 16000 repeated columns, repeated over a million rows.
    let body = r#"<table:table table:name="S"><table:table-row table:number-rows-repeated="1000000"><table:table-cell table:number-columns-repeated="16000"/><table:table-cell office:value-type="string"><text:p>x</text:p></table:table-cell></table:table-row></table:table>"#;
    let start = Instant::now();
    let err = extract(&ods_with_content(body), None, Some("bomb.ods"), &Options::default()).unwrap_err();
    assert!(matches!(err, DocError::LimitExceeded(_)), "{err:?}");
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn ods_small_sheet_still_extracts() {
    let body = r#"<table:table table:name="S"><table:table-row><table:table-cell office:value-type="string"><text:p>a</text:p></table:table-cell><table:table-cell office:value-type="string"><text:p>b</text:p></table:table-cell></table:table-row><table:table-row table:number-rows-repeated="1048000"><table:table-cell table:number-columns-repeated="1024"/></table:table-row></table:table>"#;
    let text = extract(&ods_with_content(body), None, Some("ok.ods"), &Options::default()).unwrap();
    assert_eq!(text, "S\na\tb");
}

#[test]
fn entity_flood_is_linear() {
    let html = format!("<p>{}</p>", "&".repeat(2_000_000));
    let start = Instant::now();
    let text = extract(html.as_bytes(), Some("text/html"), None, &Options::default()).unwrap();
    assert_eq!(text.len(), 2_000_000);
    assert!(start.elapsed() < Duration::from_secs(10), "took {:?}", start.elapsed());
}

#[test]
fn wide_csv_is_rejected() {
    let mut csv = ",".repeat(1_000_000);
    csv.push('\n');
    for _ in 0..1000 {
        csv.push_str("a\n");
    }
    let err = extract(csv.as_bytes(), Some("text/csv"), None, &Options::default()).unwrap_err();
    assert!(matches!(err, DocError::LimitExceeded(_)), "{err:?}");
}

#[test]
fn table_padding_stops_at_the_output_budget() {
    let mut csv = ",".repeat(2_000);
    csv.push('\n');
    for _ in 0..150_000 {
        csv.push_str("a\n");
    }
    let opts = Options { max_output_chars: 100_000, ..Options::default() }.with_format(OutputFormat::Markdown);
    let start = Instant::now();
    let out = extract_detailed(csv.as_bytes(), Some("text/csv"), None, &opts).unwrap();
    assert!(out.truncated);
    assert_eq!(out.text.chars().count(), 100_000);
    assert!(start.elapsed() < Duration::from_secs(5));
}

fn zlib(raw: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(raw).unwrap();
    encoder.finish().unwrap()
}

/// LZW as the PDF parser decodes it by default (`EarlyChange 1`).
fn lzw(raw: &[u8]) -> Vec<u8> {
    weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8).encode(raw).unwrap()
}

/// A one-page PDF whose content stream is `FlateDecode`-compressed.
fn pdf_with_flate_stream(raw: &[u8]) -> Vec<u8> {
    pdf_with_content_stream("/Filter /FlateDecode", &zlib(raw))
}

/// A one-page PDF whose content stream has `dict_entries` and `data`.
fn pdf_with_content_stream(dict_entries: &str, data: &[u8]) -> Vec<u8> {
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    let objects: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
            .to_vec(),
        {
            let mut body = format!("<< /Length {} {dict_entries} >>\nstream\n", data.len()).into_bytes();
            body.extend_from_slice(data);
            body.extend_from_slice(b"\nendstream");
            body
        },
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ];
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
    for off in offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1).as_bytes(),
    );
    out
}

/// Every way the PDF parser can expand a stream must count against the budget.
#[test]
fn pdf_decompression_bombs_through_every_decoder_are_rejected() {
    let spaces = vec![b' '; 8 * 1024 * 1024];
    let mut raw_deflate = flate2::write::DeflateEncoder::new(vec![0xff, 0xff], flate2::Compression::best());
    raw_deflate.write_all(&spaces).unwrap();
    let raw_deflate = raw_deflate.finish().unwrap();
    let bombs = [
        ("LZW", pdf_with_content_stream("/Filter /LZWDecode", &lzw(&spaces))),
        ("LZW inside Flate", pdf_with_content_stream("/Filter [/FlateDecode /LZWDecode]", &zlib(&lzw(&spaces)))),
        // A nested dictionary after the filter hid it from the old scan.
        (
            "Flate with DecodeParms",
            pdf_with_content_stream("/Filter /FlateDecode /DecodeParms << /Columns 1 >>", &zlib(&spaces)),
        ),
        // A bad zlib header makes the parser retry as raw deflate.
        ("raw deflate", pdf_with_content_stream("/Filter /FlateDecode", &raw_deflate)),
        // Whitespace between the dictionary and `stream` hid it from the old scan.
        ("spaced keyword", {
            let pdf = pdf_with_flate_stream(&spaces);
            let at = pdf.windows(9).position(|w| w == b">>\nstream").unwrap();
            [&pdf[..at + 2], &[b' '; 64][..], &pdf[at + 2..]].concat()
        }),
    ];
    let opts = Options { max_decompressed_bytes: 1024 * 1024, ..Options::default() };
    for (name, pdf) in bombs {
        let err = extract(&pdf, None, None, &opts).unwrap_err();
        assert!(matches!(err, DocError::LimitExceeded(_)), "{name}: {err:?}");
    }
}

#[test]
fn pdf_predictor_rows_count_against_the_budget() {
    // Two zeroed rows of 4 GB each would be allocated for a 10-byte stream.
    let pdf = pdf_with_content_stream(
        "/Filter /FlateDecode /DecodeParms << /Predictor 12 /Columns 4000000000 >>",
        &zlib(b"\x00 BT ET"),
    );
    let err = extract(&pdf, None, None, &Options::default()).unwrap_err();
    assert!(matches!(err, DocError::LimitExceeded(_)), "{err:?}");
}

#[test]
fn pdf_lzw_and_chained_streams_within_budget_extract() {
    let content = b"BT /F1 24 Tf 72 720 Td (Chained hello) Tj ET";
    for (dict, data) in
        [("/Filter /LZWDecode", lzw(content)), ("/Filter [/FlateDecode /LZWDecode]", zlib(&lzw(content)))]
    {
        let text = extract(&pdf_with_content_stream(dict, &data), None, None, &Options::default()).unwrap();
        assert!(text.contains("Chained hello"), "{dict}: {text}");
    }
}

#[test]
fn pdf_decompression_bomb_is_rejected_before_parsing() {
    let bomb = pdf_with_flate_stream(&vec![b' '; 8 * 1024 * 1024]);
    let opts = Options { max_decompressed_bytes: 1024 * 1024, ..Options::default() };
    let err = extract(&bomb, None, None, &opts).unwrap_err();
    assert!(matches!(err, DocError::LimitExceeded(_)), "{err:?}");
}

#[test]
fn pdf_flate_stream_within_budget_extracts() {
    let pdf = pdf_with_flate_stream(b"BT /F1 24 Tf 72 720 Td (Compressed hello) Tj ET");
    let text = extract(&pdf, None, None, &Options::default()).unwrap();
    assert!(text.contains("Compressed hello"), "{text}");
}

#[test]
fn binary_content_labelled_as_text_is_not_garbage() {
    let pdf = pdf_with_flate_stream(b"BT /F1 24 Tf 72 720 Td (Really a PDF) Tj ET");
    let out = extract_detailed(&pdf, Some("text/plain"), Some("notes.txt"), &Options::default()).unwrap();
    assert_eq!(out.format, Format::Pdf);
    assert!(out.text.contains("Really a PDF"));
    let err = extract(&[0u8, 159, 146, 150, 0, 1, 2], Some("text/plain"), None, &Options::default()).unwrap_err();
    assert!(matches!(err, DocError::Unsupported { .. }), "{err:?}");
}

#[test]
fn legacy_word_document_is_reported_as_unsupported() {
    let mut ole = vec![0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    ole.extend(std::iter::repeat_n(0u8, 512));
    ole.extend("WordDocument".encode_utf16().flat_map(u16::to_le_bytes));
    let err = extract(&ole, None, None, &Options::default()).unwrap_err();
    match err {
        DocError::Unsupported { detected } => assert!(detected.contains("legacy"), "{detected}"),
        other => panic!("unexpected {other:?}"),
    }
}
