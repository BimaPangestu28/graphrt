//! Binary formats backed by optional crates: spreadsheets, PDF and e-mail.

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

/// Spreadsheet (XLSX, XLS, ODS): every sheet as a table, preceded by its name.
#[cfg(feature = "spreadsheet")]
pub(crate) fn spreadsheet(bytes: &[u8], opts: &Options) -> Result<String, DocError> {
    use calamine::Reader;

    guarded("spreadsheet", || {
        let cursor = std::io::Cursor::new(bytes);
        let mut workbook =
            calamine::open_workbook_auto_from_rs(cursor).map_err(|e| DocError::malformed("spreadsheet", e))?;
        let names = workbook.sheet_names();
        if names.len() > opts.max_sheets {
            return Err(DocError::LimitExceeded(format!("{} sheets (limit {})", names.len(), opts.max_sheets)));
        }
        let mut parts = Vec::new();
        for name in names {
            let range = workbook.worksheet_range(&name).map_err(|e| DocError::malformed("spreadsheet", e))?;
            if range.height() > opts.max_rows {
                return Err(DocError::LimitExceeded(format!(
                    "sheet `{name}` has {} rows (limit {})",
                    range.height(),
                    opts.max_rows
                )));
            }
            let rows: Vec<Vec<String>> = range
                .rows()
                .map(|r| r.iter().map(ToString::to_string).collect::<Vec<String>>())
                .filter(|r| r.iter().any(|c| !c.is_empty()))
                .collect();
            if rows.is_empty() {
                continue;
            }
            let heading = match opts.format {
                crate::OutputFormat::Markdown => format!("## {name}"),
                crate::OutputFormat::Text => name.clone(),
            };
            parts.push(format!("{heading}\n{}", crate::text::render_table(&rows, opts.format)));
        }
        Ok(parts.join("\n\n"))
    })
}

/// PDF text, page by page (pages separated by a blank line).
#[cfg(feature = "pdf")]
pub(crate) fn pdf(bytes: &[u8], opts: &Options) -> Result<String, DocError> {
    guarded("pdf", || {
        // Count pages before the (much more expensive) text extraction.
        let doc = pdf_extract::Document::load_mem(bytes).map_err(|e| DocError::malformed("pdf", e))?;
        let page_count = doc.get_pages().len();
        if page_count > opts.max_pages {
            return Err(DocError::LimitExceeded(format!("{page_count} pages (limit {})", opts.max_pages)));
        }
        drop(doc);
        let pages = pdf_extract::extract_text_from_mem_by_pages(bytes).map_err(|e| DocError::malformed("pdf", e))?;
        let pages: Vec<String> =
            pages.iter().map(|p| crate::text::collapse_blank_lines(p)).filter(|p| !p.is_empty()).collect();
        Ok(pages.join("\n\n"))
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
