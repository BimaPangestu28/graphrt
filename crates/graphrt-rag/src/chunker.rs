//! Text chunking: Markdown-aware recursive splitting with overlap.
//!
//! The text is first divided into sections at Markdown headings (each chunk
//! remembers its heading path), then each section is split recursively on
//! progressively finer separators (paragraphs, lines, sentences, words, and
//! finally characters) until every piece fits in `max_chars`. Adjacent chunks
//! share up to `overlap` characters of context. All sizes count Unicode scalar
//! values, never bytes, so multi-byte text is never cut mid-character.

use serde::{Deserialize, Serialize};

/// Chunking parameters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChunkerConfig {
    /// Maximum characters per chunk (excluding overlap). Roughly 4 characters per token.
    #[serde(default = "default_max")]
    pub max_chars: usize,
    /// Characters of the previous chunk repeated at the start of the next one.
    #[serde(default = "default_overlap")]
    pub overlap: usize,
    /// Split at Markdown headings and record them as chunk metadata.
    #[serde(default = "yes")]
    pub markdown: bool,
}

fn default_max() -> usize {
    1000
}
fn default_overlap() -> usize {
    100
}
fn yes() -> bool {
    true
}

impl Default for ChunkerConfig {
    fn default() -> Self {
        Self { max_chars: default_max(), overlap: default_overlap(), markdown: true }
    }
}

impl ChunkerConfig {
    /// Size limits expressed in approximate tokens (1 token ≈ 4 characters).
    pub fn from_tokens(max_tokens: usize, overlap_tokens: usize) -> Self {
        Self { max_chars: max_tokens.saturating_mul(4), overlap: overlap_tokens.saturating_mul(4), markdown: true }
    }
}

/// One chunk of a document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextChunk {
    /// Position of the chunk in the document (0-based).
    pub index: usize,
    pub text: String,
    /// Heading path, e.g. `Guide > Install`, when the chunk is under a Markdown heading.
    pub heading: Option<String>,
}

/// Separators tried in order, coarse to fine. The empty separator means "split by character".
const SEPARATORS: &[&str] = &["\n\n", "\n", ". ", "? ", "! ", "; ", ", ", " ", ""];

/// Split `text` into chunks.
pub fn chunk_text(text: &str, config: &ChunkerConfig) -> Vec<TextChunk> {
    let max = config.max_chars.max(1);
    let overlap = config.overlap.min(max / 2);
    let sections = if config.markdown { markdown_sections(text) } else { vec![(None, text.to_owned())] };

    let mut chunks = Vec::new();
    for (heading, body) in sections {
        let pieces = split_recursive(&body, max, SEPARATORS);
        let mut previous: Option<String> = None;
        for piece in pieces {
            let trimmed = piece.trim();
            if trimmed.is_empty() {
                continue;
            }
            let text = match (&previous, overlap) {
                (Some(prev), n) if n > 0 => {
                    let tail = overlap_tail(prev, n);
                    if tail.is_empty() { trimmed.to_owned() } else { format!("{tail} {trimmed}") }
                }
                _ => trimmed.to_owned(),
            };
            previous = Some(trimmed.to_owned());
            chunks.push(TextChunk { index: chunks.len(), text, heading: heading.clone() });
        }
    }
    chunks
}

/// Divide Markdown text into `(heading path, body)` sections. Text before the first
/// heading has no heading. Headings inside fenced code blocks are ignored.
fn markdown_sections(text: &str) -> Vec<(Option<String>, String)> {
    let mut sections: Vec<(Option<String>, String)> = Vec::new();
    let mut path: Vec<(usize, String)> = Vec::new();
    let mut current = String::new();
    let mut current_heading: Option<String> = None;
    let mut in_fence = false;

    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        }
        let level = if in_fence { 0 } else { heading_level(trimmed) };
        if level > 0 {
            if !current.trim().is_empty() {
                sections.push((current_heading.clone(), std::mem::take(&mut current)));
            } else {
                current.clear();
            }
            let title = trimmed.get(level..).unwrap_or_default().trim().trim_end_matches('#').trim().to_owned();
            path.retain(|(l, _)| *l < level);
            path.push((level, title));
            current_heading = Some(path.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>().join(" > "));
            // Keep the heading line in the body so chunks stay self-describing.
            current.push_str(line);
        } else {
            current.push_str(line);
        }
    }
    if !current.trim().is_empty() {
        sections.push((current_heading, current));
    }
    sections
}

