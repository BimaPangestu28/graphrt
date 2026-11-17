//! Text-based formats: plain text, delimited tables, HTML, subtitles.

use crate::OutputFormat;

/// Decode bytes as text: BOM-aware UTF-8/UTF-16, falling back to Windows-1252
/// (a superset of Latin-1) when the bytes are not valid UTF-8.
pub fn decode(bytes: &[u8]) -> String {
    if let Some((encoding, bom_len)) = encoding_rs::Encoding::for_bom(bytes) {
        let body = bytes.get(bom_len..).unwrap_or_default();
        let (text, _) = encoding.decode_without_bom_handling(body);
        return text.into_owned();
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        Err(_) => {
            let (text, _) = encoding_rs::WINDOWS_1252.decode_without_bom_handling(bytes);
            text.into_owned()
        }
    }
}

/// Parse delimited text (CSV / TSV) into rows. Handles quoted fields with embedded
/// delimiters, doubled quotes and newlines.
pub fn parse_delimited(text: &str, delimiter: char) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => in_quotes = true,
            c if c == delimiter => row.push(std::mem::take(&mut field)),
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            c => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// Render rows as text: Markdown table (first row = header) or tab-separated lines.
///
/// The table may not be wider than [`Options::max_cols`](crate::Options::max_cols)
/// columns, and rendering stops as soon as the output exceeds
/// [`Options::max_output_chars`](crate::Options::max_output_chars), so padding short
/// rows to the table width can never multiply the output beyond that budget.
pub fn render_table(rows: &[Vec<String>], opts: &crate::Options) -> Result<String, crate::DocError> {
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut table = TableWriter::new(width, opts)?;
    for row in rows {
        if !table.row(row.iter().map(String::as_str)) {
            break;
        }
    }
    Ok(table.finish())
}

/// Markdown table with the first row as header, using the default limits.
/// Pipes and newlines in cells are escaped.
pub fn markdown_table(rows: &[Vec<String>]) -> String {
    let opts = crate::Options::default().with_format(OutputFormat::Markdown);
    render_table(rows, &opts).unwrap_or_default()
}

/// Streaming table renderer with a running character budget.
#[derive(Debug)]
pub(crate) struct TableWriter {
    out: String,
    chars: usize,
    budget: usize,
    width: usize,
    markdown: bool,
    rows: usize,
}

impl TableWriter {
    /// A writer for a table `width` columns wide. Fails when the table is wider than
    /// `opts.max_cols`.
    pub(crate) fn new(width: usize, opts: &crate::Options) -> Result<Self, crate::DocError> {
        if width > opts.max_cols {
            return Err(crate::DocError::LimitExceeded(format!("table has {width} columns (limit {})", opts.max_cols)));
        }
        Ok(Self {
            out: String::new(),
            chars: 0,
            budget: opts.max_output_chars,
            width,
            markdown: opts.format == OutputFormat::Markdown,
            rows: 0,
        })
    }

    /// Append one row. Returns `false` once the output budget is exhausted; further
    /// rows are ignored.
    pub(crate) fn row<'a>(&mut self, cells: impl Iterator<Item = &'a str>) -> bool {
        if self.width == 0 {
            return true;
        }
        if self.chars > self.budget {
            return false;
        }
        let mut line = String::new();
        if self.markdown {
            line.push('|');
            let mut written = 0usize;
            for cell in cells.take(self.width) {
                line.push(' ');
                for c in cell.chars() {
                    match c {
                        '|' => line.push_str("\\|"),
                        '\r' | '\n' => line.push(' '),
                        c => line.push(c),
                    }
                }
                line.push_str(" |");
                written += 1;
            }
            for _ in written..self.width {
                line.push_str("  |");
            }
        } else {
            let joined: Vec<&str> = cells.take(self.width).collect();
            line.push_str(joined.join("\t").trim_end());
        }
        self.push_line(&line);
        self.rows += 1;
        if self.markdown && self.rows == 1 {
            let separator = format!("|{}", " --- |".repeat(self.width));
            self.push_line(&separator);
        }
        self.chars <= self.budget
    }

    fn push_line(&mut self, line: &str) {
        if !self.out.is_empty() {
            self.out.push('\n');
            self.chars += 1;
        }
        self.out.push_str(line);
        self.chars += line.chars().count();
    }

    /// The rendered table.
    pub(crate) fn finish(self) -> String {
        self.out
    }
}

