//! Answers as a channel takes them: cut to its length limit, and with the
//! Markdown a model writes turned into the channel's own formatting.
//!
//! A model answers in Markdown. Telegram shows Markdown only in a dialect that
//! refuses a message over one unescaped character, and WhatsApp has its own
//! marks (`*bold*`, `_italic_`). So:
//!
//! - [`split`] cuts the Markdown source at paragraph and line ends. A code block
//!   that crosses a cut is closed in one part and opened again in the next.
//! - [`to_telegram_html`] writes one part as Telegram HTML (`parse_mode: HTML`),
//!   where only `&`, `<` and `>` need escaping, so no text can break a message.
//! - [`to_whatsapp`] writes one part with WhatsApp's marks.
//!
//! Split first, then convert each part: every part is then well formed on its own.
//!
//! ```
//! use graphrt_channel::text::{split, to_telegram_html, to_whatsapp};
//!
//! let answer = "## Refund\nYour order **#1042** is refunded. See [the policy](https://example.com/p).";
//! assert_eq!(
//!     to_telegram_html(answer),
//!     "<b>Refund</b>\nYour order <b>#1042</b> is refunded. See <a href=\"https://example.com/p\">the policy</a>."
//! );
//! assert_eq!(to_whatsapp(answer), "*Refund*\nYour order *#1042* is refunded. See the policy (https://example.com/p).");
//! assert_eq!(split(&"word ".repeat(20), 40).len(), 4);
//! ```

/// The length of `text` as Telegram and WhatsApp count it: UTF-16 code units.
pub fn units(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Room kept in every part for closing a code block that the cut interrupts.
const FENCE_ROOM: usize = 8;

/// `text` in parts of at most `max_units` UTF-16 code units each (at least 32),
/// cut at line ends where it can and inside a line only when one line is longer
/// than a part. Parts are trimmed and never empty.
pub fn split(text: &str, max_units: usize) -> Vec<String> {
    let max_units = max_units.max(32);
    let text = text.trim();
    if units(text) <= max_units {
        return if text.is_empty() { Vec::new() } else { vec![text.to_owned()] };
    }
    let room = max_units - FENCE_ROOM;
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_fence = false;
    let mut flush = |current: &mut String, in_fence: bool| {
        let mut part = std::mem::take(current);
        if in_fence {
            part.push_str("\n```");
            current.push_str("```\n");
        }
        let part = part.trim();
        // A part that only opens and closes a block says nothing.
        if !part.is_empty() && part != "```\n```" {
            parts.push(part.to_owned());
        }
    };
    for line in text.split('\n') {
        for piece in pieces_of(line, room) {
            let separator = usize::from(!current.is_empty());
            if units(&current) + separator + units(piece) > room && !current.trim().is_empty() {
                flush(&mut current, in_fence);
            }
            if !current.is_empty() && !current.ends_with('\n') {
                current.push('\n');
            }
            current.push_str(piece);
        }
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
    }
    flush(&mut current, false);
    parts
}

/// `line` whole when it fits `room`, else cut at spaces (or, without any, between characters).
fn pieces_of(line: &str, room: usize) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut rest = line;
    while units(rest) > room {
        let mut used = 0;
        let mut cut = 0;
        let mut last_space = None;
        for (index, character) in rest.char_indices() {
            if used + character.len_utf16() > room {
                break;
            }
            used += character.len_utf16();
            cut = index + character.len_utf8();
            if character == ' ' {
                last_space = Some(cut);
            }
        }
        let cut = last_space.unwrap_or(cut).max(1);
        let (head, tail) = rest.split_at_checked(cut).unwrap_or((rest, ""));
        pieces.push(head.trim_end());
        rest = tail;
    }
    pieces.push(rest);
    pieces
}

/// The first part of `text` that fits `max_units`, with `…` when something was cut.
pub fn truncate(text: &str, max_units: usize) -> String {
    if units(text) <= max_units {
        return text.to_owned();
    }
    let mut cut = String::new();
    let mut used = 1;
    for character in text.chars() {
        used += character.len_utf16();
        if used > max_units {
            break;
        }
        cut.push(character);
    }
    cut.push('…');
    cut
}

