//! Readable text of an HTML page, for steps that hand a fetched page to a model.
//!
//! A web page is mostly markup, scripts and styles; a model with a small context window
//! reads the first few thousand characters and finds no content in them. [`html_to_text`]
//! keeps what a person would read: text, with a line break where a block ends. It is a
//! tolerant scanner, not an HTML parser: malformed pages give imperfect text, never an error.

/// Elements whose content is not text for a reader.
const SKIPPED_ELEMENTS: &[&str] = &["script", "style", "noscript", "template", "svg", "head", "iframe", "canvas"];

/// Elements that end a line of text.
const BLOCK_ELEMENTS: &[&str] = &[
    "p",
    "div",
    "br",
    "li",
    "ul",
    "ol",
    "tr",
    "table",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "section",
    "article",
    "header",
    "footer",
    "nav",
    "aside",
    "main",
    "blockquote",
    "pre",
    "hr",
    "dt",
    "dd",
    "figure",
    "figcaption",
    "form",
    "title",
    "td",
    "th",
    "option",
];

/// Whether a response with this content type is an HTML page.
pub(crate) fn is_html_type(content_type: &str) -> bool {
    let media_type = content_type.split(';').next().unwrap_or_default().trim();
    media_type.eq_ignore_ascii_case("text/html") || media_type.eq_ignore_ascii_case("application/xhtml+xml")
}

/// The text of an HTML document: tags, scripts, styles and comments removed, character
/// references decoded, runs of spaces collapsed, one line per block and no empty lines.
///
/// @example `html_to_text("<h1>Hi</h1><p>a &amp; b</p><script>x()</script>") == "Hi\na & b"`
pub(crate) fn html_to_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len() / 4);
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        push_text(&mut text, &rest[..open]);
        let after_open = &rest[open + 1..];
        if let Some(comment) = after_open.strip_prefix("!--") {
            rest = comment.find("-->").map_or("", |end| &comment[end + 3..]);
            continue;
        }
        let Some(close) = after_open.find('>') else {
            // An unclosed `<` at the end of the document: nothing readable follows.
            rest = "";
            break;
        };
        let tag = &after_open[..close];
        rest = &after_open[close + 1..];
        let is_closing = tag.starts_with('/');
        let name = tag_name(tag);
        if !is_closing && SKIPPED_ELEMENTS.contains(&name.as_str()) && !tag.ends_with('/') {
            rest = skip_element(rest, &name);
            continue;
        }
        if BLOCK_ELEMENTS.contains(&name.as_str()) {
            text.push('\n');
        }
    }
    push_text(&mut text, rest);
    tidy(&text)
}

/// Lower-case element name of a tag's inside (`/DIV class="x"` gives `div`).
fn tag_name(tag: &str) -> String {
    tag.trim_start_matches('/')
        .chars()
        .take_while(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

/// What follows the closing tag of `name`; the rest of the document when it is never closed.
fn skip_element<'a>(rest: &'a str, name: &str) -> &'a str {
    let closing = format!("</{name}");
    let mut offset = 0;
    while let Some(found) = find_ignore_ascii_case(&rest[offset..], &closing) {
        let after_name = offset + found + closing.len();
        let ends_name = rest[after_name..].chars().next().is_none_or(|character| !character.is_ascii_alphanumeric());
        if ends_name {
            return rest[after_name..].find('>').map_or("", |end| &rest[after_name + end + 1..]);
        }
        offset = after_name;
    }
    ""
}

fn find_ignore_ascii_case(haystack: &str, needle: &str) -> Option<usize> {
    let (haystack, needle) = (haystack.as_bytes(), needle.as_bytes());
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len())
        .find(|&start| haystack[start..start + needle.len()].eq_ignore_ascii_case(needle))
}

