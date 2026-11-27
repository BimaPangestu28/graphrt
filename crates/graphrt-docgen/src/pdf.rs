//! A PDF writer for text documents.
//!
//! It lays out the blocks of [`crate::markdown`] on A4 pages with the fonts
//! every PDF reader has built in (Helvetica and Courier), so no font file is
//! read, embedded or shipped. The price is the character set: Western
//! European text (Windows-1252). Characters outside it are written as `?`
//! and counted, so the caller can say so.
//!
//! The file holds text and lines only: no scripts, no links, no forms, no
//! attachments and nothing fetched from anywhere.

use std::io::Write as _;

use crate::DocError;
use crate::markdown::{Block, Span};

/// A4 in points.
const PAGE_WIDTH: f32 = 595.28;
const PAGE_HEIGHT: f32 = 841.89;
const MARGIN: f32 = 56.0;
const CONTENT_WIDTH: f32 = PAGE_WIDTH - 2.0 * MARGIN;
/// Lowest baseline of body content (the page number sits below it).
const BOTTOM: f32 = MARGIN + 14.0;

const BODY_SIZE: f32 = 11.0;
const BODY_LEADING: f32 = 15.0;
const CODE_SIZE: f32 = 9.5;
const CODE_LEADING: f32 = 12.5;
const TABLE_SIZE: f32 = 10.0;
const TABLE_LEADING: f32 = 13.0;
const CELL_PADDING: f32 = 4.0;
const LIST_INDENT: f32 = 18.0;
/// Most columns of a table. More would leave no room for a word.
pub const MAX_TABLE_COLUMNS: usize = 12;

/// What a PDF is made with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PdfOptions {
    /// Title in the document properties.
    pub title: Option<String>,
    /// Refuse a document that needs more pages.
    pub max_pages: usize,
}

impl Default for PdfOptions {
    fn default() -> Self {
        Self { title: None, max_pages: 200 }
    }
}

/// A finished PDF.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pdf {
    pub bytes: Vec<u8>,
    pub pages: usize,
    /// Characters that the built in fonts cannot show and that became `?`.
    pub replaced_chars: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Font {
    Regular,
    Bold,
    Italic,
    BoldItalic,
    Mono,
}

impl Font {
    fn of(span: &Span) -> Self {
        match (span.code, span.bold, span.italic) {
            (true, _, _) => Font::Mono,
            (false, true, true) => Font::BoldItalic,
            (false, true, false) => Font::Bold,
            (false, false, true) => Font::Italic,
            (false, false, false) => Font::Regular,
        }
    }

    /// Resource name in the page (`/F1`...).
    fn resource(self) -> &'static str {
        match self {
            Font::Regular => "F1",
            Font::Bold => "F2",
            Font::Italic => "F3",
            Font::BoldItalic => "F4",
            Font::Mono => "F5",
        }
    }

    fn bold(self) -> bool {
        matches!(self, Font::Bold | Font::BoldItalic)
    }
}

/// Widths of Helvetica for the bytes 32 to 126, in thousandths of the font size.
const HELVETICA: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556, 556, 556, 556, 556,
    556, 556, 556, 278, 278, 584, 584, 584, 556, 1015, 667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833,
    722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 278, 278, 278, 469, 556, 333, 556, 556, 500, 556,
    556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556, 556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334,
    260, 334, 584,
];

/// Widths of Helvetica Bold for the bytes 32 to 126.
const HELVETICA_BOLD: [u16; 95] = [
    278, 333, 474, 556, 556, 889, 722, 238, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556, 556, 556, 556, 556,
    556, 556, 556, 333, 333, 584, 584, 584, 611, 975, 722, 722, 722, 722, 667, 611, 778, 722, 278, 556, 722, 611, 833,
    722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 333, 278, 333, 584, 556, 333, 556, 611, 556, 611,
    556, 333, 611, 611, 278, 278, 556, 278, 889, 611, 611, 611, 611, 389, 556, 333, 611, 556, 778, 556, 556, 500, 389,
    280, 389, 584,
];

/// Width of one Windows-1252 byte in thousandths of the font size. Bytes above
/// 126 get the width of the widest glyph of their kind, so a line is wrapped a
/// little early at worst and never runs over the margin.
fn glyph_width(byte: u8, font: Font) -> u16 {
    if font == Font::Mono {
        return 600;
    }
    let table = if font.bold() { &HELVETICA_BOLD } else { &HELVETICA };
    match byte {
        32..=126 => table.get(usize::from(byte - 32)).copied().unwrap_or(600),
        0xa0 => 278,
        0x91 | 0x92 => 278,
        0x93..=0x95 => 500,
        0x96 => 556,
        0xc0..=0xde => 722,
        0xdf..=0xff => 611,
        _ => 1000,
    }
}