/// The marks of one channel.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavor {
    TelegramHtml,
    WhatsApp,
}

/// Markdown as Telegram HTML, for `sendMessage` with `parse_mode: HTML`.
/// Everything that is not a mark is escaped, so the result always parses.
pub fn to_telegram_html(markdown: &str) -> String {
    render(markdown, Flavor::TelegramHtml)
}

/// Markdown with WhatsApp's marks: `*bold*`, `_italic_`, `~strike~`, and links
/// written out, since WhatsApp links only bare addresses.
pub fn to_whatsapp(markdown: &str) -> String {
    render(markdown, Flavor::WhatsApp)
}

fn render(markdown: &str, flavor: Flavor) -> String {
    let mut out = String::with_capacity(markdown.len() + markdown.len() / 8);
    let mut in_fence = false;
    let mut in_quote = false;
    let html = flavor == Flavor::TelegramHtml;
    for (index, line) in markdown.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            match (html, in_fence) {
                (true, true) => out.push_str("<pre>"),
                (true, false) => {
                    // The line break before the closing mark belongs to the mark.
                    if out.ends_with('\n') {
                        out.pop();
                    }
                    out.push_str("</pre>");
                }
                (false, _) => out.push_str("```"),
            }
            continue;
        }
        if in_fence {
            push_text(&mut out, line, flavor);
            continue;
        }
        let quoted = trimmed.strip_prefix('>').map(|rest| rest.strip_prefix(' ').unwrap_or(rest));
        if html && quoted.is_some() != in_quote {
            // The break before a quote's first or after its last line is the mark's.
            if out.ends_with('\n') {
                out.pop();
            }
            out.push_str(if in_quote { "</blockquote>\n" } else { "\n<blockquote>" });
            if !in_quote && out.starts_with('\n') {
                out.remove(0);
            }
            in_quote = !in_quote;
        }
        match quoted {
            Some(quote) if html => inline(&mut out, quote, flavor),
            Some(quote) => {
                out.push_str("> ");
                inline(&mut out, quote, flavor);
            }
            None => block_line(&mut out, line, flavor),
        }
    }
    if html && in_fence {
        out.push_str("</pre>");
    }
    if html && in_quote {
        out.push_str("</blockquote>");
    }
    out
}

/// One line outside code blocks and quotes: a heading, a list item, a rule or text.
fn block_line(out: &mut String, line: &str, flavor: Flavor) {
    let trimmed = line.trim_start();
    let indent = line.get(..line.len() - trimmed.len()).unwrap_or_default();
    let hashes = trimmed.bytes().take_while(|byte| *byte == b'#').count();
    if (1..=6).contains(&hashes)
        && let Some(title) = trimmed.get(hashes..).and_then(|rest| rest.strip_prefix(' '))
    {
        // A heading is bold: marks inside it would nest the same mark twice.
        let title = title.trim().trim_end_matches('#').trim().replace("**", "").replace("__", "");
        let mut inner = String::new();
        inline(&mut inner, &title, flavor);
        push_marked(out, &inner, Mark::Bold, flavor);
        return;
    }
    let is_rule = trimmed.len() >= 3
        && ["-", "*", "_"].iter().any(|mark| trimmed.chars().all(|c| c.to_string() == *mark || c == ' '))
        && trimmed.chars().filter(|c| !c.is_whitespace()).count() >= 3;
    if is_rule {
        out.push_str("———");
        return;
    }
    for bullet in ["- ", "* ", "+ "] {
        if let Some(item) = trimmed.strip_prefix(bullet) {
            out.push_str(indent);
            out.push_str(if flavor == Flavor::WhatsApp { "- " } else { "• " });
            inline(out, item, flavor);
            return;
        }
    }
    inline(out, line, flavor);
}

