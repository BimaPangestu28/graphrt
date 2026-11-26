//! Binary formats backed by optional crates: spreadsheets, PDF and e-mail.
//!
//! The third-party parsers used here have no resource limits of their own, so
//! every input is checked first:
//!
//! - XLSX / ODS archives are fully decompressed once (discarding the bytes) to
//!   prove they fit [`Options::max_decompressed_bytes`]; XLSX cells are read with a
//!   streaming reader into a sparse map, and ODS sheet dimensions (including
//!   repeated rows/columns) are checked against the row, column and cell limits
//!   before the parser builds its dense grid.
//! - PDF streams are decoded with a running budget before the document is
//!   loaded and again before text is extracted ([`crate::pdf_budget`]).
//!
//! These checks bound memory, not CPU time spent inside the parsers. Run
//! untrusted extraction under a timeout (the node runs it on the blocking pool).

#[cfg(any(feature = "spreadsheet", feature = "pdf"))]
use std::panic::{AssertUnwindSafe, catch_unwind};

#[cfg(any(feature = "spreadsheet", feature = "pdf", feature = "email"))]
use crate::{DocError, Options};

/// Run a third-party parser, turning a panic into a [`DocError::Malformed`].
///
/// Note: this only helps in builds with `panic = "unwind"`. With `panic = "abort"`
/// a panicking parser terminates the process.
#[cfg(any(feature = "spreadsheet", feature = "pdf"))]
fn guarded<T>(format: &'static str, f: impl FnOnce() -> Result<T, DocError>) -> Result<T, DocError> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(_) => Err(DocError::malformed(format, "parser panicked on malformed input")),
    }
}

/// Spreadsheet (XLSX, ODS, and XLS with the `xls` feature): every sheet as a
/// table, preceded by its name.
#[cfg(feature = "spreadsheet")]
pub(crate) fn spreadsheet(bytes: &[u8], format: crate::Format, opts: &Options) -> Result<String, DocError> {
    match format {
        crate::Format::Xlsx => xlsx(bytes, opts),
        crate::Format::Ods => ods(bytes, opts),
        _ => xls(bytes, opts),
    }
}

/// Heading line placed before each sheet.
#[cfg(feature = "spreadsheet")]
fn sheet_heading(name: &str, opts: &Options) -> String {
    match opts.format {
        crate::OutputFormat::Markdown => format!("## {name}"),
        crate::OutputFormat::Text => name.to_owned(),
    }
}

#[cfg(feature = "spreadsheet")]
fn check_sheet_count(count: usize, opts: &Options) -> Result<(), DocError> {
    if count > opts.max_sheets {
        return Err(DocError::LimitExceeded(format!("{count} sheets (limit {})", opts.max_sheets)));
    }
    Ok(())
}

/// Rendered sheet sections and the output characters they use, so later sheets
/// get only what is left of [`Options::max_output_chars`].
#[cfg(feature = "spreadsheet")]
#[derive(Default)]
struct SheetSections {
    parts: Vec<String>,
    used: usize,
}

#[cfg(feature = "spreadsheet")]
impl SheetSections {
    /// The options for the next sheet: the output budget minus what earlier sheets used.
    fn remaining_budget(&self, opts: &Options) -> Options {
        let mut sheet = opts.clone();
        sheet.max_output_chars = opts.max_output_chars.saturating_sub(self.used);
        sheet
    }

    /// Add a section; `false` once the output budget is spent and no further sheet fits.
    fn push(&mut self, part: String, opts: &Options) -> bool {
        self.used = self.used.saturating_add(part.chars().count());
        self.parts.push(part);
        self.used <= opts.max_output_chars
    }

    fn finish(self) -> String {
        self.parts.join("\n\n")
    }
}

