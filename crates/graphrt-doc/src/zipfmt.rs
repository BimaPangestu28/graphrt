//! Zip-based formats (DOCX, PPTX, EPUB) with zip-bomb protection.

use std::io::{Cursor, Read};

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::detect::Format;
use crate::{DocError, Options, OutputFormat};

/// A zip archive read with entry-count and decompressed-size limits.
pub(crate) struct SafeZip<'a> {
    archive: zip::ZipArchive<Cursor<&'a [u8]>>,
    budget: u64,
    max_entry: u64,
}

impl<'a> SafeZip<'a> {
    pub(crate) fn open(bytes: &'a [u8], opts: &Options) -> Result<Self, DocError> {
        let archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| DocError::malformed("zip", e))?;
        if archive.len() > opts.max_zip_entries {
            return Err(DocError::LimitExceeded(format!(
                "archive has {} entries (limit {})",
                archive.len(),
                opts.max_zip_entries
            )));
        }
        Ok(Self { archive, budget: opts.max_decompressed_bytes, max_entry: opts.max_decompressed_bytes })
    }

    pub(crate) fn names(&self) -> Vec<String> {
        // Entries with undecodable names are skipped: they cannot be any of the
        // well-known paths this module looks for.
        self.archive.file_names().filter_map(Result::ok).map(|n| n.into_owned()).collect()
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.archive.index_for_name(name).is_some()
    }

    /// Read an entry as UTF-8 text, charging its decompressed size to the budget.
    /// Returns `Ok(None)` when the entry does not exist.
    pub(crate) fn read_text(&mut self, name: &str) -> Result<Option<String>, DocError> {
        let Some(index) = self.archive.index_for_name(name) else {
            return Ok(None);
        };
        let limit = self.budget.min(self.max_entry);
        let entry = self.archive.by_index(index).map_err(|e| DocError::malformed("zip", e))?;
        let mut buf = Vec::new();
        entry.take(limit.saturating_add(1)).read_to_end(&mut buf).map_err(|e| DocError::malformed("zip", e))?;
        let read = u64::try_from(buf.len()).unwrap_or(u64::MAX);
        if read > limit {
            return Err(DocError::LimitExceeded("decompressed archive content exceeds the size limit".into()));
        }
        self.budget = self.budget.saturating_sub(read);
        Ok(Some(crate::text::decode(&buf)))
    }
}

/// Identify the format of an Office/EPUB zip container.
pub(crate) fn identify(bytes: &[u8], opts: &Options) -> Result<Option<Format>, DocError> {
    let mut zip = SafeZip::open(bytes, opts)?;
    if let Some(mime) = zip.read_text("mimetype")? {
        match mime.trim() {
            "application/epub+zip" => return Ok(Some(Format::Epub)),
            "application/vnd.oasis.opendocument.spreadsheet" => return Ok(Some(Format::Ods)),
            _ => {}
        }
    }
    if zip.contains("word/document.xml") {
        return Ok(Some(Format::Docx));
    }
    if zip.contains("ppt/presentation.xml") {
        return Ok(Some(Format::Pptx));
    }
    if zip.contains("xl/workbook.xml") {
        return Ok(Some(Format::Xlsx));
    }
    Ok(None)
}

fn xml_error(e: impl std::fmt::Display) -> DocError {
    DocError::malformed("xml", e)
}

/// Resolve an XML entity reference (`&amp;`, `&#x41;`) to text.
fn entity_text(r: &quick_xml::events::BytesRef<'_>) -> String {
    match r.resolve_char_ref() {
        Ok(Some(c)) => c.to_string(),
        _ => crate::text::decode_entities(&format!("&{};", &**r)),
    }
}