fn text_width(bytes: &[u8], font: Font, size: f32) -> f32 {
    let units: u32 = bytes.iter().map(|byte| u32::from(glyph_width(*byte, font))).sum();
    units as f32 * size / 1000.0
}

/// The Windows-1252 byte of a character, when it has one.
fn encode_char(c: char) -> Option<u8> {
    let code = u32::from(c);
    match c {
        ' '..='~' => u8::try_from(code).ok(),
        '\u{a0}'..='\u{ff}' => u8::try_from(code).ok(),
        '\t' => Some(b' '),
        '\u{20ac}' => Some(0x80),
        '\u{201a}' => Some(0x82),
        '\u{2026}' => Some(0x85),
        '\u{2018}' => Some(0x91),
        '\u{2019}' => Some(0x92),
        '\u{201c}' => Some(0x93),
        '\u{201d}' => Some(0x94),
        '\u{2022}' => Some(0x95),
        '\u{2013}' => Some(0x96),
        '\u{2014}' => Some(0x97),
        '\u{2122}' => Some(0x99),
        // Spaces and hyphens that only differ in width.
        '\u{2002}'..='\u{200a}' | '\u{202f}' => Some(b' '),
        '\u{2010}' | '\u{2011}' | '\u{2212}' => Some(b'-'),
        _ => None,
    }
}

/// A word in one font, already encoded.
#[derive(Clone, Debug)]
struct Word {
    bytes: Vec<u8>,
    font: Font,
    /// Whether a space separates it from the word before.
    space_before: bool,
}

/// One laid out line: runs of bytes per font.
type Line = Vec<(Font, Vec<u8>)>;

struct Layout {
    /// Finished pages.
    done: Vec<String>,
    /// The page being filled.
    current: String,
    /// Baseline of the next line, from the bottom of the page.
    y: f32,
    replaced_chars: usize,
    max_pages: usize,
}

impl Layout {
    fn new(max_pages: usize) -> Self {
        Self { done: Vec::new(), current: String::new(), y: PAGE_HEIGHT - MARGIN, replaced_chars: 0, max_pages }
    }

    fn page(&mut self) -> &mut String {
        &mut self.current
    }

    fn new_page(&mut self) -> Result<(), DocError> {
        if self.done.len() + 1 >= self.max_pages {
            return Err(DocError::TooLong { what: "pages", limit: self.max_pages });
        }
        self.done.push(std::mem::take(&mut self.current));
        self.y = PAGE_HEIGHT - MARGIN;
        Ok(())
    }

    /// Make room for `height` points, on a new page when this one is full.
    fn need(&mut self, height: f32) -> Result<(), DocError> {
        if self.y - height < BOTTOM {
            self.new_page()?;
        }
        Ok(())
    }