/// XLSX through calamine's streaming cell reader: only non-empty cells are kept,
/// so a sheet declaring a huge used range cannot force a dense allocation.
#[cfg(feature = "spreadsheet")]
fn xlsx(bytes: &[u8], opts: &Options) -> Result<String, DocError> {
    use calamine::Reader;

    crate::zipfmt::SafeZip::open(bytes, opts)?.check_total_size()?;
    guarded("spreadsheet", || {
        let mut workbook = calamine::Xlsx::new(std::io::Cursor::new(bytes))
            .map_err(|e: calamine::XlsxError| DocError::malformed("spreadsheet", e))?;
        let names = workbook.sheet_names();
        check_sheet_count(names.len(), opts)?;
        let mut sections = SheetSections::default();
        for name in names {
            let sheet = read_sparse_sheet(&mut workbook, &name, opts)?;
            let Some(part) = sheet.render(&name, &sections.remaining_budget(opts))? else { continue };
            if !sections.push(part, opts) {
                break;
            }
        }
        Ok(sections.finish())
    })
}

/// Stream the non-empty cells of one XLSX sheet, enforcing the row, column and cell limits.
#[cfg(feature = "spreadsheet")]
fn read_sparse_sheet(
    workbook: &mut calamine::Xlsx<std::io::Cursor<&[u8]>>,
    name: &str,
    opts: &Options,
) -> Result<SparseSheet, DocError> {
    let mut sheet = SparseSheet::default();
    let mut reader = workbook.worksheet_cells_reader(name).map_err(|e| DocError::malformed("spreadsheet", e))?;
    while let Some(cell) = reader.next_cell().map_err(|e| DocError::malformed("spreadsheet", e))? {
        let text = calamine::Data::from(cell.get_value().clone()).to_string();
        if text.is_empty() {
            continue;
        }
        let (row, col) = cell.get_position();
        sheet.insert(name, (row, col), text, opts)?;
    }
    Ok(sheet)
}

/// Non-empty cells of a sheet by row and column, plus the column span they cover.
#[cfg(feature = "spreadsheet")]
struct SparseSheet {
    rows: std::collections::BTreeMap<u32, std::collections::BTreeMap<u32, String>>,
    min_col: u32,
    max_col: u32,
    cells: usize,
}

#[cfg(feature = "spreadsheet")]
impl Default for SparseSheet {
    fn default() -> Self {
        Self { rows: std::collections::BTreeMap::new(), min_col: u32::MAX, max_col: 0, cells: 0 }
    }
}

#[cfg(feature = "spreadsheet")]
impl SparseSheet {
    /// Add a cell of sheet `name` at zero-based `(row, col)`, failing once the sheet
    /// goes past the row, column or non-empty cell limit.
    fn insert(&mut self, name: &str, (row, col): (u32, u32), text: String, opts: &Options) -> Result<(), DocError> {
        check_cell_position(name, row, col, opts)?;
        self.cells += 1;
        if self.cells > opts.max_cells {
            return Err(DocError::LimitExceeded(format!(
                "sheet `{name}` has more than {} non-empty cells",
                opts.max_cells
            )));
        }
        self.min_col = self.min_col.min(col);
        self.max_col = self.max_col.max(col);
        self.rows.entry(row).or_default().insert(col, text);
        Ok(())
    }

    /// The sheet as a section (`None` when it has no non-empty cell). Empty rows
    /// and the columns left of the first used one are not rendered.
    fn render(&self, name: &str, opts: &Options) -> Result<Option<String>, DocError> {
        if self.rows.is_empty() {
            return Ok(None);
        }
        let width = usize::try_from(self.max_col.saturating_sub(self.min_col)).unwrap_or(usize::MAX).saturating_add(1);
        let mut table = crate::text::TableWriter::new(width, opts)?;
        for columns in self.rows.values() {
            let row = (self.min_col..=self.max_col).map(|c| columns.get(&c).map_or("", String::as_str));
            if !table.row(row) {
                break;
            }
        }
        Ok(Some(format!("{}\n{}", sheet_heading(name, opts), table.finish())))
    }
}

/// Fail when a zero-based cell position lies past the row or column limit.
#[cfg(feature = "spreadsheet")]
fn check_cell_position(name: &str, row: u32, col: u32, opts: &Options) -> Result<(), DocError> {
    // Cell positions are u32; a limit past u32::MAX caps at the last position.
    let beyond = |index: u32, limit: usize| index >= u32::try_from(limit).unwrap_or(u32::MAX);
    if beyond(row, opts.max_rows) {
        return Err(DocError::LimitExceeded(format!(
            "sheet `{name}` uses row {} (limit {})",
            u64::from(row) + 1,
            opts.max_rows
        )));
    }
    if beyond(col, opts.max_cols) {
        return Err(DocError::LimitExceeded(format!(
            "sheet `{name}` uses column {} (limit {})",
            u64::from(col) + 1,
            opts.max_cols
        )));
    }
    Ok(())
}