/// Appends text between tags with its character references decoded.
fn push_text(text: &mut String, raw: &str) {
    let mut rest = raw;
    while let Some(ampersand) = rest.find('&') {
        text.push_str(&rest[..ampersand]);
        let after = &rest[ampersand + 1..];
        let reference_end =
            after.char_indices().take(12).find(|(_, character)| *character == ';').map(|(index, _)| index);
        match reference_end.and_then(|end| decode_reference(&after[..end]).map(|decoded| (end, decoded))) {
            Some((end, decoded)) => {
                text.push(decoded);
                rest = &after[end + 1..];
            }
            None => {
                text.push('&');
                rest = after;
            }
        }
    }
    text.push_str(rest);
}

/// The character of `amp`, `#39`, `#x27`...; `None` for a reference this does not know.
fn decode_reference(reference: &str) -> Option<char> {
    if let Some(number) = reference.strip_prefix('#') {
        let code = match number.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => number.parse::<u32>().ok()?,
        };
        return char::from_u32(code).filter(|character| !character.is_control());
    }
    Some(match reference {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "ndash" => '\u{2013}',
        "mdash" => '\u{2014}',
        "hellip" => '\u{2026}',
        "copy" => '\u{a9}',
        "reg" => '\u{ae}',
        "laquo" => '\u{ab}',
        "raquo" => '\u{bb}',
        "lsquo" => '\u{2018}',
        "rsquo" => '\u{2019}',
        "ldquo" => '\u{201c}',
        "rdquo" => '\u{201d}',
        _ => return None,
    })
}

/// One line per block: spaces collapsed, empty lines dropped.
fn tidy(text: &str) -> String {
    let mut tidy = String::with_capacity(text.len());
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let Some(first) = words.next() else { continue };
        if !tidy.is_empty() {
            tidy.push('\n');
        }
        tidy.push_str(first);
        for word in words {
            tidy.push(' ');
            tidy.push_str(word);
        }
    }
    tidy
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_what_a_reader_sees() {
        let html = r#"<!DOCTYPE html><html><head><title>Shop</title><style>p { color: red }</style>
            <script>var a = "<p>not text</p>";</script></head>
            <body><!-- banner --><nav><a href="/">Home</a> <a href="/about">About</a></nav>
            <h1>Opening   hours</h1><p>Mon&ndash;Fri&nbsp;9 &amp; 17<br>Closed on <b>Sunday</b></p>
            <ul><li>Coffee</li><li>Tea &#38; cake &#x263A;</li></ul></body></html>"#;
        assert_eq!(
            html_to_text(html),
            "Home About\nOpening hours\nMon\u{2013}Fri 9 & 17\nClosed on Sunday\nCoffee\nTea & cake \u{263a}"
        );
    }

    #[test]
    fn scripts_and_styles_are_dropped_whatever_their_case_or_content() {
        let html = "a<SCRIPT type=\"x\">if (a < b) { document.write('</scripts>') }</Script>b<style>x</style>c<svg><path/></svg>d";
        assert_eq!(html_to_text(html), "abcd");
        assert_eq!(html_to_text("before<script>never closed"), "before");
    }

    #[test]
    fn malformed_markup_gives_text_not_an_error() {
        assert_eq!(html_to_text("plain text, no tags"), "plain text, no tags");
        assert_eq!(html_to_text("a < b"), "a");
        assert_eq!(html_to_text("AT&T &unknown; &#xZZ; 5 &"), "AT&T &unknown; &#xZZ; 5 &");
        assert_eq!(html_to_text("<p>é ü 日本</p><br/><br/>"), "é ü 日本");
        assert_eq!(html_to_text(""), "");
        assert_eq!(html_to_text("x<!-- never closed"), "x");
    }

    #[test]
    fn html_content_types_are_recognised() {
        assert!(is_html_type("text/html; charset=utf-8"));
        assert!(is_html_type("TEXT/HTML"));
        assert!(is_html_type("application/xhtml+xml"));
        assert!(!is_html_type("application/json"));
        assert!(!is_html_type("text/plain"));
    }
}
