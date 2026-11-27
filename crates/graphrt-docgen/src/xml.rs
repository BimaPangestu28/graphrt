//! Text for XML: escaped, and without the characters XML 1.0 cannot hold.

/// `text` as XML character data or an attribute value. The five characters
/// with a meaning are escaped, and control characters that would make the
/// document invalid are left out, so no text can open a tag.
pub(crate) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if c.is_control() || matches!(c, '\u{fffe}' | '\u{ffff}') => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::escape;

    #[test]
    fn text_cannot_open_a_tag_or_break_the_document() {
        assert_eq!(escape("a < b & \"c\" > 'd'"), "a &lt; b &amp; &quot;c&quot; &gt; &apos;d&apos;");
        assert_eq!(escape("</w:t><w:hyperlink r:id=\"x\">"), "&lt;/w:t&gt;&lt;w:hyperlink r:id=&quot;x&quot;&gt;");
        assert_eq!(escape("a\u{0}b\u{1b}c\u{7f}d\te\nf\u{ffff}"), "abcd\te\nf");
        assert_eq!(escape("&amp;"), "&amp;amp;");
    }
}
