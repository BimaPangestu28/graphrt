//! Fixture builders shared by the integration tests (each test binary uses a subset).

#![allow(dead_code, clippy::unwrap_used)]

use std::io::{Cursor, Write};

/// A zip archive of `(name, body)` entries with default (stored) options.
pub fn zip_of(entries: &[(&str, &str)]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    for (name, body) in entries {
        w.start_file(*name, opts).unwrap();
        w.write_all(body.as_bytes()).unwrap();
    }
    w.finish().unwrap().into_inner()
}

/// zlib-compressed `raw`, as a PDF `FlateDecode` stream stores it.
pub fn zlib(raw: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(raw).unwrap();
    encoder.finish().unwrap()
}

/// A one-page PDF (Helvetica as `/F1`) whose uncompressed content stream is `content`.
pub fn pdf_with_content(content: &[u8]) -> Vec<u8> {
    pdf_with_content_stream("", content)
}

/// A one-page PDF whose `FlateDecode`-compressed content stream decodes to `raw`.
pub fn pdf_with_flate_stream(raw: &[u8]) -> Vec<u8> {
    pdf_with_content_stream("/Filter /FlateDecode", &zlib(raw))
}

/// A one-page PDF with correct xref offsets whose content stream dictionary has
/// `dict_entries` (after `/Length`) and whose stream bytes are `data`.
pub fn pdf_with_content_stream(dict_entries: &str, data: &[u8]) -> Vec<u8> {
    let separator = if dict_entries.is_empty() { "" } else { " " };
    let mut stream = format!("<< /Length {}{separator}{dict_entries} >>\nstream\n", data.len()).into_bytes();
    stream.extend_from_slice(data);
    stream.extend_from_slice(b"\nendstream");
    let objects: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
            .to_vec(),
        stream,
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
    ];
    pdf_of(&objects)
}

/// A PDF of `objects` (numbered from 1, the first is the catalog) with its xref table.
fn pdf_of(objects: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
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
