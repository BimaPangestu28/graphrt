//! A Word (DOCX) writer for the blocks of [`crate::markdown`].
//!
//! The package holds four parts written here: content types, relationships,
//! styles and the document. It has no macros, no external relationships, no
//! embedded objects and no fields, so opening it runs and fetches nothing.

use crate::DocError;
use crate::markdown::{Block, Span};
use crate::package::zip_parts;
use crate::xml::escape;

const CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/><Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/></Types>"#;

const ROOT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/></Relationships>"#;

const DOCUMENT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#;

const STYLES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii="Arial" w:hAnsi="Arial" w:cs="Arial"/><w:sz w:val="22"/><w:szCs w:val="22"/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="140" w:line="276" w:lineRule="auto"/></w:pPr></w:pPrDefault></w:docDefaults><w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/></w:style><w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:pPr><w:keepNext/><w:spacing w:before="320" w:after="120"/><w:outlineLvl w:val="0"/></w:pPr><w:rPr><w:b/><w:sz w:val="40"/><w:szCs w:val="40"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="Heading2"><w:name w:val="heading 2"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:pPr><w:keepNext/><w:spacing w:before="260" w:after="100"/><w:outlineLvl w:val="1"/></w:pPr><w:rPr><w:b/><w:sz w:val="32"/><w:szCs w:val="32"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="Heading3"><w:name w:val="heading 3"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:pPr><w:keepNext/><w:spacing w:before="200" w:after="80"/><w:outlineLvl w:val="2"/></w:pPr><w:rPr><w:b/><w:sz w:val="26"/><w:szCs w:val="26"/></w:rPr></w:style></w:styles>"#;

/// What a Word file is made with.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DocxOptions {
    /// Title in the document properties.
    pub title: Option<String>,
}

fn run(span: &Span) -> String {
    let mut properties = String::new();
    if span.code {
        properties.push_str(r#"<w:rFonts w:ascii="Courier New" w:hAnsi="Courier New" w:cs="Courier New"/>"#);
    }
    if span.bold {
        properties.push_str("<w:b/>");
    }
    if span.italic {
        properties.push_str("<w:i/>");
    }
    let properties = if properties.is_empty() { properties } else { format!("<w:rPr>{properties}</w:rPr>") };
    format!(r#"<w:r>{properties}<w:t xml:space="preserve">{}</w:t></w:r>"#, escape(&span.text))
}

fn paragraph(properties: &str, spans: &[Span]) -> String {
    let runs: String = spans.iter().map(run).collect();
    let properties = if properties.is_empty() { String::new() } else { format!("<w:pPr>{properties}</w:pPr>") };
    format!("<w:p>{properties}{runs}</w:p>")
}

fn table(header: bool, rows: &[Vec<String>]) -> String {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let border = |side: &str| format!(r#"<w:{side} w:val="single" w:sz="4" w:space="0" w:color="999999"/>"#);
    let borders: String =
        ["top", "left", "bottom", "right", "insideH", "insideV"].iter().map(|side| border(side)).collect();
    let mut xml = format!(
        r#"<w:tbl><w:tblPr><w:tblW w:w="5000" w:type="pct"/><w:tblBorders>{borders}</w:tblBorders><w:tblCellMar><w:left w:w="80" w:type="dxa"/><w:right w:w="80" w:type="dxa"/></w:tblCellMar></w:tblPr>"#
    );
    for (index, row) in rows.iter().enumerate() {
        xml.push_str("<w:tr>");
        for column in 0..columns {
            let text = row.get(column).map(String::as_str).unwrap_or_default();
            let span = Span { text: text.to_owned(), bold: header && index == 0, ..Span::default() };
            xml.push_str(&format!("<w:tc>{}</w:tc>", paragraph(r#"<w:spacing w:after="0"/>"#, &[span])));
        }
        xml.push_str("</w:tr>");
    }
    xml.push_str("</w:tbl><w:p/>");
    xml
}

fn body(blocks: &[Block]) -> String {
    let mut xml = String::new();
    for block in blocks {
        match block {
            Block::Heading { level, spans } => {
                xml.push_str(&paragraph(&format!(r#"<w:pStyle w:val="Heading{}"/>"#, (*level).clamp(1, 3)), spans));
            }
            Block::Paragraph(spans) => xml.push_str(&paragraph("", spans)),
            Block::List { ordered, items } => {
                for (index, item) in items.iter().enumerate() {
                    let marker = if *ordered { format!("{}.\t", index + 1) } else { "\u{2022}\t".to_owned() };
                    let mut spans = vec![Span::plain(marker)];
                    spans.extend(item.iter().cloned());
                    let indent = r#"<w:spacing w:after="60"/><w:ind w:left="720" w:hanging="360"/>"#;
                    xml.push_str(&paragraph(indent, &spans));
                }
            }
            Block::Table { header, rows } if !rows.is_empty() => xml.push_str(&table(*header, rows)),
            Block::Table { .. } => {}
            Block::Code(lines) => {
                for line in lines {
                    let span = Span { text: line.replace('\t', "    "), code: true, ..Span::default() };
                    xml.push_str(&paragraph(r#"<w:spacing w:after="0"/><w:ind w:left="240"/>"#, &[span]));
                }
                xml.push_str("<w:p/>");
            }
            Block::Rule => xml.push_str(
                r#"<w:p><w:pPr><w:pBdr><w:bottom w:val="single" w:sz="6" w:space="1" w:color="999999"/></w:pBdr></w:pPr></w:p>"#,
            ),
            Block::PageBreak => xml.push_str(r#"<w:p><w:r><w:br w:type="page"/></w:r></w:p>"#),
        }
    }
    xml
}

/// Write `blocks` as a Word document (A4, one section).
///
/// @throws DocError - When the package cannot be written.
pub fn render(blocks: &[Block], options: &DocxOptions) -> Result<Vec<u8>, DocError> {
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{}<w:sectPr><w:pgSz w:w="11906" w:h="16838"/><w:pgMar w:top="1134" w:right="1134" w:bottom="1134" w:left="1134" w:header="708" w:footer="708" w:gutter="0"/></w:sectPr></w:body></w:document>"#,
        body(blocks)
    );
    let title: String = options.title.as_deref().unwrap_or_default().chars().take(200).collect();
    let core = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>{}</dc:title><dc:creator>Graphrt</dc:creator></cp:coreProperties>"#,
        escape(&title)
    );
    zip_parts(&[
        ("[Content_Types].xml", CONTENT_TYPES.to_owned()),
        ("_rels/.rels", ROOT_RELS.to_owned()),
        ("docProps/core.xml", core),
        ("word/_rels/document.xml.rels", DOCUMENT_RELS.to_owned()),
        ("word/styles.xml", STYLES.to_owned()),
        ("word/document.xml", document),
    ])
}