#[derive(Clone, Copy)]
enum Mark {
    Bold,
    Italic,
    Strike,
}

/// `inner` (already rendered) between the marks of `mark`.
fn push_marked(out: &mut String, inner: &str, mark: Mark, flavor: Flavor) {
    let (open, close) = match (flavor, mark) {
        (Flavor::TelegramHtml, Mark::Bold) => ("<b>", "</b>"),
        (Flavor::TelegramHtml, Mark::Italic) => ("<i>", "</i>"),
        (Flavor::TelegramHtml, Mark::Strike) => ("<s>", "</s>"),
        (Flavor::WhatsApp, Mark::Bold) => ("*", "*"),
        (Flavor::WhatsApp, Mark::Italic) => ("_", "_"),
        (Flavor::WhatsApp, Mark::Strike) => ("~", "~"),
    };
    out.push_str(open);
    out.push_str(inner);
    out.push_str(close);
}

/// Text that is not a mark: escaped for HTML, as it is for WhatsApp.
fn push_text(out: &mut String, text: &str, flavor: Flavor) {
    if flavor == Flavor::WhatsApp {
        out.push_str(text);
        return;
    }
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
}

/// The position at or after `from` where `pattern` starts.
fn find(chars: &[char], from: usize, pattern: &[char]) -> Option<usize> {
    let haystack = chars.get(from..)?;
    haystack.windows(pattern.len()).position(|window| window == pattern).map(|offset| from + offset)
}

fn text_of(chars: &[char], from: usize, to: usize) -> String {
    chars.get(from..to).unwrap_or_default().iter().collect()
}

/// The emphasis, code and links of one line.
fn inline(out: &mut String, source: &str, flavor: Flavor) {
    let chars: Vec<char> = source.chars().collect();
    let at = |index: usize| chars.get(index).copied();
    let mut plain = String::new();
    let mut index = 0;
    while let Some(character) = at(index) {
        // Where the mark at `index` ends, with what it wraps already written to `out`.
        let mut consumed = None;
        match character {
            '\\' if at(index + 1).is_some_and(|next| next.is_ascii_punctuation()) => {
                plain.extend(at(index + 1));
                consumed = Some(index + 2);
            }
            '`' => {
                if let Some(end) = find(&chars, index + 1, &['`']).filter(|end| *end > index + 1) {
                    push_text(out, &std::mem::take(&mut plain), flavor);
                    let code = text_of(&chars, index + 1, end);
                    if flavor == Flavor::TelegramHtml {
                        out.push_str("<code>");
                        push_text(out, &code, flavor);
                        out.push_str("</code>");
                    } else {
                        out.push('`');
                        out.push_str(&code);
                        out.push('`');
                    }
                    consumed = Some(end + 1);
                }
            }
            '!' | '[' => {
                let open = if character == '!' { index + 1 } else { index };
                if at(open) == Some('[')
                    && let Some(close) = find(&chars, open + 1, &[']', '('])
                    && let Some(end) = find(&chars, close + 2, &[')'])
                {
                    push_text(out, &std::mem::take(&mut plain), flavor);
                    let label = text_of(&chars, open + 1, close);
                    let url = text_of(&chars, close + 2, end);
                    push_link(out, &label, url.trim(), flavor);
                    consumed = Some(end + 1);
                }
            }
            '*' | '_' | '~' => {
                let double = at(index + 1) == Some(character);
                let mark = match (character, double) {
                    ('~', true) => Some((Mark::Strike, 2)),
                    ('~', false) => None,
                    (_, true) => Some((Mark::Bold, 2)),
                    (_, false) => Some((Mark::Italic, 1)),
                };
                // `snake_case` and `2 * 3` are not emphasis.
                let opens = |width: usize| {
                    at(index + width).is_some_and(|next| !next.is_whitespace())
                        && (character != '_' || index == 0 || !at(index - 1).is_some_and(char::is_alphanumeric))
                };
                if let Some((mark, width)) = mark
                    && opens(width)
                    && let Some(end) = closing(&chars, index + width, character, width)
                {
                    push_text(out, &std::mem::take(&mut plain), flavor);
                    let mut inner = String::new();
                    inline(&mut inner, &text_of(&chars, index + width, end), flavor);
                    push_marked(out, &inner, mark, flavor);
                    consumed = Some(end + width);
                }
            }
            _ => {}
        }
        match consumed {
            Some(next) => index = next,
            None => {
                plain.push(character);
                index += 1;
            }
        }
    }
    push_text(out, &plain, flavor);
}

