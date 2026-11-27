//! The document model and the Markdown subset it is read from.
//!
//! Models write Markdown without being asked, so documents are described in
//! it: headings, paragraphs, bullet and numbered lists, tables, code blocks,
//! rules and bold, italic and inline code. Whatever is not understood stays
//! literal text. Nothing here is executed, fetched or included: an image or a
//! link is written as its text.

/// A run of text in one style.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    /// Inline code: a fixed width font.
    pub code: bool,
}

impl Span {
    /// Plain text.
    pub fn plain(text: impl Into<String>) -> Self {
        Self { text: text.into(), ..Self::default() }
    }
}

/// One block of a document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    /// A heading of level 1 to 3.
    Heading {
        level: u8,
        spans: Vec<Span>,
    },
    Paragraph(Vec<Span>),
    List {
        ordered: bool,
        items: Vec<Vec<Span>>,
    },
    /// Rows of cells. With `header`, the first row is the header.
    Table {
        header: bool,
        rows: Vec<Vec<String>>,
    },
    /// Lines of a code block, shown as they are.
    Code(Vec<String>),
    /// A horizontal line.
    Rule,
    /// What follows starts on a new page (a line that is exactly `\newpage`).
    PageBreak,
}

/// The plain text of some spans.
pub fn plain_text(spans: &[Span]) -> String {
    spans.iter().map(|span| span.text.as_str()).collect()
}

/// Read a document from Markdown.
pub fn parse(markdown: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut paragraph: Vec<&str> = Vec::new();
    let mut lines = markdown.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if let Some(fence) = fence_of(trimmed) {
            flush_paragraph(&mut paragraph, &mut blocks);
            let mut code = Vec::new();
            for inner in lines.by_ref() {
                if inner.trim().starts_with(fence) {
                    break;
                }
                code.push(inner.to_owned());
            }
            blocks.push(Block::Code(code));
        } else if trimmed.is_empty() {
            flush_paragraph(&mut paragraph, &mut blocks);
        } else if trimmed == "\\newpage" {
            flush_paragraph(&mut paragraph, &mut blocks);
            blocks.push(Block::PageBreak);
        } else if let Some((level, text)) = heading_of(trimmed) {
            flush_paragraph(&mut paragraph, &mut blocks);
            blocks.push(Block::Heading { level, spans: inline(text) });
        } else if is_rule(trimmed) {
            flush_paragraph(&mut paragraph, &mut blocks);
            blocks.push(Block::Rule);
        } else if trimmed.starts_with('|') {
            flush_paragraph(&mut paragraph, &mut blocks);
            let mut rows = vec![trimmed];
            while let Some(next) = lines.peek().map(|next| next.trim()).filter(|next| next.starts_with('|')) {
                rows.push(next);
                lines.next();
            }
            blocks.push(table_of(&rows));
        } else if let Some((ordered, first)) = list_item_of(trimmed) {
            flush_paragraph(&mut paragraph, &mut blocks);
            let mut items = vec![first.to_owned()];
            while let Some(next) = lines.peek().copied() {
                let next_trimmed = next.trim();
                if let Some((_, item)) = list_item_of(next_trimmed) {
                    items.push(item.to_owned());
                } else if next.starts_with([' ', '\t']) && !next_trimmed.is_empty() {
                    // An indented line continues the item above it.
                    if let Some(last) = items.last_mut() {
                        last.push(' ');
                        last.push_str(next_trimmed);
                    }
                } else {
                    break;
                }
                lines.next();
            }
            blocks.push(Block::List { ordered, items: items.iter().map(|item| inline(item)).collect() });
        } else {
            paragraph.push(trimmed.strip_prefix("> ").unwrap_or(trimmed));
        }
    }
    flush_paragraph(&mut paragraph, &mut blocks);
    blocks
}

fn flush_paragraph(paragraph: &mut Vec<&str>, blocks: &mut Vec<Block>) {
    if !paragraph.is_empty() {
        blocks.push(Block::Paragraph(inline(&paragraph.join(" "))));
        paragraph.clear();
    }
}

fn fence_of(line: &str) -> Option<&'static str> {
    ["```", "~~~"].into_iter().find(|fence| line.starts_with(fence))
}

fn heading_of(line: &str) -> Option<(u8, &str)> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    let text = line.get(hashes..)?;
    if !(1..=6).contains(&hashes) || !text.starts_with(' ') {
        return None;
    }
    let level = u8::try_from(hashes.min(3)).unwrap_or(3);
    Some((level, text.trim().trim_end_matches('#').trim_end()))
}