    fn encode(&mut self, text: &str) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(text.len());
        for c in text.chars() {
            if c.is_control() && c != '\t' {
                continue;
            }
            match encode_char(c) {
                Some(byte) => bytes.push(byte),
                None => {
                    self.replaced_chars += 1;
                    bytes.push(b'?');
                }
            }
        }
        bytes
    }

    fn words(&mut self, spans: &[Span]) -> Vec<Word> {
        let mut words = Vec::new();
        let mut pending_space = false;
        for span in spans {
            let font = Font::of(span);
            let mut first = true;
            for piece in span.text.split(' ') {
                // Every split boundary was a space.
                if !first {
                    pending_space = true;
                }
                first = false;
                if piece.is_empty() {
                    continue;
                }
                let bytes = self.encode(piece);
                if bytes.is_empty() {
                    continue;
                }
                words.push(Word { bytes, font, space_before: pending_space && !words.is_empty() });
                pending_space = false;
            }
        }
        words
    }

    /// Break words into lines no wider than `width`. A word wider than a line is cut.
    fn wrap(words: Vec<Word>, width: f32, size: f32) -> Vec<Line> {
        let mut lines: Vec<Line> = Vec::new();
        let mut line: Line = Vec::new();
        let mut used = 0.0f32;
        let push = |line: &mut Line, font: Font, bytes: &[u8]| match line.last_mut() {
            Some((last, run)) if *last == font => run.extend_from_slice(bytes),
            _ => line.push((font, bytes.to_vec())),
        };
        for word in words {
            let space = if word.space_before && !line.is_empty() { text_width(b" ", word.font, size) } else { 0.0 };
            let word_width = text_width(&word.bytes, word.font, size);
            if !line.is_empty() && used + space + word_width > width {
                lines.push(std::mem::take(&mut line));
                used = 0.0;
            } else if space > 0.0 {
                push(&mut line, word.font, b" ");
                used += space;
            }
            if word_width <= width {
                push(&mut line, word.font, &word.bytes);
                used += word_width;
                continue;
            }
            // Longer than a whole line: cut it wherever the line ends.
            for byte in word.bytes {
                let glyph = text_width(&[byte], word.font, size);
                if used + glyph > width && !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                    used = 0.0;
                }
                push(&mut line, word.font, &[byte]);
                used += glyph;
            }
        }
        if !line.is_empty() {
            lines.push(line);
        }
        lines
    }

    fn draw_line(&mut self, line: &Line, x: f32, y: f32, size: f32) {
        let mut ops = format!("BT {x:.2} {y:.2} Td ");
        for (font, bytes) in line {
            ops.push_str(&format!("/{} {size} Tf (", font.resource()));
            push_string(&mut ops, bytes);
            ops.push_str(") Tj ");
        }
        ops.push_str("ET\n");
        self.page().push_str(&ops);
    }

    /// Wrapped text starting `indent` points from the left margin.
    fn text(&mut self, spans: &[Span], indent: f32, size: f32, leading: f32) -> Result<(), DocError> {
        let words = self.words(spans);
        for line in Self::wrap(words, CONTENT_WIDTH - indent, size) {
            self.need(leading)?;
            self.y -= leading;
            let y = self.y;
            self.draw_line(&line, MARGIN + indent, y, size);
        }
        Ok(())
    }

    fn rule(&mut self, y: f32) {
        let right = PAGE_WIDTH - MARGIN;
        self.page().push_str(&format!("0.6 G 0.5 w {MARGIN:.2} {y:.2} m {right:.2} {y:.2} l S 0 G\n"));
    }

    fn block(&mut self, block: &Block) -> Result<(), DocError> {
        match block {
            Block::Heading { level, spans } => {
                let (size, leading) = match level {
                    1 => (20.0, 26.0),
                    2 => (16.0, 22.0),
                    _ => (13.0, 18.0),
                };
                let bold: Vec<Span> = spans.iter().map(|span| Span { bold: true, ..span.clone() }).collect();
                // A heading stays with at least two lines of what follows.
                self.need(leading + 2.0 * BODY_LEADING + 6.0)?;
                self.y -= 6.0;
                self.text(&bold, 0.0, size, leading)?;
                self.y -= 4.0;
            }
            Block::Paragraph(spans) => {
                self.text(spans, 0.0, BODY_SIZE, BODY_LEADING)?;
                self.y -= 7.0;
            }
            Block::List { ordered, items } => {
                for (index, item) in items.iter().enumerate() {
                    self.need(BODY_LEADING)?;
                    let marker = if *ordered { format!("{}.", index + 1) } else { "\u{2022}".to_owned() };
                    let marker = self.encode(&marker);
                    let y = self.y - BODY_LEADING;
                    self.draw_line(&vec![(Font::Regular, marker)], MARGIN + 4.0, y, BODY_SIZE);
                    if item.is_empty() {
                        self.y -= BODY_LEADING;
                    }
                    self.text(item, LIST_INDENT + if *ordered { 6.0 } else { 0.0 }, BODY_SIZE, BODY_LEADING)?;
                    self.y -= 2.0;
                }
                self.y -= 5.0;
            }
            Block::Code(lines) => {
                for line in lines {
                    let span = Span { text: line.replace('\t', "    "), code: true, ..Span::default() };
                    // Leading spaces are layout in code: keep them as an indent.
                    let indent = line.chars().take_while(|c| *c == ' ').count() as f32 * 0.6 * CODE_SIZE;
                    if line.trim().is_empty() {
                        self.need(CODE_LEADING)?;
                        self.y -= CODE_LEADING;
                    } else {
                        self.text(&[span], 8.0 + indent.min(CONTENT_WIDTH / 2.0), CODE_SIZE, CODE_LEADING)?;
                    }
                }
                self.y -= 7.0;
            }
            Block::Table { header, rows } => self.table(*header, rows)?,
            Block::Rule => {
                self.need(14.0)?;
                self.y -= 8.0;
                let y = self.y;
                self.rule(y);
                self.y -= 6.0;
            }
            Block::PageBreak => self.new_page()?,
        }
        Ok(())
    }

    fn table(&mut self, header: bool, rows: &[Vec<String>]) -> Result<(), DocError> {
        let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return Ok(());
        }
        if columns > MAX_TABLE_COLUMNS {
            return Err(DocError::TooLong { what: "table columns", limit: MAX_TABLE_COLUMNS });
        }
        let column_width = CONTENT_WIDTH / columns as f32;
        // A row never grows past a page: the rest of an endless cell is cut.
        let max_lines = ((PAGE_HEIGHT - MARGIN - BOTTOM - 2.0 * CELL_PADDING) / TABLE_LEADING) as usize;
        self.y -= 4.0;
        for (index, row) in rows.iter().enumerate() {
            let bold = header && index == 0;
            let mut cells: Vec<Vec<Line>> = Vec::with_capacity(columns);
            for column in 0..columns {
                let text = row.get(column).map(String::as_str).unwrap_or_default();
                let words = self.words(&[Span { text: text.to_owned(), bold, ..Span::default() }]);
                let mut lines = Self::wrap(words, column_width - 2.0 * CELL_PADDING, TABLE_SIZE);
                lines.truncate(max_lines.max(1));
                cells.push(lines);
            }
            let line_count = cells.iter().map(Vec::len).max().unwrap_or(1).max(1);
            let height = line_count as f32 * TABLE_LEADING + 2.0 * CELL_PADDING;
            self.need(height)?;
            let top = self.y;
            let bottom = top - height;
            let mut ops = String::from("0.6 G 0.5 w ");
            ops.push_str(&format!("{MARGIN:.2} {bottom:.2} {CONTENT_WIDTH:.2} {height:.2} re S "));
            for column in 1..columns {
                let x = MARGIN + column as f32 * column_width;
                ops.push_str(&format!("{x:.2} {bottom:.2} m {x:.2} {top:.2} l S "));
            }
            ops.push_str("0 G\n");
            self.page().push_str(&ops);
            for (column, lines) in cells.iter().enumerate() {
                let x = MARGIN + column as f32 * column_width + CELL_PADDING;
                for (number, line) in lines.iter().enumerate() {
                    let y = top - CELL_PADDING - (number as f32 + 1.0) * TABLE_LEADING + 3.0;
                    self.draw_line(line, x, y, TABLE_SIZE);
                }
            }
            self.y = bottom;
        }
        self.y -= 9.0;
        Ok(())
    }
}

