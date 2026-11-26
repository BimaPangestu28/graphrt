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
//! - PDF streams are inflated with a running budget before the document is
//!   loaded, and the document is parsed once.
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

/// Characters still available in the output budget.
#[cfg(feature = "spreadsheet")]
fn remaining_budget(opts: &Options, used: usize) -> Options {
    let mut sheet = opts.clone();
    sheet.max_output_chars = opts.max_output_chars.saturating_sub(used);
    sheet
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

/// XLSX through calamine's streaming cell reader: only non-empty cells are kept,
/// so a sheet declaring a huge used range cannot force a dense allocation.
#[cfg(feature = "spreadsheet")]
fn xlsx(bytes: &[u8], opts: &Options) -> Result<String, DocError> {
    use std::collections::BTreeMap;

    use calamine::Reader;

    crate::zipfmt::SafeZip::open(bytes, opts)?.check_total_size()?;
    guarded("spreadsheet", || {
        let mut workbook = calamine::Xlsx::new(std::io::Cursor::new(bytes))
            .map_err(|e: calamine::XlsxError| DocError::malformed("spreadsheet", e))?;
        let names = workbook.sheet_names();
        check_sheet_count(names.len(), opts)?;
        let max_rows = u32::try_from(opts.max_rows).unwrap_or(u32::MAX);
        let max_cols = u32::try_from(opts.max_cols).unwrap_or(u32::MAX);
        let mut parts: Vec<String> = Vec::new();
        let mut used = 0usize;
        for name in names {
            let mut rows: BTreeMap<u32, BTreeMap<u32, String>> = BTreeMap::new();
            let (mut min_col, mut max_col) = (u32::MAX, 0u32);
            let mut cells = 0usize;
            {
                let mut reader =
                    workbook.worksheet_cells_reader(&name).map_err(|e| DocError::malformed("spreadsheet", e))?;
                while let Some(cell) = reader.next_cell().map_err(|e| DocError::malformed("spreadsheet", e))? {
                    let text = calamine::Data::from(cell.get_value().clone()).to_string();
                    if text.is_empty() {
                        continue;
                    }
                    let (row, col) = cell.get_position();
                    if row >= max_rows {
                        return Err(DocError::LimitExceeded(format!(
                            "sheet `{name}` uses row {} (limit {})",
                            u64::from(row) + 1,
                            opts.max_rows
                        )));
                    }
                    if col >= max_cols {
                        return Err(DocError::LimitExceeded(format!(
                            "sheet `{name}` uses column {} (limit {})",
                            u64::from(col) + 1,
                            opts.max_cols
                        )));
                    }
                    cells += 1;
                    if cells > opts.max_cells {
                        return Err(DocError::LimitExceeded(format!(
                            "sheet `{name}` has more than {} non-empty cells",
                            opts.max_cells
                        )));
                    }
                    min_col = min_col.min(col);
                    max_col = max_col.max(col);
                    rows.entry(row).or_default().insert(col, text);
                }
            }
            if rows.is_empty() {
                continue;
            }
            let width = usize::try_from(max_col.saturating_sub(min_col)).unwrap_or(usize::MAX).saturating_add(1);
            let mut table = crate::text::TableWriter::new(width, &remaining_budget(opts, used))?;
            for columns in rows.values() {
                let row = (min_col..=max_col).map(|c| columns.get(&c).map_or("", String::as_str));
                if !table.row(row) {
                    break;
                }
            }
            let part = format!("{}\n{}", sheet_heading(&name, opts), table.finish());
            used = used.saturating_add(part.chars().count());
            parts.push(part);
            if used > opts.max_output_chars {
                break;
            }
        }
        Ok(parts.join("\n\n"))
    })
}

/// ODS: dimensions (with repeated rows/columns) are checked on `content.xml`
/// before calamine builds its dense per-sheet grid.
#[cfg(feature = "spreadsheet")]
fn ods(bytes: &[u8], opts: &Options) -> Result<String, DocError> {
    use calamine::Reader;

    crate::zipfmt::SafeZip::open(bytes, opts)?.check_total_size()?;
    let content = crate::zipfmt::SafeZip::open(bytes, opts)?
        .read_text("content.xml")?
        .ok_or_else(|| DocError::malformed("ods", "missing content.xml"))?;
    crate::zipfmt::check_ods_dimensions(&content, opts)?;
    drop(content);
    guarded("spreadsheet", || {
        let mut workbook = calamine::Ods::new(std::io::Cursor::new(bytes))
            .map_err(|e: calamine::OdsError| DocError::malformed("spreadsheet", e))?;
        let names = workbook.sheet_names();
        check_sheet_count(names.len(), opts)?;
        let mut parts = Vec::new();
        let mut used = 0usize;
        for name in names {
            let range = workbook.worksheet_range(&name).map_err(|e| DocError::malformed("spreadsheet", e))?;
            if let Some(part) = render_range(&name, &range, opts, used)? {
                used = used.saturating_add(part.chars().count());
                parts.push(part);
                if used > opts.max_output_chars {
                    break;
                }
            }
        }
        Ok(parts.join("\n\n"))
    })
}

/// Legacy XLS (BIFF), only with the `xls` feature. The parser materializes every
/// sheet as a dense grid when the workbook is opened, so this format cannot be
/// bounded the way XLSX and ODS are; enable it only for trusted input.
#[cfg(feature = "spreadsheet")]
fn xls(bytes: &[u8], opts: &Options) -> Result<String, DocError> {
    #[cfg(feature = "xls")]
    {
        use calamine::Reader;

        guarded("spreadsheet", || {
            let mut workbook = calamine::Xls::new(std::io::Cursor::new(bytes))
                .map_err(|e: calamine::XlsError| DocError::malformed("spreadsheet", e))?;
            let names = workbook.sheet_names();
            check_sheet_count(names.len(), opts)?;
            let mut parts = Vec::new();
            let mut used = 0usize;
            for name in names {
                let range = workbook.worksheet_range(&name).map_err(|e| DocError::malformed("spreadsheet", e))?;
                if let Some(part) = render_range(&name, &range, opts, used)? {
                    used = used.saturating_add(part.chars().count());
                    parts.push(part);
                    if used > opts.max_output_chars {
                        break;
                    }
                }
            }
            Ok(parts.join("\n\n"))
        })
    }
    #[cfg(not(feature = "xls"))]
    {
        let _ = (bytes, opts);
        Err(DocError::FeatureDisabled { format: crate::Format::Xls, feature: "xls" })
    }
}

/// Render a dense calamine range as one sheet section (`None` when the sheet is empty).
#[cfg(feature = "spreadsheet")]
fn render_range(
    name: &str,
    range: &calamine::Range<calamine::Data>,
    opts: &Options,
    used: usize,
) -> Result<Option<String>, DocError> {
    let rows = u64::try_from(range.height()).unwrap_or(u64::MAX);
    let cols = u64::try_from(range.width()).unwrap_or(u64::MAX);
    crate::zipfmt::check_sheet_dimensions(rows, cols, opts)?;
    let mut table = crate::text::TableWriter::new(range.width(), &remaining_budget(opts, used))?;
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
    check_pdf_streams(bytes, opts.max_decompressed_bytes)?;
    guarded("pdf", || {
        let mut doc = pdf_extract::Document::load_mem(bytes).map_err(|e| DocError::malformed("pdf", e))?;
        if doc.is_encrypted() {
            // Documents encrypted with an empty user password are readable; others fail here.
            doc.decrypt("").map_err(|e| DocError::malformed("pdf", format!("encrypted document: {e}")))?;
        }
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

/// Inflate every `FlateDecode` stream of a PDF into a sink with a shared budget,
/// rejecting documents whose streams expand beyond `budget` bytes (decompression
/// bombs) before the PDF parser inflates them without limits.
#[cfg(feature = "pdf")]
pub(crate) fn check_pdf_streams(bytes: &[u8], budget: u64) -> Result<(), DocError> {
    use std::io::Read;

    let mut remaining = budget;
    let mut pos = 0usize;
    while let Some(offset) = find(bytes.get(pos..).unwrap_or_default(), b"stream") {
        let keyword = pos + offset;
        let mut data_start = keyword + b"stream".len();
        // The keyword must end a dictionary (`>> stream`) and be followed by EOL.
        let is_stream =
            bytes.get(keyword.saturating_sub(16)..keyword).is_some_and(|before| before.windows(2).any(|w| w == b">>"))
                && bytes.get(keyword.saturating_sub(3)..keyword).is_none_or(|b| b != b"end");
        match bytes.get(data_start..data_start + 2) {
            Some(b"\r\n") => data_start += 2,
            Some([b'\n', _]) | Some([b'\r', _]) => data_start += 1,
            _ => {}
        }
        if !is_stream {
            pos = keyword + 1;
            continue;
        }
        let Some(end_offset) = find(bytes.get(data_start..).unwrap_or_default(), b"endstream") else {
            break;
        };
        let data_end = data_start + end_offset;
        // Look for the dictionary start only in a bounded window, keeping the scan linear.
        let window_start = keyword.saturating_sub(4096);
        let dict_start = rfind(bytes.get(window_start..keyword).unwrap_or_default(), b"<<")
            .map_or(window_start, |p| window_start + p);
        let dict = bytes.get(dict_start..keyword).unwrap_or_default();
        if find(dict, b"/FlateDecode").is_some() || find(dict, b"/Fl ").is_some() || find(dict, b"/Fl/").is_some() {
            let data = bytes.get(data_start..data_end).unwrap_or_default();
            let mut decoder = flate2::read::ZlibDecoder::new(data).take(remaining.saturating_add(1));
            // Corrupt streams are left for the parser to report; only the size matters here.
            let inflated = std::io::copy(&mut decoder, &mut std::io::sink()).unwrap_or(0);
            if inflated > remaining {
                return Err(DocError::LimitExceeded(format!("PDF streams decompress to more than {budget} bytes")));
            }
            remaining -= inflated;
        }
        pos = data_end + b"endstream".len();
    }
    Ok(())
}

#[cfg(feature = "pdf")]
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(feature = "pdf")]
fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).rposition(|w| w == needle)
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