fn is_rule(line: &str) -> bool {
    let marks: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    marks.len() >= 3 && ["-", "*", "_"].iter().any(|mark| marks.chars().all(|c| mark.starts_with(c)))
}

/// `(ordered, text)` of a list item line.
fn list_item_of(line: &str) -> Option<(bool, &str)> {
    for marker in ["- ", "* ", "+ "] {
        if let Some(text) = line.strip_prefix(marker) {
            return Some((false, text.trim()));
        }
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    let rest = line.get(digits..)?;
    if (1..=9).contains(&digits)
        && let Some(text) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") "))
    {
        return Some((true, text.trim()));
    }
    None
}

/// The cells of one `| a | b |` line. `\|` is a literal bar.
fn cells_of(line: &str) -> Vec<String> {
    let inner = line.trim().trim_start_matches('|');
    let inner = inner.strip_suffix('|').filter(|inner| !inner.ends_with('\\')).unwrap_or(inner);
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cell.push('|');
                chars.next();
            }
            '|' => cells.push(std::mem::take(&mut cell).trim().to_owned()),
            other => cell.push(other),
        }
    }
    cells.push(cell.trim().to_owned());
    cells
}

fn is_separator_row(cells: &[String]) -> bool {
    !cells.is_empty()
        && cells.iter().all(|cell| cell.contains('-') && cell.chars().all(|c| matches!(c, '-' | ':' | ' ')))
}

fn table_of(lines: &[&str]) -> Block {
    let mut rows: Vec<Vec<String>> = lines.iter().map(|line| cells_of(line)).collect();
    let header = rows.get(1).is_some_and(|row| is_separator_row(row));
    if header {
        rows.remove(1);
    }
    // Cells are plain text: styles inside a table are dropped, their markers too.
    for cell in rows.iter_mut().flatten() {
        *cell = plain_text(&inline(cell));
    }
    Block::Table { header, rows }
}

/// Split text into styled spans: `**bold**`, `*italic*`, `` `code` ``,
/// `[text](url)` as `text (url)`, and `\` before a marker for the marker itself.
pub fn inline(text: &str) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    let mut current = Span::default();
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    let rest_has = |from: usize, marker: &[char]| {
        chars.get(from..).is_some_and(|rest| rest.windows(marker.len()).any(|window| window == marker))
    };
    let flush = |current: &mut Span, spans: &mut Vec<Span>| {
        if !current.text.is_empty() {
            spans.push(std::mem::replace(current, Span { text: String::new(), ..current.clone() }));
        }
    };
    while let Some(&c) = chars.get(index) {
        let next = chars.get(index + 1).copied();
        match c {
            '\\' if next.is_some_and(|next| matches!(next, '*' | '`' | '\\' | '[' | '_' | '#' | '|')) => {
                current.text.push(next.unwrap_or('\\'));
                index += 2;
                continue;
            }
            '`' if current.code || rest_has(index + 1, &['`']) => {
                flush(&mut current, &mut spans);
                current.code = !current.code;
            }
            _ if current.code => current.text.push(c),
            '*' if next == Some('*') => {
                if current.bold || opens(&chars, index + 2, &['*', '*']) {
                    flush(&mut current, &mut spans);
                    current.bold = !current.bold;
                } else {
                    current.text.push_str("**");
                }
                index += 1;
            }
            '*' if current.italic || opens(&chars, index + 1, &['*']) => {
                flush(&mut current, &mut spans);
                current.italic = !current.italic;
            }
            '[' => match link_at(&chars, index) {
                Some((label, url, end)) => {
                    current.text.push_str(&label);
                    if !url.is_empty() && url != label {
                        current.text.push_str(&format!(" ({url})"));
                    }
                    index = end;
                    continue;
                }
                None => current.text.push(c),
            },
            other => current.text.push(other),
        }
        index += 1;
    }
    if !current.text.is_empty() {
        spans.push(current);
    }
    spans
}

/// Whether a style marker at `from - marker.len()` opens a style: text follows
/// directly, and the marker closes somewhere later.
fn opens(chars: &[char], from: usize, marker: &[char]) -> bool {
    let follows = chars.get(from).is_some_and(|c| !c.is_whitespace());
    follows && chars.get(from..).is_some_and(|rest| rest.windows(marker.len()).any(|window| window == marker))
}