/// Append `bytes` as the inside of a PDF literal string: the three characters
/// with a meaning are escaped and everything outside ASCII is written in octal,
/// so no text can end the string or start an operator.
fn push_string(out: &mut String, bytes: &[u8]) {
    for byte in bytes {
        match byte {
            b'(' | b')' | b'\\' => {
                out.push('\\');
                out.push(char::from(*byte));
            }
            32..=126 => out.push(char::from(*byte)),
            other => out.push_str(&format!("\\{other:03o}")),
        }
    }
}

/// A text string for the document properties: ASCII as a literal, anything
/// else as UTF-16 in hex.
fn info_string(text: &str) -> String {
    let clean: String = text.chars().filter(|c| !c.is_control()).take(200).collect();
    if clean.is_ascii() {
        let mut out = String::from("(");
        push_string(&mut out, clean.as_bytes());
        out.push(')');
        return out;
    }
    let mut out = String::from("<FEFF");
    for unit in clean.encode_utf16() {
        out.push_str(&format!("{unit:04X}"));
    }
    out.push('>');
    out
}

fn deflate(data: &[u8]) -> Result<Vec<u8>, DocError> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).map_err(|e| DocError::Write(e.to_string()))?;
    encoder.finish().map_err(|e| DocError::Write(e.to_string()))
}

