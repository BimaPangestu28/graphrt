//! An Excel (XLSX) writer for rows of cells.
//!
//! Text is always written as text (`inlineStr`). A cell that starts with
//! `=`, `+`, `-` or `@` is therefore shown as it is and never calculated, so
//! data an agent copied from an e-mail or a web page cannot become a formula
//! in the reader's spreadsheet. The package has no macros, no external links
//! and no formulas at all.

use crate::DocError;
use crate::package::zip_parts;
use crate::xml::escape;

/// Largest whole number a spreadsheet keeps exactly (2^53).
const MAX_EXACT_INTEGER: f64 = 9_007_199_254_740_992.0;

/// One cell.
#[derive(Clone, Debug, PartialEq)]
pub enum Cell {
    Empty,
    Text(String),
    Number(f64),
    Bool(bool),
}

impl Cell {
    /// A whole number: a number when a spreadsheet keeps it exactly, else its
    /// digits as text (an account number must not lose its last digits).
    pub fn integer(value: i128) -> Self {
        if (value as f64).abs() <= MAX_EXACT_INTEGER {
            Cell::Number(value as f64)
        } else {
            Cell::Text(value.to_string())
        }
    }
}

/// One worksheet.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sheet {
    pub name: String,
    pub rows: Vec<Vec<Cell>>,
    /// Show the first row in bold and keep it in view while scrolling.
    pub header: bool,
}

/// What a workbook may hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XlsxLimits {
    pub max_sheets: usize,
    /// Cells of the whole workbook.
    pub max_cells: usize,
    pub max_columns: usize,
    /// Characters of one cell. Excel itself stops at 32,767.
    pub max_cell_chars: usize,
}

impl Default for XlsxLimits {
    fn default() -> Self {
        Self { max_sheets: 20, max_cells: 200_000, max_columns: 256, max_cell_chars: 32_767 }
    }
}

const CONTENT_TYPES_HEAD: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>"#;

const ROOT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;

/// Two cell formats: 0 plain, 1 bold.
const STYLES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><fonts count="2"><font><sz val="11"/><name val="Calibri"/></font><font><b/><sz val="11"/><name val="Calibri"/></font></fonts><fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills><borders count="1"><border><left/><right/><top/><bottom/><diagonal/></border></borders><cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs><cellXfs count="2"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/><xf numFmtId="0" fontId="1" fillId="0" borderId="0" xfId="0" applyFont="1"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#;

/// `A`, `B`... `Z`, `AA`... for a column counted from zero.
fn column_name(mut index: usize) -> String {
    let mut letters = Vec::new();
    loop {
        letters.push(char::from(b'A' + u8::try_from(index % 26).unwrap_or(0)));
        if index < 26 {
            break;
        }
        index = index / 26 - 1;
    }
    letters.iter().rev().collect()
}

/// Sheet names Excel accepts: at most 31 characters, none of `[]:*?/\`, not
/// empty, no quote at either end, and no two alike.
fn sheet_names(sheets: &[Sheet]) -> Vec<String> {
    let mut names: Vec<String> = Vec::with_capacity(sheets.len());
    for (index, sheet) in sheets.iter().enumerate() {
        let cleaned: String = sheet
            .name
            .chars()
            .map(|c| if matches!(c, '[' | ']' | ':' | '*' | '?' | '/' | '\\') || c.is_control() { ' ' } else { c })
            .collect();
        let cleaned = cleaned.trim().trim_matches('\'').trim();
        let base: String =
            if cleaned.is_empty() { format!("Sheet{}", index + 1) } else { cleaned.chars().take(31).collect() };
        let mut name = base.clone();
        let mut copy = 2;
        while names.iter().any(|taken| taken.eq_ignore_ascii_case(&name)) {
            let suffix = format!(" ({copy})");
            name = format!("{}{suffix}", base.chars().take(31 - suffix.len()).collect::<String>());
            copy += 1;
        }
        names.push(name);
    }
    names
}