/// Extract the cue text of WebVTT / SRT subtitles (timings, indexes and headers removed).
pub fn subtitles(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut in_note = false;
    for raw in text.lines() {
        let line = raw.trim().trim_start_matches('\u{feff}');
        if line.is_empty() {
            in_note = false;
            continue;
        }
        if in_note {
            continue;
        }
        if line.starts_with("WEBVTT") || line.starts_with("STYLE") || line.starts_with("REGION") {
            in_note = true;
            continue;
        }
        if line.starts_with("NOTE") {
            in_note = true;
            continue;
        }
        if line.contains("-->") || line.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let cleaned = strip_inline_tags(line);
        if out.last() != Some(&cleaned) && !cleaned.is_empty() {
            out.push(cleaned);
        }
    }
    out.join("\n")
}

fn strip_inline_tags(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_tag = false;
    for c in line.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    decode_entities(out.trim())
}

/// Convert HTML to readable text. Scripts, styles and comments are dropped; block
/// elements become line breaks. In Markdown mode headings become `#` lines and
/// list items `- ` lines.
pub fn html_to_text(html: &str, format: OutputFormat) -> String {
    let markdown = format == OutputFormat::Markdown;
    let mut out = String::with_capacity(html.len() / 2);
    let lower = html.to_ascii_lowercase();
    let bytes = html.as_bytes();
    let mut i = 0;
    let mut text_start = 0;
    while i < bytes.len() {
        if bytes.get(i) != Some(&b'<') {
            i += 1;
            continue;
        }
        push_text(&mut out, html.get(text_start..i).unwrap_or_default());
        let rest = lower.get(i..).unwrap_or_default();
        if rest.starts_with("<!--") {
            i = rest.find("-->").map_or(bytes.len(), |p| i + p + 3);
            text_start = i;
            continue;
        }
        let Some(end) = rest.find('>') else {
            text_start = bytes.len();
            break;
        };
        let tag = rest.get(1..end).unwrap_or_default();
        let closing = tag.starts_with('/');
        let name: String =
            tag.trim_start_matches('/').chars().take_while(|c| c.is_ascii_alphanumeric()).collect::<String>();
        i += end + 1;
        if !closing && matches!(name.as_str(), "script" | "style" | "noscript" | "template" | "head") {
            let close = format!("</{name}");
            i = lower.get(i..).and_then(|r| r.find(&close)).map_or(bytes.len(), |p| {
                let after = i + p;
                lower.get(after..).and_then(|r| r.find('>')).map_or(bytes.len(), |q| after + q + 1)
            });
            text_start = i;
            continue;
        }
        match name.as_str() {
            "br" => out.push('\n'),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                ensure_blank_line(&mut out);
                if markdown && !closing {
                    let level = name.get(1..).and_then(|d| d.parse::<usize>().ok()).unwrap_or(1);
                    out.push_str(&"#".repeat(level));
                    out.push(' ');
                }
            }
            "li" if !closing => {
                ensure_newline(&mut out);
                if markdown {
                    out.push_str("- ");
                }
            }
            "td" | "th" if !closing && !out.ends_with('\n') && !out.is_empty() => out.push('\t'),
            "p" | "div" | "section" | "article" | "header" | "footer" | "blockquote" | "pre" | "table" | "ul"
            | "ol" | "nav" | "aside" | "main" | "figure" | "hr" => ensure_blank_line(&mut out),
            "tr" | "li" | "dt" | "dd" => ensure_newline(&mut out),
            _ => {}
        }
        text_start = i;
    }
    push_text(&mut out, html.get(text_start..).unwrap_or_default());
    collapse_blank_lines(&out)
}