/// Extract DOCX body text: paragraphs, line breaks, tabs and tables.
pub(crate) fn docx(bytes: &[u8], opts: &Options) -> Result<String, DocError> {
    let mut zip = SafeZip::open(bytes, opts)?;
    let xml =
        zip.read_text("word/document.xml")?.ok_or_else(|| DocError::malformed("docx", "missing word/document.xml"))?;
    let mut reader = Reader::from_str(&xml);
    let mut out = String::new();
    let mut paragraph = String::new();
    let mut in_text = false;
    // Table state: depth > 0 while inside a table; rows collect cells.
    let mut table_depth = 0usize;
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cell = String::new();
    loop {
        match reader.read_event().map_err(xml_error)? {
            Event::Start(e) => match e.local_name().as_ref() {
                "t" => in_text = true,
                "tbl" => {
                    table_depth += 1;
                    if table_depth == 1 {
                        flush_paragraph(&mut out, &mut paragraph);
                        rows.clear();
                    }
                }
                "tr" if table_depth == 1 => row.clear(),
                "tc" if table_depth == 1 => cell.clear(),
                _ => {}
            },
            Event::Empty(e) => {
                let target = if table_depth > 0 { &mut cell } else { &mut paragraph };
                match e.local_name().as_ref() {
                    "tab" => target.push('\t'),
                    "br" | "cr" => target.push('\n'),
                    _ => {}
                }
            }
            Event::Text(t) if in_text => {
                let target = if table_depth > 0 { &mut cell } else { &mut paragraph };
                target.push_str(&t.xml10_content());
            }
            Event::GeneralRef(r) if in_text => {
                let target = if table_depth > 0 { &mut cell } else { &mut paragraph };
                target.push_str(&entity_text(&r));
            }
            Event::End(e) => match e.local_name().as_ref() {
                "t" => in_text = false,
                "p" => {
                    if table_depth > 0 {
                        if !cell.is_empty() && !cell.ends_with('\n') {
                            cell.push('\n');
                        }
                    } else {
                        flush_paragraph(&mut out, &mut paragraph);
                    }
                }
                "tc" if table_depth == 1 => row.push(cell.trim().to_owned()),
                "tr" if table_depth == 1 => rows.push(std::mem::take(&mut row)),
                "tbl" => {
                    table_depth = table_depth.saturating_sub(1);
                    if table_depth == 0 {
                        let table = crate::text::render_table(&rows, opts.format);
                        if !table.is_empty() {
                            push_block(&mut out, &table);
                        }
                        rows.clear();
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        if out.len() > opts.max_output_chars.saturating_mul(4) {
            break;
        }
    }
    flush_paragraph(&mut out, &mut paragraph);
    Ok(out)
}

fn flush_paragraph(out: &mut String, paragraph: &mut String) {
    let text = paragraph.trim_end();
    if !text.trim().is_empty() {
        push_block(out, text);
    }
    paragraph.clear();
}

fn push_block(out: &mut String, block: &str) {
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(block);
}

/// Extract PPTX slide text in slide order. Each slide starts with `Slide N` (text)
/// or `## Slide N` (Markdown).
pub(crate) fn pptx(bytes: &[u8], opts: &Options) -> Result<String, DocError> {
    let mut zip = SafeZip::open(bytes, opts)?;
    let mut slides: Vec<(u32, String)> = zip
        .names()
        .into_iter()
        .filter_map(|n| {
            let num = n.strip_prefix("ppt/slides/slide")?.strip_suffix(".xml")?.parse::<u32>().ok()?;
            Some((num, n))
        })
        .collect();
    slides.sort_by_key(|(n, _)| *n);
    if slides.len() > opts.max_pages {
        return Err(DocError::LimitExceeded(format!("{} slides (limit {})", slides.len(), opts.max_pages)));
    }
    let mut out = String::new();
    for (num, name) in slides {
        let Some(xml) = zip.read_text(&name)? else { continue };
        let paragraphs = drawing_paragraphs(&xml)?;
        if paragraphs.is_empty() {
            continue;
        }
        let heading = match opts.format {
            OutputFormat::Markdown => format!("## Slide {num}"),
            OutputFormat::Text => format!("Slide {num}"),
        };
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&heading);
        out.push('\n');
        out.push_str(&paragraphs.join("\n"));
    }
    Ok(out)
}

/// Text of `a:p` paragraphs (`a:t` runs) in DrawingML.
fn drawing_paragraphs(xml: &str) -> Result<Vec<String>, DocError> {
    let mut reader = Reader::from_str(xml);
    let mut paragraphs = Vec::new();
    let mut current = String::new();
    let mut in_text = false;
    loop {
        match reader.read_event().map_err(xml_error)? {
            Event::Start(e) if e.local_name().as_ref() == "t" => in_text = true,
            Event::Empty(e) if e.local_name().as_ref() == "br" => current.push('\n'),
            Event::Text(t) if in_text => current.push_str(&t.xml10_content()),
            Event::GeneralRef(r) if in_text => current.push_str(&entity_text(&r)),
            Event::End(e) => match e.local_name().as_ref() {
                "t" => in_text = false,
                "p" => {
                    let text = current.trim().to_owned();
                    if !text.is_empty() {
                        paragraphs.push(text);
                    }
                    current.clear();
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(paragraphs)
}

/// Extract EPUB chapter text in spine order (falling back to archive order).
pub(crate) fn epub(bytes: &[u8], opts: &Options) -> Result<String, DocError> {
    let mut zip = SafeZip::open(bytes, opts)?;
    let chapters = match epub_spine(&mut zip)? {
        Some(chapters) if !chapters.is_empty() => chapters,
        _ => {
            let mut names: Vec<String> = zip
                .names()
                .into_iter()
                .filter(|n| {
                    let lower = n.to_ascii_lowercase();
                    lower.ends_with(".xhtml") || lower.ends_with(".html") || lower.ends_with(".htm")
                })
                .collect();
            names.sort();
            names
        }
    };
    if chapters.len() > opts.max_pages {
        return Err(DocError::LimitExceeded(format!("{} chapters (limit {})", chapters.len(), opts.max_pages)));
    }
    let mut parts = Vec::new();
    for name in chapters {
        if let Some(html) = zip.read_text(&name)? {
            let text = crate::text::html_to_text(&html, opts.format);
            if !text.is_empty() {
                parts.push(text);
            }
        }
    }
    Ok(parts.join("\n\n"))
}

/// Chapter paths from the OPF spine, resolved relative to the OPF file.
fn epub_spine(zip: &mut SafeZip<'_>) -> Result<Option<Vec<String>>, DocError> {
    let Some(container) = zip.read_text("META-INF/container.xml")? else {
        return Ok(None);
    };
    let Some(opf_path) = first_attribute(&container, "rootfile", "full-path")? else {
        return Ok(None);
    };
    let Some(opf) = zip.read_text(&opf_path)? else {
        return Ok(None);
    };
    let base = opf_path.rsplit_once('/').map(|(dir, _)| format!("{dir}/")).unwrap_or_default();

    let mut manifest: Vec<(String, String)> = Vec::new();
    let mut spine: Vec<String> = Vec::new();
    let mut reader = Reader::from_str(&opf);
    loop {
        match reader.read_event().map_err(xml_error)? {
            Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                "item" => {
                    let id = attribute(&e, "id")?;
                    let href = attribute(&e, "href")?;
                    if let (Some(id), Some(href)) = (id, href) {
                        manifest.push((id, href));
                    }
                }
                "itemref" => {
                    if let Some(idref) = attribute(&e, "idref")? {
                        spine.push(idref);
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    let chapters = spine
        .iter()
        .filter_map(|idref| manifest.iter().find(|(id, _)| id == idref))
        .map(|(_, href)| format!("{base}{}", percent_decode_path(href)))
        .collect();
    Ok(Some(chapters))
}

fn percent_decode_path(href: &str) -> String {
    let href = href.split('#').next().unwrap_or(href);
    if !href.contains('%') {
        return href.to_owned();
    }
    let bytes = href.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            if let Some(v) = bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn attribute(e: &quick_xml::events::BytesStart<'_>, name: &str) -> Result<Option<String>, DocError> {
    for attr in e.attributes() {
        let attr = attr.map_err(xml_error)?;
        if attr.key.local_name().as_ref() == name {
            let value = attr.normalized_value(quick_xml::XmlVersion::Implicit1_0).map_err(xml_error)?;
            return Ok(Some(value.into_owned()));
        }
    }
    Ok(None)
}

fn first_attribute(xml: &str, element: &str, name: &str) -> Result<Option<String>, DocError> {
    let mut reader = Reader::from_str(xml);
    loop {
        match reader.read_event().map_err(xml_error)? {
            Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == element => return attribute(&e, name),
            Event::Eof => return Ok(None),
            _ => {}
        }
    }
}