/// Where the emphasis opened before `from` closes: `width` times `mark`, after
/// text that does not end in a space, and for `_` not inside a word.
fn closing(chars: &[char], from: usize, mark: char, width: usize) -> Option<usize> {
    let at = |index: usize| chars.get(index).copied();
    let mut index = from + 1;
    while index < chars.len() {
        let here = (0..width).all(|offset| at(index + offset) == Some(mark));
        let after = at(index + width);
        let ends_word = !at(index - 1).is_some_and(char::is_whitespace);
        let alone = width == 2 || after != Some(mark);
        let outside_word = mark != '_' || !after.is_some_and(char::is_alphanumeric);
        if here && ends_word && alone && outside_word {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn push_link(out: &mut String, label: &str, url: &str, flavor: Flavor) {
    let linkable = ["https://", "http://", "mailto:", "tg://"].iter().any(|scheme| url.starts_with(scheme));
    let label = label.trim();
    if flavor == Flavor::TelegramHtml && linkable {
        out.push_str("<a href=\"");
        push_text(out, &url.replace('"', "%22"), flavor);
        out.push_str("\">");
        push_text(out, if label.is_empty() { url } else { label }, flavor);
        out.push_str("</a>");
        return;
    }
    let written = match (label.is_empty() || label == url, url.is_empty()) {
        (true, _) => url.to_owned(),
        (false, true) => label.to_owned(),
        (false, false) => format!("{label} ({url})"),
    };
    push_text(out, &written, flavor);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_text_is_one_part_and_nothing_is_no_part() {
        assert_eq!(split("  Halo  ", 4096), ["Halo"]);
        assert!(split("  \n ", 4096).is_empty());
    }

    #[test]
    fn long_text_is_cut_at_line_ends_and_nothing_is_lost() {
        let lines: Vec<String> = (0..400).map(|n| format!("Line number {n} of the answer.")).collect();
        let text = lines.join("\n");
        let parts = split(&text, 1000);
        assert!(parts.len() > 5);
        assert!(parts.iter().all(|part| units(part) <= 1000), "every part fits");
        assert_eq!(parts.join("\n"), text, "the parts are the text");
    }

    #[test]
    fn one_endless_line_is_cut_at_spaces_or_between_characters() {
        let words = "kata ".repeat(300);
        let parts = split(&words, 100);
        assert!(parts.iter().all(|part| units(part) <= 100 && !part.ends_with(' ')));
        assert_eq!(parts.join(" "), words.trim());
        let unbroken = "é".repeat(500);
        let parts = split(&unbroken, 64);
        assert!(parts.iter().all(|part| units(part) <= 64));
        assert_eq!(parts.concat(), unbroken);
    }

    #[test]
    fn length_is_counted_in_utf16_units() {
        // One emoji is two units: 40 of them do not fit a part of 64.
        let emoji = "😀".repeat(40);
        assert_eq!(units(&emoji), 80);
        let parts = split(&emoji, 64);
        assert!(parts.len() >= 2);
        assert!(parts.iter().all(|part| units(part) <= 64));
        assert_eq!(parts.concat(), emoji);
    }

    #[test]
    fn a_code_block_that_crosses_a_cut_is_closed_and_opened_again() {
        let code: Vec<String> = (0..60).map(|n| format!("let value_{n} = {n};")).collect();
        let text = format!("Here is the code:\n```rust\n{}\n```\nDone.", code.join("\n"));
        let parts = split(&text, 300);
        assert!(parts.len() > 2);
        for part in &parts {
            assert!(units(part) <= 300);
            assert_eq!(part.matches("```").count() % 2, 0, "balanced fences in: {part}");
        }
        assert!(parts.last().is_some_and(|last| last.ends_with("Done.")));
    }

    #[test]
    fn truncation_marks_what_it_cut() {
        assert_eq!(truncate("short", 10), "short");
        let cut = truncate(&"a".repeat(50), 10);
        assert_eq!(units(&cut), 10);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn telegram_html_escapes_everything_that_is_not_a_mark() {
        assert_eq!(to_telegram_html("1 < 2 && 3 > 2"), "1 &lt; 2 &amp;&amp; 3 &gt; 2");
        assert_eq!(to_telegram_html("<b>not bold</b>"), "&lt;b&gt;not bold&lt;/b&gt;");
        assert_eq!(to_telegram_html("`a < b`"), "<code>a &lt; b</code>");
        assert_eq!(
            to_telegram_html("[x](https://e.com/?a=1&b=\"2\")"),
            "<a href=\"https://e.com/?a=1&amp;b=%222%22\">x</a>"
        );
        // A link that is not a web address stays text: nothing to tap by mistake.
        assert_eq!(to_telegram_html("[x](javascript:alert(1))"), "x (javascript:alert(1))");
    }

    #[test]
    fn emphasis_is_converted_and_ordinary_marks_are_left_alone() {
        assert_eq!(
            to_telegram_html("**bold** *it* _it_ ~~no~~ __b__"),
            "<b>bold</b> <i>it</i> <i>it</i> <s>no</s> <b>b</b>"
        );
        assert_eq!(to_whatsapp("**bold** *it* _it_ ~~no~~"), "*bold* _it_ _it_ ~no~");
        assert_eq!(to_whatsapp("**bold with *nested* text**"), "*bold with _nested_ text*");
        for untouched in ["snake_case_name and other_name", "2 * 3 * 4", "a ** b", "5 ~ 6", "\\*literal\\*"] {
            let expected = untouched.replace('\\', "");
            assert_eq!(to_whatsapp(untouched), expected);
            assert_eq!(to_telegram_html(untouched), expected);
        }
    }

    #[test]
    fn blocks_become_what_the_channel_shows() {
        let markdown = "# Title **x**\n\n- one\n* two\n1. three\n\n---\n> quoted *text*\n> more\nafter";
        assert_eq!(
            to_telegram_html(markdown),
            "<b>Title x</b>\n\n• one\n• two\n1. three\n\n———\n<blockquote>quoted <i>text</i>\nmore</blockquote>\nafter"
        );
        assert_eq!(to_whatsapp(markdown), "*Title x*\n\n- one\n- two\n1. three\n\n———\n> quoted _text_\n> more\nafter");
    }

    #[test]
    fn code_blocks_keep_their_text() {
        let markdown = "Run:\n```sh\necho \"<hi>\" && **not bold**\n```\nok";
        assert_eq!(to_telegram_html(markdown), "Run:\n<pre>\necho \"&lt;hi&gt;\" &amp;&amp; **not bold**</pre>\nok");
        assert_eq!(to_whatsapp(markdown), "Run:\n```\necho \"<hi>\" && **not bold**\n```\nok");
        // A block the model never closed is closed, so the message still parses.
        assert!(to_telegram_html("```\nlet x = 1;").ends_with("</pre>"));
        assert!(to_telegram_html("> open quote").ends_with("</blockquote>"));
    }

    #[test]
    fn links_are_written_out_for_whatsapp() {
        assert_eq!(to_whatsapp("[docs](https://e.com)"), "docs (https://e.com)");
        assert_eq!(to_whatsapp("[https://e.com](https://e.com)"), "https://e.com");
        assert_eq!(to_whatsapp("![chart](https://e.com/c.png)"), "chart (https://e.com/c.png)");
        assert_eq!(to_whatsapp("[not a link] (really)"), "[not a link] (really)");
    }
}