/// ODS: dimensions (with repeated rows/columns) are checked on `content.xml`
/// before calamine builds its dense per-sheet grid.
#[cfg(feature = "spreadsheet")]
fn ods(bytes: &[u8], opts: &Options) -> Result<String, DocError> {
    crate::zipfmt::SafeZip::open(bytes, opts)?.check_total_size()?;
    let content = crate::zipfmt::SafeZip::open(bytes, opts)?
        .read_text("content.xml")?
        .ok_or_else(|| DocError::malformed("ods", "missing content.xml"))?;
    crate::zipfmt::check_ods_dimensions(&content, opts)?;
    drop(content);
    guarded("spreadsheet", || {
        let workbook = <calamine::Ods<_> as calamine::Reader<_>>::new(std::io::Cursor::new(bytes))
            .map_err(|e: calamine::OdsError| DocError::malformed("spreadsheet", e))?;
        dense_workbook(workbook, opts)
    })
}

/// Legacy XLS (BIFF), only with the `xls` feature. The parser materializes every
/// sheet as a dense grid when the workbook is opened, so this format cannot be
/// bounded the way XLSX and ODS are; enable it only for trusted input.
#[cfg(feature = "spreadsheet")]
fn xls(bytes: &[u8], opts: &Options) -> Result<String, DocError> {
    #[cfg(feature = "xls")]
    {
        guarded("spreadsheet", || {
            let workbook = <calamine::Xls<_> as calamine::Reader<_>>::new(std::io::Cursor::new(bytes))
                .map_err(|e: calamine::XlsError| DocError::malformed("spreadsheet", e))?;
            dense_workbook(workbook, opts)
        })
    }
    #[cfg(not(feature = "xls"))]
    {
        let _ = (bytes, opts);
        Err(DocError::FeatureDisabled { format: crate::Format::Xls, feature: "xls" })
    }
}

/// Every sheet of a workbook whose reader loads dense ranges (ODS, XLS).
#[cfg(feature = "spreadsheet")]
fn dense_workbook<RS, R>(mut workbook: R, opts: &Options) -> Result<String, DocError>
where
    RS: std::io::Read + std::io::Seek,
    R: calamine::Reader<RS>,
    R::Error: std::fmt::Display,
{
    let names = workbook.sheet_names();
    check_sheet_count(names.len(), opts)?;
    let mut sections = SheetSections::default();
    for name in names {
        let range = workbook.worksheet_range(&name).map_err(|e| DocError::malformed("spreadsheet", e))?;
        let Some(part) = render_range(&name, &range, opts, &sections.remaining_budget(opts))? else { continue };
        if !sections.push(part, opts) {
            break;
        }
    }
    Ok(sections.finish())
}

/// Render a dense calamine range as one sheet section (`None` when the sheet is
/// empty). `opts` sets the dimension limits, `budget` the output still available.
#[cfg(feature = "spreadsheet")]
fn render_range(
    name: &str,
    range: &calamine::Range<calamine::Data>,
    opts: &Options,
    budget: &Options,
) -> Result<Option<String>, DocError> {
    let rows = u64::try_from(range.height()).unwrap_or(u64::MAX);
    let cols = u64::try_from(range.width()).unwrap_or(u64::MAX);
    crate::zipfmt::check_sheet_dimensions(rows, cols, opts)?;
    let mut table = crate::text::TableWriter::new(range.width(), budget)?;
    let mut any = false;
    for row in range.rows() {
        let cells: Vec<String> = row.iter().map(ToString::to_string).collect();
        if cells.iter().all(String::is_empty) {
            continue;
        }
        any = true;
        if !table.row(cells.iter().map(String::as_str)) {
            break;
        }
    }
    if !any {
        return Ok(None);
    }
    Ok(Some(format!("{}\n{}", sheet_heading(name, opts), table.finish())))
}