/// `#` count of an ATX heading line (1-6), or 0.
fn heading_level(line: &str) -> usize {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && line.chars().nth(hashes).is_some_and(char::is_whitespace) { hashes } else { 0 }
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// Split into pieces of at most `max` characters using the first separator that
/// occurs, merging small adjacent pieces greedily.
fn split_recursive(text: &str, max: usize, separators: &[&str]) -> Vec<String> {
    if char_len(text) <= max {
        return vec![text.to_owned()];
    }
    let Some((sep, rest)) = separators.split_first() else {
        return hard_split(text, max);
    };
    if sep.is_empty() {
        return hard_split(text, max);
    }
    if !text.contains(sep) {
        return split_recursive(text, max, rest);
    }

    // Keep the separator attached to the end of each piece.
    let parts: Vec<&str> = text.split_inclusive(sep).collect();
    let mut out = Vec::new();
    let mut buffer = String::new();
    // Running character count of `buffer`, so each part is measured once.
    let mut buffer_chars = 0usize;
    for part in parts {
        let part_chars = char_len(part);
        if part_chars > max {
            if !buffer.is_empty() {
                out.push(std::mem::take(&mut buffer));
                buffer_chars = 0;
            }
            out.extend(split_recursive(part, max, rest));
            continue;
        }
        if buffer_chars + part_chars > max && !buffer.is_empty() {
            out.push(std::mem::take(&mut buffer));
            buffer_chars = 0;
        }
        buffer.push_str(part);
        buffer_chars += part_chars;
    }
    if !buffer.is_empty() {
        out.push(buffer);
    }
    out
}

fn hard_split(text: &str, max: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars.chunks(max.max(1)).map(|c| c.iter().collect()).collect()
}

/// The last `n` characters of `text`, starting at a word boundary when possible.
fn overlap_tail(text: &str, n: usize) -> String {
    let total = char_len(text);
    if total <= n {
        return text.to_owned();
    }
    let tail: String = text.chars().skip(total - n).collect();
    match tail.find(char::is_whitespace) {
        Some(pos) if pos + 1 < tail.len() => tail.get(pos..).unwrap_or_default().trim_start().to_owned(),
        _ => tail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn respects_max_size_and_keeps_text() {
        let text = "Alpha beta gamma. Delta epsilon zeta. Eta theta iota. Kappa lambda mu.".repeat(20);
        let cfg = ChunkerConfig { max_chars: 100, overlap: 0, markdown: false };
        let chunks = chunk_text(&text, &cfg);
        assert!(chunks.len() > 5);
        for c in &chunks {
            assert!(char_len(&c.text) <= 100, "{}", c.text.len());
        }
        let joined: String = chunks.iter().map(|c| c.text.as_str()).collect::<Vec<_>>().join(" ");
        assert_eq!(joined.split_whitespace().count(), text.split_whitespace().count());
    }

    #[test]
    fn overlap_repeats_previous_tail() {
        let text = "one two three four five six seven eight nine ten eleven twelve";
        let cfg = ChunkerConfig { max_chars: 25, overlap: 10, markdown: false };
        let chunks = chunk_text(text, &cfg);
        assert!(chunks.len() >= 2);
        let second = &chunks[1].text;
        let tail = overlap_tail(&chunks[0].text, 10);
        assert!(!tail.is_empty());
        assert!(chunks[0].text.ends_with(&tail));
        assert!(second.starts_with(&tail), "{second:?} vs {:?}", chunks[0].text);
    }

    #[test]
    fn markdown_headings_become_metadata() {
        let text = "Intro text.\n\n# Guide\n\nRead me.\n\n## Install\n\nRun it.\n\n```\n# not a heading\n```\n\n# Faq\nQ and A.\n";
        let chunks = chunk_text(text, &ChunkerConfig::default());
        let headings: Vec<Option<&str>> = chunks.iter().map(|c| c.heading.as_deref()).collect();
        assert_eq!(headings, vec![None, Some("Guide"), Some("Guide > Install"), Some("Faq")]);
        assert!(chunks[2].text.contains("# not a heading"));
    }

    #[test]
    fn multibyte_text_is_split_on_char_boundaries() {
        let text = "日本語のテキストです。".repeat(50);
        let cfg = ChunkerConfig { max_chars: 30, overlap: 5, markdown: false };
        let chunks = chunk_text(&text, &cfg);
        assert!(chunks.len() > 10);
        assert!(chunks.iter().all(|c| char_len(&c.text) <= 36));
    }

    #[test]
    fn token_helper() {
        let cfg = ChunkerConfig::from_tokens(256, 32);
        assert_eq!(cfg.max_chars, 1024);
        assert_eq!(cfg.overlap, 128);
    }
}