fn push_text(out: &mut String, raw: &str) {
    if raw.is_empty() {
        return;
    }
    let decoded = decode_entities(raw);
    let mut last_space = out.ends_with([' ', '\n', '\t']) || out.is_empty();
    for c in decoded.chars() {
        if c.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(c);
            last_space = false;
        }
    }
}

fn ensure_newline(out: &mut String) {
    trim_trailing_spaces(out);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

fn ensure_blank_line(out: &mut String) {
    trim_trailing_spaces(out);
    if out.is_empty() {
        return;
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.ends_with("\n\n") {
        out.push('\n');
    }
}

fn trim_trailing_spaces(out: &mut String) {
    while out.ends_with(' ') || out.ends_with('\t') {
        out.pop();
    }
}

/// Trim each line and collapse runs of blank lines to one.
pub fn collapse_blank_lines(text: &str) -> String {
    let mut out = Vec::new();
    let mut blank = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            if !blank && !out.is_empty() {
                out.push("");
            }
            blank = true;
        } else {
            out.push(line);
            blank = false;
        }
    }
    while out.last() == Some(&"") {
        out.pop();
    }
    out.join("\n")
}

/// Decode the common named entities plus numeric character references.
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(rest.get(..pos).unwrap_or_default());
        let after = rest.get(pos + 1..).unwrap_or_default();
        // Entity names are short: only look at a bounded window, so input with many
        // `&` and no `;` stays linear.
        let entity_end = after.bytes().take(11).position(|b| b == b';');
        let decoded = entity_end.and_then(|e| decode_entity(after.get(..e).unwrap_or_default()));
        match (decoded, entity_end) {
            (Some(c), Some(e)) => {
                out.push(c);
                rest = after.get(e + 1..).unwrap_or_default();
            }
            _ => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn decode_entity(name: &str) -> Option<char> {
    if let Some(num) = name.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse().ok()?,
        };
        return char::from_u32(code);
    }
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "ndash" => '–',
        "mdash" => '—',
        "hellip" => '…',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "bull" => '•',
        "middot" => '·',
        "euro" => '€',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_latin1_fallback_and_bom() {
        assert_eq!(decode(b"caf\xe9"), "café");
        assert_eq!(decode("\u{feff}hi".as_bytes()), "hi");
        assert_eq!(decode(&[0xFF, 0xFE, b'h', 0, b'i', 0]), "hi");
    }

    #[test]
    fn csv_quotes() {
        let rows = parse_delimited("a,b\n\"x,1\",\"say \"\"hi\"\"\nthere\"\r\n", ',');
        assert_eq!(rows, vec![vec!["a", "b"], vec!["x,1", "say \"hi\"\nthere"]]);
        let md = markdown_table(&rows);
        assert!(md.starts_with("| a | b |\n| --- | --- |\n"), "{md}");
        assert!(md.contains("say \"hi\" there"));
    }

    #[test]
    fn html_strips_scripts_and_styles() {
        let html = "<html><head><title>T</title><style>p{}</style></head><body>\
            <h1>Title</h1><script>alert('x')</script><p>Hello&nbsp;<b>world</b> &amp; you</p>\
            <!-- secret --><ul><li>one</li><li>two</li></ul></body></html>";
        let text = html_to_text(html, OutputFormat::Text);
        assert_eq!(text, "Title\n\nHello world & you\n\none\ntwo");
        let md = html_to_text(html, OutputFormat::Markdown);
        assert!(md.contains("# Title"));
        assert!(md.contains("- one\n- two"));
        assert!(!md.contains("alert") && !md.contains("secret") && !md.contains("p{}"));
    }

    #[test]
    fn subtitle_cues() {
        let vtt = "WEBVTT\n\nNOTE hidden\nstill hidden\n\n1\n00:00:01.000 --> 00:00:02.000\n<v Bob>Hello</v>\n\n00:00:03.000 --> 00:00:04.000\nWorld &amp; all\n";
        assert_eq!(subtitles(vtt), "Hello\nWorld & all");
        let srt = "1\n00:00:01,000 --> 00:00:02,000\nHi\n\n2\n00:00:02,000 --> 00:00:03,000\nThere\n";
        assert_eq!(subtitles(srt), "Hi\nThere");
    }
}