/// PDF text, page by page (pages separated by a blank line). With
/// [`Options::page_markers`] each page starts with its [`crate::page_marker`]
/// (page numbers count from 1 and include pages without text).
#[cfg(feature = "pdf")]
pub(crate) fn pdf(bytes: &[u8], opts: &Options) -> Result<String, DocError> {
    crate::pdf_budget::check_raw_streams(bytes, opts.max_decompressed_bytes)?;
    guarded("pdf", || {
        let mut doc = pdf_extract::Document::load_mem(bytes).map_err(|e| DocError::malformed("pdf", e))?;
        if doc.is_encrypted() {
            // Documents encrypted with an empty user password are readable; others fail here.
            doc.decrypt("").map_err(|e| DocError::malformed("pdf", format!("encrypted document: {e}")))?;
        }
        // The raw scan is a heuristic and cannot read encrypted streams; this
        // sees every stream as the extractor will decode it.
        crate::pdf_budget::check_loaded_streams(&doc, opts.max_decompressed_bytes)?;
        let pages = doc.get_pages();
        if pages.len() > opts.max_pages {
            return Err(DocError::LimitExceeded(format!("{} pages (limit {})", pages.len(), opts.max_pages)));
        }
        let mut out: Vec<String> = Vec::new();
        let mut chars = 0usize;
        let mut first_error: Option<String> = None;
        for (page, number) in pages.keys().zip(1u32..) {
            let mut text = String::new();
            let result = {
                let mut output = pdf_extract::PlainTextOutput::new(&mut text);
                pdf_extract::output_doc_page(&doc, &mut output, *page)
            };
            if let Err(e) = result {
                first_error.get_or_insert_with(|| format!("page {page}: {e}"));
                continue;
            }
            let text = crate::text::collapse_blank_lines(&text);
            if text.is_empty() {
                continue;
            }
            chars = chars.saturating_add(text.chars().count());
            out.push(if opts.page_markers { format!("{}{text}", crate::page_marker(number)) } else { text });
            if chars > opts.max_output_chars {
                break;
            }
        }
        match (out.is_empty(), first_error) {
            (true, Some(error)) => Err(DocError::malformed("pdf", error)),
            _ => Ok(out.join("\n\n")),
        }
    })
}

/// E-mail: main headers followed by the text body (HTML converted when there is no text part).
#[cfg(feature = "email")]
pub(crate) fn email(bytes: &[u8], opts: &Options) -> Result<String, DocError> {
    let message = mail_parser::MessageParser::default()
        .parse(bytes)
        .ok_or_else(|| DocError::malformed("eml", "not an RFC 5322 message"))?;
    let addresses = |list: Option<&mail_parser::Address<'_>>| -> Option<String> {
        let list = list?;
        let rendered: Vec<String> = list
            .iter()
            .map(|m| match (m.name.as_deref(), m.address.as_deref()) {
                (Some(name), Some(addr)) => format!("{name} <{addr}>"),
                (None, Some(addr)) => addr.to_owned(),
                (Some(name), None) => name.to_owned(),
                (None, None) => String::new(),
            })
            .filter(|s| !s.is_empty())
            .collect();
        if rendered.is_empty() { None } else { Some(rendered.join(", ")) }
    };
    let mut lines = Vec::new();
    if let Some(from) = addresses(message.from()) {
        lines.push(format!("From: {from}"));
    }
    if let Some(to) = addresses(message.to()) {
        lines.push(format!("To: {to}"));
    }
    if let Some(cc) = addresses(message.cc()) {
        lines.push(format!("Cc: {cc}"));
    }
    if let Some(date) = message.date() {
        lines.push(format!("Date: {}", date.to_rfc3339()));
    }
    if let Some(subject) = message.subject() {
        lines.push(format!("Subject: {subject}"));
    }
    let body = match message.body_text(0) {
        Some(text) => crate::text::collapse_blank_lines(&text),
        None => message.body_html(0).map(|html| crate::text::html_to_text(&html, opts.format)).unwrap_or_default(),
    };
    let header = lines.join("\n");
    Ok(match (header.is_empty(), body.is_empty()) {
        (true, _) => body,
        (false, true) => header,
        (false, false) => format!("{header}\n\n{body}"),
    })
}