fn sheet_xml(sheet: &Sheet, limits: &XlsxLimits) -> String {
    let columns = sheet.rows.iter().map(Vec::len).max().unwrap_or(0);
    // Column widths from the longest text, so a sheet is readable when it opens.
    let mut widths = vec![0usize; columns];
    let mut rows = String::new();
    for (row_index, row) in sheet.rows.iter().enumerate() {
        rows.push_str(&format!(r#"<row r="{}">"#, row_index + 1));
        for (column, cell) in row.iter().enumerate() {
            let reference = format!("{}{}", column_name(column), row_index + 1);
            let style = if sheet.header && row_index == 0 { r#" s="1""# } else { "" };
            let shown = match cell {
                Cell::Empty => 0,
                Cell::Text(text) => {
                    let text: String = text.chars().take(limits.max_cell_chars).collect();
                    rows.push_str(&format!(
                        r#"<c r="{reference}"{style} t="inlineStr"><is><t xml:space="preserve">{}</t></is></c>"#,
                        escape(&text)
                    ));
                    text.lines().map(|line| line.chars().count()).max().unwrap_or(0)
                }
                Cell::Number(number) if number.is_finite() => {
                    let text = number.to_string();
                    rows.push_str(&format!(r#"<c r="{reference}"{style}><v>{text}</v></c>"#));
                    text.len()
                }
                // Not a number a spreadsheet can hold.
                Cell::Number(_) => 0,
                Cell::Bool(value) => {
                    rows.push_str(&format!(r#"<c r="{reference}"{style} t="b"><v>{}</v></c>"#, u8::from(*value)));
                    5
                }
            };
            if let Some(width) = widths.get_mut(column) {
                *width = (*width).max(shown);
            }
        }
        rows.push_str("</row>");
    }
    let cols: String = widths
        .iter()
        .enumerate()
        .map(|(index, width)| {
            format!(r#"<col min="{0}" max="{0}" width="{1}" customWidth="1"/>"#, index + 1, (width + 2).clamp(8, 60))
        })
        .collect();
    let cols = if cols.is_empty() { cols } else { format!("<cols>{cols}</cols>") };
    let view = if sheet.header && sheet.rows.len() > 1 {
        r#"<sheetViews><sheetView workbookViewId="0"><pane ySplit="1" topLeftCell="A2" activePane="bottomLeft" state="frozen"/></sheetView></sheetViews>"#
    } else {
        ""
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">{view}{cols}<sheetData>{rows}</sheetData></worksheet>"#
    )
}

/// Write `sheets` as a workbook.
///
/// @throws DocError - No sheet, or more sheets, columns or cells than `limits` allow.
pub fn render(sheets: &[Sheet], limits: &XlsxLimits) -> Result<Vec<u8>, DocError> {
    if sheets.is_empty() {
        return Err(DocError::Empty("a workbook needs at least one sheet"));
    }
    if sheets.len() > limits.max_sheets {
        return Err(DocError::TooLong { what: "sheets", limit: limits.max_sheets });
    }
    let cells: usize = sheets.iter().flat_map(|sheet| sheet.rows.iter()).map(Vec::len).sum();
    if cells > limits.max_cells {
        return Err(DocError::TooLong { what: "cells", limit: limits.max_cells });
    }
    if sheets.iter().flat_map(|sheet| sheet.rows.iter()).any(|row| row.len() > limits.max_columns) {
        return Err(DocError::TooLong { what: "columns", limit: limits.max_columns });
    }
    let names = sheet_names(sheets);
    let mut content_types = CONTENT_TYPES_HEAD.to_owned();
    let mut workbook = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets>"#,
    );
    let mut rels = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
    );
    let mut sheet_parts: Vec<(String, String)> = Vec::with_capacity(sheets.len());
    for (index, (sheet, name)) in sheets.iter().zip(&names).enumerate() {
        let number = index + 1;
        content_types.push_str(&format!(
            r#"<Override PartName="/xl/worksheets/sheet{number}.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>"#
        ));
        workbook.push_str(&format!(r#"<sheet name="{}" sheetId="{number}" r:id="rId{number}"/>"#, escape(name)));
        rels.push_str(&format!(
            r#"<Relationship Id="rId{number}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet{number}.xml"/>"#
        ));
        sheet_parts.push((format!("xl/worksheets/sheet{number}.xml"), sheet_xml(sheet, limits)));
    }
    content_types.push_str("</Types>");
    workbook.push_str("</sheets></workbook>");
    rels.push_str(&format!(
        r#"<Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#,
        sheets.len() + 1
    ));
    let mut parts: Vec<(&str, String)> = vec![
        ("[Content_Types].xml", content_types),
        ("_rels/.rels", ROOT_RELS.to_owned()),
        ("xl/workbook.xml", workbook),
        ("xl/_rels/workbook.xml.rels", rels),
        ("xl/styles.xml", STYLES.to_owned()),
    ];
    parts.extend(sheet_parts.iter().map(|(name, xml)| (name.as_str(), xml.clone())));
    zip_parts(&parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_names_count_like_a_spreadsheet() {
        let names: Vec<String> = [0, 1, 25, 26, 27, 51, 52, 701, 702, 16_383].into_iter().map(column_name).collect();
        assert_eq!(names, ["A", "B", "Z", "AA", "AB", "AZ", "BA", "ZZ", "AAA", "XFD"]);
    }

    #[test]
    fn sheet_names_are_made_valid_and_distinct() {
        let sheet = |name: &str| Sheet { name: name.to_owned(), ..Sheet::default() };
        let names = sheet_names(&[
            sheet("Q3 [draft]: a/b*c?"),
            sheet(""),
            sheet("'quoted'"),
            sheet("data"),
            sheet("DATA"),
            sheet(&"x".repeat(40)),
            sheet(&"x".repeat(40)),
        ]);
        assert_eq!(names[0], "Q3  draft   a b c");
        assert_eq!(names[1], "Sheet2");
        assert_eq!(names[2], "quoted");
        assert_eq!((names[3].as_str(), names[4].as_str()), ("data", "DATA (2)"));
        assert_eq!(names[5].chars().count(), 31);
        assert_eq!(names[6].chars().count(), 31);
        assert!(names[6].ends_with(" (2)"));
    }

    #[test]
    fn long_whole_numbers_stay_exact_as_text() {
        assert_eq!(Cell::integer(42), Cell::Number(42.0));
        assert_eq!(Cell::integer(-9_007_199_254_740_992), Cell::Number(-9_007_199_254_740_992.0));
        assert_eq!(Cell::integer(1_234_567_890_123_456_789), Cell::Text("1234567890123456789".into()));
    }

    #[test]
    fn a_workbook_stops_at_its_limits() {
        let limits = XlsxLimits { max_sheets: 2, max_cells: 10, max_columns: 4, ..XlsxLimits::default() };
        let rows = |rows: usize, columns: usize| Sheet {
            rows: vec![vec![Cell::Number(1.0); columns]; rows],
            ..Sheet::default()
        };
        assert_eq!(render(&[], &limits), Err(DocError::Empty("a workbook needs at least one sheet")));
        assert_eq!(
            render(&[rows(1, 1), rows(1, 1), rows(1, 1)], &limits),
            Err(DocError::TooLong { what: "sheets", limit: 2 })
        );
        assert_eq!(render(&[rows(3, 4)], &limits), Err(DocError::TooLong { what: "cells", limit: 10 }));
        assert_eq!(render(&[rows(1, 5)], &limits), Err(DocError::TooLong { what: "columns", limit: 4 }));
        assert!(render(&[rows(2, 4), rows(0, 0)], &limits).is_ok());
    }
}