/// `[label](url)` starting at `start`: the label, the URL and the index after it.
fn link_at(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    let rest = chars.get(start + 1..)?;
    let close = rest.iter().position(|c| *c == ']')?;
    if rest.get(close + 1) != Some(&'(') {
        return None;
    }
    let after = rest.get(close + 2..)?;
    let end = after.iter().position(|c| *c == ')')?;
    let label: String = rest.get(..close)?.iter().collect();
    let url: String = after.get(..end)?.iter().collect();
    Some((label, url.trim().to_owned(), start + 1 + close + 2 + end + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(block: &Block) -> String {
        match block {
            Block::Heading { spans, .. } | Block::Paragraph(spans) => plain_text(spans),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn blocks_of_a_report() {
        let blocks = parse(
            "# Sales report\n\nRevenue grew **12%**\nover the quarter.\n\n## By region\n- Jakarta\n- Surabaya\n  and around\n\n1. first\n2) second\n\n| Region | Revenue |\n|---|---:|\n| Jakarta | 120 |\n| A \\| B | 80 |\n\n---\n\\newpage\n```\nlet x = 1;\n  indented\n```\n> quoted",
        );
        assert_eq!(blocks.len(), 10);
        assert_eq!(blocks[0], Block::Heading { level: 1, spans: vec![Span::plain("Sales report")] });
        assert_eq!(text_of(&blocks[1]), "Revenue grew 12% over the quarter.");
        assert_eq!(blocks[2], Block::Heading { level: 2, spans: vec![Span::plain("By region")] });
        let Block::List { ordered: false, items } = &blocks[3] else { panic!("{:?}", blocks[3]) };
        assert_eq!(items.iter().map(|item| plain_text(item)).collect::<Vec<_>>(), ["Jakarta", "Surabaya and around"]);
        let Block::List { ordered: true, items } = &blocks[4] else { panic!("{:?}", blocks[4]) };
        assert_eq!(items.len(), 2);
        let Block::Table { header: true, rows } = &blocks[5] else { panic!("{:?}", blocks[5]) };
        assert_eq!(rows, &[vec!["Region", "Revenue"], vec!["Jakarta", "120"], vec!["A | B", "80"]]);
        assert_eq!(blocks[6], Block::Rule);
        assert_eq!(blocks[7], Block::PageBreak);
        assert_eq!(blocks[8], Block::Code(vec!["let x = 1;".into(), "  indented".into()]));
        assert_eq!(text_of(&blocks[9]), "quoted");
    }

    #[test]
    fn inline_styles_and_what_stays_literal() {
        let spans = inline("plain **bold** *it* `co*de` [site](https://example.com) \\*kept\\*");
        let styled: Vec<(&str, bool, bool, bool)> =
            spans.iter().map(|span| (span.text.as_str(), span.bold, span.italic, span.code)).collect();
        assert_eq!(
            styled,
            [
                ("plain ", false, false, false),
                ("bold", true, false, false),
                (" ", false, false, false),
                ("it", false, true, false),
                (" ", false, false, false),
                ("co*de", false, false, true),
                (" site (https://example.com) *kept*", false, false, false),
            ]
        );
        // A lone marker is text, not a style that swallows the rest.
        assert_eq!(inline("5 * 3 = 15 and 2 ** 8"), [Span::plain("5 * 3 = 15 and 2 ** 8")]);
        assert_eq!(inline("a * b * c"), [Span::plain("a * b * c")]);
        assert_eq!(inline("snake_case_name and a ` tick"), [Span::plain("snake_case_name and a ` tick")]);
        assert_eq!(inline("[not a link] (x)"), [Span::plain("[not a link] (x)")]);
        assert!(inline("").is_empty());
    }

    #[test]
    fn odd_input_never_panics() {
        for input in [
            "|",
            "||",
            "| a",
            "#",
            "####### seven",
            "```",
            "- ",
            "1.",
            "**",
            "*",
            "`",
            "[",
            "[](",
            "\\",
            "| a |\n|",
            "\u{0}\u{202e}",
        ] {
            let _ = parse(input);
        }
        assert_eq!(parse("#hashtag"), [Block::Paragraph(vec![Span::plain("#hashtag")])]);
        assert_eq!(parse("| only |"), [Block::Table { header: false, rows: vec![vec!["only".into()]] }]);
    }
}
