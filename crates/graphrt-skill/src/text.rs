//! Checks on text that a model or a person will read.
//!
//! A skill's name and description go into an agent's system prompt, and its
//! files are shown to people who decide whether to install it. Characters
//! that render as nothing, or that reorder what is displayed, can hide an
//! instruction from the person while the model still reads it.

/// The first character of `text` that a reader cannot see or that changes how
/// the text around it is displayed, with its 1-based line.
///
/// Flags control characters other than tab, line feed and carriage return,
/// zero width and word joiner characters, bidirectional formatting, and the
/// Unicode tag block (U+E0000 to U+E007F), which encodes invisible ASCII.
pub fn hidden_character(text: &str) -> Option<(u32, char)> {
    let mut line = 1u32;
    for character in text.chars() {
        if character == '\n' {
            line = line.saturating_add(1);
            continue;
        }
        if is_hidden(character) {
            return Some((line, character));
        }
    }
    None
}

fn is_hidden(character: char) -> bool {
    if character == '\t' || character == '\r' {
        return false;
    }
    character.is_control()
        || matches!(
            character,
            // Zero width space, joiners and directional marks.
            '\u{200B}'..='\u{200F}'
            // Bidirectional embeddings and overrides.
            | '\u{202A}'..='\u{202E}'
            // Word joiner and invisible operators.
            | '\u{2060}'..='\u{2064}'
            // Bidirectional isolates.
            | '\u{2066}'..='\u{2069}'
            // Soft hyphen, Arabic letter mark, zero width no-break space.
            | '\u{00AD}' | '\u{061C}' | '\u{FEFF}'
            // Unicode tags: an invisible copy of ASCII.
            | '\u{E0000}'..='\u{E007F}'
        )
}

/// Whether `text` contains something shaped like a markup tag (`<name`, `</`,
/// `<!` or `<?`). Names and descriptions sit in a system prompt, where a tag
/// could pose as structure the operator wrote.
pub fn looks_like_markup(text: &str) -> bool {
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '<'
            && characters.peek().is_some_and(|next| next.is_ascii_alphabetic() || matches!(next, '/' | '!' | '?'))
        {
            return true;
        }
    }
    false
}

/// `text` on one line: every run of whitespace becomes one space.
pub fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// At most `max_chars` characters of `text`, ending in `...` when it was cut.
pub fn shorten(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max_chars.saturating_sub(3)).collect();
    format!("{}...", kept.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_text_has_no_hidden_characters() {
        assert_eq!(hidden_character("Tabs\tand\r\nnew lines are fine. Émoji 🙂 too."), None);
    }

    #[test]
    fn invisible_and_reordering_characters_are_found_with_their_line() {
        assert_eq!(hidden_character("a\nb\u{200B}c"), Some((2, '\u{200B}')));
        assert_eq!(hidden_character("x\u{202E}y"), Some((1, '\u{202E}')));
        assert_eq!(hidden_character("tag\u{E0041}"), Some((1, '\u{E0041}')));
        assert_eq!(hidden_character("bell\u{7}"), Some((1, '\u{7}')));
        assert_eq!(hidden_character("\u{FEFF}bom"), Some((1, '\u{FEFF}')));
    }

    #[test]
    fn markup_is_recognised_but_comparisons_are_not() {
        assert!(looks_like_markup("use <system>now</system>"));
        assert!(looks_like_markup("</instructions>"));
        assert!(looks_like_markup("<!-- note -->"));
        assert!(!looks_like_markup("files < 10 MB and a > b"));
        assert!(!looks_like_markup("1 <2"));
    }

    #[test]
    fn shorten_counts_characters() {
        assert_eq!(shorten("héllo", 5), "héllo");
        assert_eq!(shorten("héllo wörld", 8), "héllo...");
        assert_eq!(single_line(" a\n  b\tc "), "a b c");
    }
}