/// Lay out `blocks` and write the PDF.
///
/// @throws DocError - More pages or table columns than allowed.
pub fn render(blocks: &[Block], options: &PdfOptions) -> Result<Pdf, DocError> {
    let mut layout = Layout::new(options.max_pages.max(1));
    for block in blocks {
        layout.block(block)?;
    }
    let mut pages = std::mem::take(&mut layout.done);
    // A break at the very end leaves an empty last page: drop it.
    if pages.is_empty() || !layout.current.is_empty() {
        pages.push(std::mem::take(&mut layout.current));
    }
    let page_count = pages.len();
    let mut objects: Vec<Vec<u8>> = Vec::new();
    // 1: catalog, 2: page tree, 3 to 7: fonts, 8: properties, then two objects per page.
    objects.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
    let kids: Vec<String> = (0..page_count).map(|page| format!("{} 0 R", 9 + 2 * page)).collect();
    objects.push(format!("<< /Type /Pages /Count {page_count} /Kids [{}] >>", kids.join(" ")).into_bytes());
    for base in ["Helvetica", "Helvetica-Bold", "Helvetica-Oblique", "Helvetica-BoldOblique", "Courier"] {
        objects.push(
            format!("<< /Type /Font /Subtype /Type1 /BaseFont /{base} /Encoding /WinAnsiEncoding >>").into_bytes(),
        );
    }
    let title = options.title.as_deref().map(|title| format!(" /Title {}", info_string(title))).unwrap_or_default();
    objects.push(format!("<< /Producer (Graphrt){title} >>").into_bytes());
    for (index, content) in pages.iter().enumerate() {
        let label = format!("{} / {page_count}", index + 1);
        let x = (PAGE_WIDTH - text_width(label.as_bytes(), Font::Regular, 9.0)) / 2.0;
        let footer = format!("BT 0.45 g {x:.2} {:.2} Td /F1 9 Tf ({label}) Tj 0 g ET\n", MARGIN - 18.0);
        let stream = deflate(format!("{content}{footer}").as_bytes())?;
        objects.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {PAGE_WIDTH} {PAGE_HEIGHT}] /Contents {} 0 R \
                 /Resources << /Font << /F1 3 0 R /F2 4 0 R /F3 5 0 R /F4 6 0 R /F5 7 0 R >> >> >>",
                10 + 2 * index
            )
            .into_bytes(),
        );
        let mut object = format!("<< /Length {} /Filter /FlateDecode >>\nstream\n", stream.len()).into_bytes();
        object.extend_from_slice(&stream);
        object.extend_from_slice(b"\nendstream");
        objects.push(object);
    }

    let mut bytes = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (index, object) in objects.iter().enumerate() {
        offsets.push(bytes.len());
        bytes.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        bytes.extend_from_slice(object);
        bytes.extend_from_slice(b"\nendobj\n");
    }
    let xref = bytes.len();
    bytes.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
    for offset in offsets {
        bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    bytes.extend_from_slice(
        format!("trailer\n<< /Size {} /Root 1 0 R /Info 8 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1)
            .as_bytes(),
    );
    Ok(Pdf { bytes, pages: page_count, replaced_chars: layout.replaced_chars })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::parse;

    #[test]
    fn text_cannot_end_a_string_or_start_an_operator() {
        let mut out = String::new();
        push_string(&mut out, b"a) Tj ET /JS (app.alert(1)) \\ (");
        assert_eq!(out, "a\\) Tj ET /JS \\(app.alert\\(1\\)\\) \\\\ \\(");
        let mut out = String::new();
        push_string(&mut out, &[b'a', b'\n', 0xe9, 0x00]);
        assert_eq!(out, "a\\012\\351\\000");
        assert_eq!(info_string("Q3 (draft)"), "(Q3 \\(draft\\))");
        assert_eq!(info_string("Caf\u{e9}"), "<FEFF00430061006600E9>");
    }

    #[test]
    fn characters_outside_the_fonts_are_counted_not_dropped() {
        let pdf =
            render(&parse("Total: 5 \u{20ac} \u{2013} \u{6f22}\u{5b57} \u{1f600}"), &PdfOptions::default()).unwrap();
        assert_eq!(pdf.replaced_chars, 3);
        assert_eq!(pdf.pages, 1);
        assert!(pdf.bytes.starts_with(b"%PDF-1.4"));
        assert!(pdf.bytes.ends_with(b"%%EOF\n"));
    }

    #[test]
    fn lines_never_run_past_the_margin() {
        let mut layout = Layout::new(10);
        let long = "word ".repeat(400) + &"x".repeat(900);
        let words = layout.words(&[Span::plain(long)]);
        let lines = Layout::wrap(words, CONTENT_WIDTH, BODY_SIZE);
        assert!(lines.len() > 20);
        for line in lines {
            let width: f32 = line.iter().map(|(font, bytes)| text_width(bytes, *font, BODY_SIZE)).sum();
            assert!(width <= CONTENT_WIDTH + 0.01, "{width}");
        }
    }

    #[test]
    fn a_document_stops_at_the_page_limit() {
        let endless = "A paragraph that fills a line or two of the page.\n\n".repeat(2_000);
        let refused = render(&parse(&endless), &PdfOptions { max_pages: 5, ..PdfOptions::default() });
        assert_eq!(refused, Err(DocError::TooLong { what: "pages", limit: 5 }));
        let fits = render(&parse(&endless), &PdfOptions { max_pages: 500, ..PdfOptions::default() }).unwrap();
        assert!(fits.pages > 5 && fits.pages < 500);
        let wide = format!("|{}", "a|".repeat(13));
        assert_eq!(
            render(&parse(&wide), &PdfOptions::default()),
            Err(DocError::TooLong { what: "table columns", limit: MAX_TABLE_COLUMNS })
        );
    }
}
