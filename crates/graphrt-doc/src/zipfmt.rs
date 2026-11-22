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

impl SafeZip<'_> {
    /// Decompress every entry (discarding the bytes) to prove the whole archive fits
    /// the decompressed-size budget. Used before handing an archive to a parser that
    /// has no limits of its own.
    #[cfg(feature = "spreadsheet")]
    pub(crate) fn check_total_size(&mut self) -> Result<(), DocError> {
        for index in 0..self.archive.len() {
            let limit = self.budget;
            let entry = self.archive.by_index(index).map_err(|e| DocError::malformed("zip", e))?;
            let read = std::io::copy(&mut entry.take(limit.saturating_add(1)), &mut std::io::sink())
                .map_err(|e| DocError::malformed("zip", e))?;
            if read > limit {
                return Err(DocError::LimitExceeded("decompressed archive content exceeds the size limit".into()));
            }
            self.budget = self.budget.saturating_sub(read);
        }
        Ok(())
    }
}

/// Check that every sheet of an ODS `content.xml` fits the row, column and cell
/// limits, accounting for `number-rows-repeated` / `number-columns-repeated`.
/// This bounds the dense range a spreadsheet parser would allocate.
#[cfg(feature = "spreadsheet")]
pub(crate) fn check_ods_dimensions(xml: &str, opts: &Options) -> Result<(), DocError> {
    let mut reader = Reader::from_str(xml);
    let mut sheet: Option<Bounds> = None;
    let mut row_index: u64 = 0;
    // (rows repeated, used column span) of the row being read.
    let mut row: Option<(u64, Option<(u64, u64)>)> = None;
    let mut col: u64 = 0;
    loop {
        let event = reader.read_event().map_err(xml_error)?;
        let is_start = matches!(event, Event::Start(_));
        match &event {
            Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                "table" => {
                    sheet = Some(Bounds::default());
                    row_index = 0;
                }
                "table-row" => {
                    let repeated = repeat_count(e, "number-rows-repeated")?;
                    col = 0;
                    if is_start {
                        row = Some((repeated, None));
                    } else {
                        row_index = row_index.saturating_add(repeated);
                    }
                }
                "table-cell" | "covered-table-cell" => {
                    let repeated = repeat_count(e, "number-columns-repeated")?;
                    let has_value = is_start || attribute(e, "value-type")?.is_some();
                    if has_value && let Some((_, used)) = row.as_mut() {
                        let last = col.saturating_add(repeated).saturating_sub(1);
                        *used = Some(match *used {
                            Some((lo, hi)) => (lo.min(col), hi.max(last)),
                            None => (col, last),
                        });
                    }
                    col = col.saturating_add(repeated);
                }
                _ => {}
            },
            Event::End(e) => match e.local_name().as_ref() {
                "table-row" => {
                    if let Some((repeated, used)) = row.take() {
                        if let (Some((lo, hi)), Some(bounds)) = (used, sheet.as_mut()) {
                            bounds.include(row_index, row_index.saturating_add(repeated).saturating_sub(1), lo, hi);
                        }
                        row_index = row_index.saturating_add(repeated);
                    }
                }
                "table" => {
                    if let Some(bounds) = sheet.take() {
                        bounds.check(opts)?;
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(())
}

/// Bounding box of the used cells of one sheet.
#[cfg(feature = "spreadsheet")]
#[derive(Debug, Default)]
struct Bounds {
    used: Option<(u64, u64, u64, u64)>,
}

#[cfg(feature = "spreadsheet")]
impl Bounds {
    fn include(&mut self, row_lo: u64, row_hi: u64, col_lo: u64, col_hi: u64) {
        self.used = Some(match self.used {
            Some((r0, r1, c0, c1)) => (r0.min(row_lo), r1.max(row_hi), c0.min(col_lo), c1.max(col_hi)),
            None => (row_lo, row_hi, col_lo, col_hi),
        });
    }

    fn check(&self, opts: &Options) -> Result<(), DocError> {
        let Some((r0, r1, c0, c1)) = self.used else { return Ok(()) };
        let rows = r1.saturating_sub(r0).saturating_add(1);
        let cols = c1.saturating_sub(c0).saturating_add(1);
        check_sheet_dimensions(rows, cols, opts)
    }
}

/// Reject a sheet whose used range exceeds the row, column or cell limits.
#[cfg(feature = "spreadsheet")]
pub(crate) fn check_sheet_dimensions(rows: u64, cols: u64, opts: &Options) -> Result<(), DocError> {
    let as_u64 = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
    if rows > as_u64(opts.max_rows) {
        return Err(DocError::LimitExceeded(format!("sheet has {rows} rows (limit {})", opts.max_rows)));
    }
    if cols > as_u64(opts.max_cols) {
        return Err(DocError::LimitExceeded(format!("sheet has {cols} columns (limit {})", opts.max_cols)));
    }
    let cells = rows.saturating_mul(cols);
    if cells > as_u64(opts.max_cells) {
        return Err(DocError::LimitExceeded(format!("sheet spans {cells} cells (limit {})", opts.max_cells)));
    }
    Ok(())
}

#[cfg(feature = "spreadsheet")]
fn repeat_count(e: &quick_xml::events::BytesStart<'_>, name: &str) -> Result<u64, DocError> {
    match attribute(e, name)? {
        Some(v) => v.trim().parse::<u64>().map_err(|_| DocError::malformed("ods", format!("invalid `{name}` value"))),
        None => Ok(1),
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
    let mut body = DocxBody::default();
    loop {
        match reader.read_event().map_err(xml_error)? {
            Event::Start(e) => body.open(e.local_name().as_ref()),
            Event::Empty(e) => body.empty(e.local_name().as_ref()),
            Event::Text(t) if body.in_text => body.target().push_str(&t.xml10_content()),
            Event::GeneralRef(r) if body.in_text => body.target().push_str(&entity_text(&r)),
            Event::End(e) => body.close(e.local_name().as_ref(), opts)?,
            Event::Eof => break,
            _ => {}
        }
        // Stop parsing well past the output cap; the caller truncates.
        if body.out.len() > opts.max_output_chars.saturating_mul(4) {
            break;
        }
    }
    flush_paragraph(&mut body.out, &mut body.paragraph);
    Ok(body.out)
}

/// Text collected while walking `word/document.xml`. Outside tables text goes
/// to the current paragraph; inside a table, to the current cell. Nested tables
/// are flattened into the cell of the outermost one.
#[derive(Default)]
struct DocxBody {
    out: String,
    paragraph: String,
    /// Inside a `<w:t>` text run.
    in_text: bool,
    /// Table nesting depth; 0 outside tables.
    table_depth: usize,
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    cell: String,
}

impl DocxBody {
    /// Where text, tabs and breaks go right now.
    fn target(&mut self) -> &mut String {
        if self.table_depth > 0 { &mut self.cell } else { &mut self.paragraph }
    }

    fn open(&mut self, name: &str) {
        match name {
            "t" => self.in_text = true,
            "tbl" => {
                self.table_depth += 1;
                if self.table_depth == 1 {
                    flush_paragraph(&mut self.out, &mut self.paragraph);
                    self.rows.clear();
                }
            }
            "tr" if self.table_depth == 1 => self.row.clear(),
            "tc" if self.table_depth == 1 => self.cell.clear(),
            _ => {}
        }
    }

    fn empty(&mut self, name: &str) {
        match name {
            "tab" => self.target().push('\t'),
            "br" | "cr" => self.target().push('\n'),
            _ => {}
        }
    }

    fn close(&mut self, name: &str, opts: &Options) -> Result<(), DocError> {
        match name {
            "t" => self.in_text = false,
            "p" => self.end_paragraph(),
            "tc" if self.table_depth == 1 => self.row.push(self.cell.trim().to_owned()),
            "tr" if self.table_depth == 1 => self.rows.push(std::mem::take(&mut self.row)),
            "tbl" => self.end_table(opts)?,
            _ => {}
        }
        Ok(())
    }

    /// A paragraph inside a cell is a line of that cell; outside, a block of the output.
    fn end_paragraph(&mut self) {
        if self.table_depth == 0 {
            flush_paragraph(&mut self.out, &mut self.paragraph);
        } else if !self.cell.is_empty() && !self.cell.ends_with('\n') {
            self.cell.push('\n');
        }
    }

    fn end_table(&mut self, opts: &Options) -> Result<(), DocError> {
        self.table_depth = self.table_depth.saturating_sub(1);
        if self.table_depth > 0 {
            return Ok(());
        }
        let table = crate::text::render_table(&self.rows, opts)?;
        if !table.is_empty() {
            push_block(&mut self.out, &table);
        }
        self.rows.clear();
        Ok(())
    }
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
        if b == b'%'
            && let Some(v) = bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
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
