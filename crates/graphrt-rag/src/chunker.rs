//! Text chunking: Markdown-aware recursive splitting with overlap.
//!
//! The text is first divided into sections at Markdown headings (each chunk
//! remembers its heading path) and, with [`ChunkerConfig::split_pages`], at
//! page markers. Each section is then split recursively on progressively finer
//! separators (by default paragraphs, lines, sentences, words, and finally
//! characters) until every piece fits in `max_chars`. Pieces shorter than
//! `min_chars` are merged with a neighbour when the result still fits.
//! Adjacent chunks share up to `overlap` characters of context. All sizes count
//! Unicode scalar values, never bytes, so multi-byte text is never cut
//! mid-character.
//!
//! The separators can be replaced by an ordered list of literal strings and
//! (feature `regex`) regular expressions; splitting by character always
//! remains the last resort, so no chunk ever exceeds `max_chars`.

use std::ops::Range;

use serde::{Deserialize, Serialize};

use crate::error::RagError;
use crate::pages::{PAGE_MARKER_END, PAGE_MARKER_START};

/// Characters per token used by the token helpers (a common rule of thumb for
/// English and Indonesian text with BPE tokenizers).
pub const CHARS_PER_TOKEN: usize = 4;

/// Separators used when [`ChunkerConfig::separators`] is empty, coarse to fine:
/// paragraph, line, sentence ends, clause, word.
pub const DEFAULT_SEPARATORS: &[&str] = &["\n\n", "\n", ". ", "? ", "! ", "; ", ", ", " "];

/// Most separators one configuration may list.
pub const MAX_SEPARATORS: usize = 20;
/// Longest separator (literal or pattern), in characters.
pub const MAX_SEPARATOR_CHARS: usize = 200;
/// Compiled size limit of one regular-expression separator, in bytes.
#[cfg(feature = "regex")]
const REGEX_SIZE_LIMIT: usize = 256 * 1024;

/// Approximate token count of `text` ([`CHARS_PER_TOKEN`] characters per token, rounded up).
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(CHARS_PER_TOKEN)
}

/// One place where text may be split.
///
/// In YAML / JSON a literal is a plain string (`"\n\n"`) and a regular
/// expression is an object (`{ regex: "(?m)^Article \\d+" }`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Separator {
    /// Split after every occurrence of this text (it stays at the end of the piece before).
    Text(String),
    /// Split before every match of this pattern (the match starts the next
    /// piece), e.g. `(?m)^Article \d+` to keep each article together.
    /// Needs the `regex` feature.
    Regex {
        /// Pattern in the syntax of the `regex` crate (linear-time, no backtracking).
        regex: String,
    },
}

/// Chunking parameters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChunkerConfig {
    /// Maximum characters per chunk (excluding overlap). Roughly 4 characters per token.
    #[serde(default = "default_max")]
    pub max_chars: usize,
    /// Characters of the previous chunk repeated at the start of the next one
    /// (at most half of `max_chars`; larger values are reduced to that).
    #[serde(default = "default_overlap")]
    pub overlap: usize,
    /// Split at Markdown headings and record them as chunk metadata.
    #[serde(default = "yes")]
    pub markdown: bool,
    /// Parent-child mode: split the text into parents of up to this many characters,
    /// then each parent into children of `max_chars` (without overlap). Children are
    /// embedded and searched; retrieval returns their parent's text. Must be larger
    /// than `max_chars`.
    #[serde(default)]
    pub parent_max_chars: Option<usize>,
    /// Where to split, tried in order (coarse to fine). Empty means
    /// [`DEFAULT_SEPARATORS`]. Splitting by character is always tried last.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub separators: Vec<Separator>,
    /// Pieces shorter than this many characters are merged with the piece
    /// before or after them when the result fits in `max_chars` (0 = off).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub min_chars: usize,
    /// Never let a chunk span two pages: start a new chunk at every page marker
    /// (see [`crate::PAGE_MARKER_START`]). Text without markers is unaffected.
    #[serde(default, skip_serializing_if = "is_false")]
    pub split_pages: bool,
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
fn is_zero(value: &usize) -> bool {
    *value == 0
}
fn is_false(value: &bool) -> bool {
    !*value
}

impl Default for ChunkerConfig {
    fn default() -> Self {
        Self {
            max_chars: default_max(),
            overlap: default_overlap(),
            markdown: true,
            parent_max_chars: None,
            separators: Vec::new(),
            min_chars: 0,
            split_pages: false,
        }
    }
}

impl ChunkerConfig {
    /// Size limits expressed in approximate tokens ([`CHARS_PER_TOKEN`] characters per token).
    pub fn from_tokens(max_tokens: usize, overlap_tokens: usize) -> Self {
        Self {
            max_chars: max_tokens.saturating_mul(CHARS_PER_TOKEN),
            overlap: overlap_tokens.saturating_mul(CHARS_PER_TOKEN),
            ..Self::default()
        }
    }

    /// Checks the configuration: positive `max_chars`, `min_chars` not above
    /// it, a parent size larger than it, and usable separators (non-empty,
    /// at most [`MAX_SEPARATORS`] of at most [`MAX_SEPARATOR_CHARS`] characters,
    /// patterns that compile and never match empty text).
    ///
    /// # Errors
    /// [`RagError::Config`] naming the first problem.
    pub fn validate(&self) -> Result<(), RagError> {
        compile(self).map(|_| ())
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
    /// Index of the parent chunk in parent-child mode.
    pub parent: Option<usize>,
    /// Character offset where the chunk's own text starts (without the overlap
    /// prefix), counted in the text without page markers.
    pub start: usize,
    /// Character offset just after the chunk's own text (same coordinates as `start`).
    pub end: usize,
}

/// Split `text` into chunks (children carrying their parent index in parent-child mode).
///
/// Separators that are invalid (see [`ChunkerConfig::validate`]) are skipped;
/// use [`try_chunk_text`] to get the error instead.
pub fn chunk_text(text: &str, config: &ChunkerConfig) -> Vec<TextChunk> {
    let splitters = compile_lenient(config);
    chunk_compiled(text, config, &splitters)
}

/// Like [`chunk_text`], but rejects an invalid configuration.
///
/// # Errors
/// [`RagError::Config`] when [`ChunkerConfig::validate`] fails.
pub fn try_chunk_text(text: &str, config: &ChunkerConfig) -> Result<Vec<TextChunk>, RagError> {
    let splitters = compile(config)?;
    Ok(chunk_compiled(text, config, &splitters))
}

fn chunk_compiled(text: &str, config: &ChunkerConfig, splitters: &[Splitter]) -> Vec<TextChunk> {
    match config.parent_max_chars.filter(|&p| p > config.max_chars) {
        Some(parent_max) => chunk_with_parents(text, config, parent_max, splitters),
        None => chunk_flat(text, config, splitters),
    }
}

// ---------------------------------------------------------------------------
// Separators
// ---------------------------------------------------------------------------

/// A compiled separator.
#[derive(Debug)]
enum Splitter {
    /// Split after each occurrence.
    Text(String),
    /// Split before each match.
    #[cfg(feature = "regex")]
    Regex(regex::Regex),
}

fn compile(config: &ChunkerConfig) -> Result<Vec<Splitter>, RagError> {
    if config.max_chars == 0 {
        return Err(RagError::Config("`chunking.max_chars` must be positive".into()));
    }
    if config.min_chars > config.max_chars {
        return Err(RagError::Config("`chunking.min_chars` must not be larger than `max_chars`".into()));
    }
    if config.parent_max_chars.is_some_and(|p| p <= config.max_chars) {
        return Err(RagError::Config("`chunking.parent_max_chars` must be larger than `max_chars`".into()));
    }
    if config.separators.len() > MAX_SEPARATORS {
        return Err(RagError::Config(format!("`chunking.separators` lists more than {MAX_SEPARATORS} separators")));
    }
    if config.separators.is_empty() {
        return Ok(DEFAULT_SEPARATORS.iter().map(|s| Splitter::Text((*s).to_owned())).collect());
    }
    config.separators.iter().map(compile_one).collect()
}

/// Every valid separator; the defaults when none is valid.
fn compile_lenient(config: &ChunkerConfig) -> Vec<Splitter> {
    let compiled: Vec<Splitter> = config.separators.iter().filter_map(|s| compile_one(s).ok()).collect();
    if compiled.is_empty() {
        DEFAULT_SEPARATORS.iter().map(|s| Splitter::Text((*s).to_owned())).collect()
    } else {
        compiled
    }
}

fn compile_one(separator: &Separator) -> Result<Splitter, RagError> {
    let (value, what) = match separator {
        Separator::Text(text) => (text, "separator"),
        Separator::Regex { regex } => (regex, "separator pattern"),
    };
    if value.is_empty() {
        return Err(RagError::Config(format!("a {what} must not be empty")));
    }
    if value.chars().count() > MAX_SEPARATOR_CHARS {
        return Err(RagError::Config(format!("a {what} is longer than {MAX_SEPARATOR_CHARS} characters")));
    }
    match separator {
        Separator::Text(text) => Ok(Splitter::Text(text.clone())),
        Separator::Regex { regex } => compile_regex(regex),
    }
}

#[cfg(feature = "regex")]
fn compile_regex(pattern: &str) -> Result<Splitter, RagError> {
    let regex = regex::RegexBuilder::new(pattern)
        .size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|e| RagError::Config(format!("separator pattern `{pattern}` is not valid: {e}")))?;
    if regex.is_match("") {
        return Err(RagError::Config(format!("separator pattern `{pattern}` matches empty text")));
    }
    Ok(Splitter::Regex(regex))
}

#[cfg(not(feature = "regex"))]
fn compile_regex(pattern: &str) -> Result<Splitter, RagError> {
    Err(RagError::Config(format!(
        "separator pattern `{pattern}` needs graphrt-rag's `regex` feature, which this build leaves out"
    )))
}

impl Splitter {
    /// Byte positions (inside `text`, never 0 or `text.len()`) where `text` is cut.
    fn cut_points(&self, text: &str) -> Vec<usize> {
        match self {
            Splitter::Text(sep) => text
                .match_indices(sep.as_str())
                .map(|(at, matched)| at + matched.len())
                .filter(|&at| at < text.len())
                .collect(),
            #[cfg(feature = "regex")]
            Splitter::Regex(regex) => regex.find_iter(text).map(|m| m.start()).filter(|&at| at > 0).collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// Chunking
// ---------------------------------------------------------------------------

/// Parents of up to `parent_max` characters, each split into children of
/// `max_chars`. Children have no overlap, so joining a parent's children gives
/// back its text and the parent never needs to be stored separately.
fn chunk_with_parents(text: &str, config: &ChunkerConfig, parent_max: usize, splitters: &[Splitter]) -> Vec<TextChunk> {
    let parent_config = ChunkerConfig { max_chars: parent_max, overlap: 0, parent_max_chars: None, ..config.clone() };
    let child_config =
        ChunkerConfig { overlap: 0, markdown: false, split_pages: false, parent_max_chars: None, ..config.clone() };
    let mut children = Vec::new();
    for (parent_index, parent) in chunk_flat(text, &parent_config, splitters).into_iter().enumerate() {
        for child in chunk_flat(&parent.text, &child_config, splitters) {
            children.push(TextChunk {
                index: children.len(),
                text: child.text,
                heading: parent.heading.clone(),
                parent: Some(parent_index),
                start: parent.start + child.start,
                end: parent.start + child.end,
            });
        }
    }
    children
}

fn chunk_flat(text: &str, config: &ChunkerConfig, splitters: &[Splitter]) -> Vec<TextChunk> {
    let max = config.max_chars.max(1);
    let overlap = config.overlap.min(max / 2);
    let mut sections = if config.markdown { markdown_sections(text) } else { vec![(None, 0..text.len())] };
    if config.split_pages {
        sections = split_sections_at_pages(text, sections);
    }

    let mut cursor = CharCursor::default();
    let mut chunks = Vec::new();
    for (heading, range) in sections {
        let section = text.get(range.clone()).unwrap_or_default();
        let pieces: Vec<Range<usize>> = split_recursive(section, max, splitters)
            .into_iter()
            .filter_map(|piece| trim_range(section, piece))
            .collect();
        let pieces = merge_small(section, pieces, config.min_chars, max);
        let mut previous: Option<&str> = None;
        for piece in pieces {
            let own = section.get(piece.clone()).unwrap_or_default();
            let chunk_text = match (previous, overlap) {
                (Some(prev), n) if n > 0 => {
                    let tail = overlap_tail(prev, n);
                    if tail.is_empty() { own.to_owned() } else { format!("{tail} {own}") }
                }
                _ => own.to_owned(),
            };
            previous = Some(own);
            let start = cursor.visible_at(text, range.start + piece.start);
            let end = cursor.visible_at(text, range.start + piece.end);
            chunks.push(TextChunk {
                index: chunks.len(),
                text: chunk_text,
                heading: heading.clone(),
                parent: None,
                start,
                end,
            });
        }
    }
    chunks
}

/// The range without leading and trailing whitespace; `None` when nothing is left.
fn trim_range(text: &str, range: Range<usize>) -> Option<Range<usize>> {
    let piece = text.get(range.clone())?;
    let leading = piece.len() - piece.trim_start().len();
    let trimmed = piece.trim();
    if trimmed.is_empty() {
        return None;
    }
    let start = range.start + leading;
    Some(start..start + trimmed.len())
}

/// Merge pieces shorter than `min` characters into a neighbour when the merged
/// span (with the text between them) still fits in `max` characters.
fn merge_small(text: &str, pieces: Vec<Range<usize>>, min: usize, max: usize) -> Vec<Range<usize>> {
    if min == 0 {
        return pieces;
    }
    let len = |range: &Range<usize>| text.get(range.clone()).map_or(0, char_len);
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(pieces.len());
    for piece in pieces {
        if let Some(last) = merged.last_mut() {
            let small = len(last) < min || len(&piece) < min;
            if small && len(&(last.start..piece.end)) <= max {
                last.end = piece.end;
                continue;
            }
        }
        merged.push(piece);
    }
    merged
}

/// Converts byte positions (passed in ascending order) into character offsets
/// that skip page markers.
#[derive(Default)]
struct CharCursor {
    byte: usize,
    visible: usize,
    in_marker: bool,
}

impl CharCursor {
    fn visible_at(&mut self, text: &str, byte: usize) -> usize {
        if byte > self.byte {
            for c in text.get(self.byte..byte).unwrap_or_default().chars() {
                match c {
                    PAGE_MARKER_START => self.in_marker = true,
                    PAGE_MARKER_END => self.in_marker = false,
                    _ if !self.in_marker => self.visible += 1,
                    _ => {}
                }
            }
            self.byte = byte;
        }
        self.visible
    }
}

/// Divide Markdown text into `(heading path, byte range)` sections. Text before
/// the first heading has no heading. Headings inside fenced code blocks are
/// ignored; whitespace-only sections are dropped.
fn markdown_sections(text: &str) -> Vec<(Option<String>, Range<usize>)> {
    let mut sections: Vec<(Option<String>, Range<usize>)> = Vec::new();
    let mut path: Vec<(usize, String)> = Vec::new();
    let mut current_start = 0usize;
    let mut current_heading: Option<String> = None;
    let mut in_fence = false;
    let mut offset = 0usize;

    for line in text.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        }
        let level = if in_fence { 0 } else { heading_level(trimmed) };
        if level == 0 {
            continue;
        }
        let body = text.get(current_start..line_start).unwrap_or_default();
        if !body.trim().is_empty() {
            sections.push((current_heading.clone(), current_start..line_start));
        }
        let title = trimmed.get(level..).unwrap_or_default().trim().trim_end_matches('#').trim().to_owned();
        path.retain(|(l, _)| *l < level);
        path.push((level, title));
        current_heading = Some(path.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>().join(" > "));
        // The heading line stays in the body so chunks stay self-describing.
        current_start = line_start;
    }
    let body = text.get(current_start..).unwrap_or_default();
    if !body.trim().is_empty() {
        sections.push((current_heading, current_start..text.len()));
    }
    sections
}

/// Cut every section again before each page marker inside it.
fn split_sections_at_pages(
    text: &str,
    sections: Vec<(Option<String>, Range<usize>)>,
) -> Vec<(Option<String>, Range<usize>)> {
    let mut out = Vec::with_capacity(sections.len());
    for (heading, range) in sections {
        let section = text.get(range.clone()).unwrap_or_default();
        let mut start = range.start;
        for (at, _) in section.match_indices(PAGE_MARKER_START).filter(|(at, _)| *at > 0) {
            out.push((heading.clone(), start..range.start + at));
            start = range.start + at;
        }
        out.push((heading, start..range.end));
    }
    out
}

/// `#` count of an ATX heading line (1-6), or 0.
fn heading_level(line: &str) -> usize {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && line.chars().nth(hashes).is_some_and(char::is_whitespace) { hashes } else { 0 }
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// Split into byte ranges of at most `max` characters using the first separator
/// that occurs, merging small adjacent pieces greedily. The ranges cover `text`
/// completely and in order.
fn split_recursive(text: &str, max: usize, splitters: &[Splitter]) -> Vec<Range<usize>> {
    if char_len(text) <= max {
        let whole = 0..text.len();
        return vec![whole];
    }
    let Some((splitter, rest)) = splitters.split_first() else {
        return hard_split(text, max);
    };
    let cuts = splitter.cut_points(text);
    if cuts.is_empty() {
        return split_recursive(text, max, rest);
    }

    let bounds = std::iter::once(0).chain(cuts).chain(std::iter::once(text.len()));
    let mut out = Vec::new();
    let mut buffer: Option<Range<usize>> = None;
    // Running character count of `buffer`, so each part is measured once.
    let mut buffer_chars = 0usize;
    let mut previous_bound: Option<usize> = None;
    for bound in bounds {
        let Some(part_start) = previous_bound.replace(bound) else { continue };
        if bound <= part_start {
            continue;
        }
        let part = text.get(part_start..bound).unwrap_or_default();
        let part_chars = char_len(part);
        if part_chars > max {
            if let Some(full) = buffer.take() {
                out.push(full);
                buffer_chars = 0;
            }
            out.extend(split_recursive(part, max, rest).into_iter().map(|r| r.start + part_start..r.end + part_start));
            continue;
        }
        if buffer_chars + part_chars > max
            && let Some(full) = buffer.take()
        {
            out.push(full);
            buffer_chars = 0;
        }
        buffer = Some(match buffer {
            Some(open) => open.start..bound,
            None => part_start..bound,
        });
        buffer_chars += part_chars;
    }
    if let Some(full) = buffer {
        out.push(full);
    }
    out
}

fn hard_split(text: &str, max: usize) -> Vec<Range<usize>> {
    let max = max.max(1);
    let mut out = Vec::new();
    let mut start = 0usize;
    for (count, (at, _)) in text.char_indices().enumerate() {
        if count > 0 && count % max == 0 {
            out.push(start..at);
            start = at;
        }
    }
    if start < text.len() {
        out.push(start..text.len());
    }
    out
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

    fn flat(max_chars: usize, overlap: usize) -> ChunkerConfig {
        ChunkerConfig { max_chars, overlap, markdown: false, ..ChunkerConfig::default() }
    }

    /// The text without page markers, as character offsets see it.
    fn visible(text: &str) -> Vec<char> {
        crate::strip_page_markers(text).chars().collect()
    }

    fn own_text(text: &str, chunk: &TextChunk) -> String {
        visible(text).get(chunk.start..chunk.end).unwrap_or_default().iter().collect()
    }

    #[test]
    fn respects_max_size_and_keeps_text() {
        let text = "Alpha beta gamma. Delta epsilon zeta. Eta theta iota. Kappa lambda mu.".repeat(20);
        let chunks = chunk_text(&text, &flat(100, 0));
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
        let chunks = chunk_text(text, &flat(25, 10));
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
        let chunks = chunk_text(&text, &flat(30, 5));
        assert!(chunks.len() > 10);
        assert!(chunks.iter().all(|c| char_len(&c.text) <= 36));
    }

    #[test]
    fn token_helpers() {
        let cfg = ChunkerConfig::from_tokens(256, 32);
        assert_eq!(cfg.max_chars, 1024);
        assert_eq!(cfg.overlap, 128);
        assert!(cfg.markdown);
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(estimate_tokens("日本語の"), 1);
    }

    #[test]
    fn parent_child_mode_splits_parents_into_children_without_overlap() {
        let paragraph =
            |topic: &str| format!("{topic} first sentence here. {topic} second sentence here. {topic} third one.");
        let text = format!("{}\n\n{}\n\n{}", paragraph("Alpha"), paragraph("Beta"), paragraph("Gamma"));
        let cfg = ChunkerConfig { parent_max_chars: Some(90), ..flat(40, 30) };
        let chunks = chunk_text(&text, &cfg);
        assert!(chunks.iter().all(|c| c.parent.is_some()), "every chunk is a child");
        assert!(chunks.iter().all(|c| c.text.chars().count() <= 40), "children fit max_chars");
        assert!(chunks.windows(2).all(|w| w[0].index + 1 == w[1].index), "indexes are sequential");
        let parents: std::collections::BTreeSet<usize> = chunks.iter().filter_map(|c| c.parent).collect();
        assert!(parents.len() >= 3, "{parents:?}");
        // No overlap: every word of the text appears once across children.
        let words = |s: &str| s.split_whitespace().count();
        assert_eq!(chunks.iter().map(|c| words(&c.text)).sum::<usize>(), words(&text));
        // A parent's children never mix topics from different paragraphs.
        for p in parents {
            let joined: Vec<&str> = chunks.iter().filter(|c| c.parent == Some(p)).map(|c| c.text.as_str()).collect();
            let joined = joined.join(" ");
            let topics = ["Alpha", "Beta", "Gamma"].iter().filter(|t| joined.contains(*t)).count();
            assert_eq!(topics, 1, "parent {p} mixes paragraphs: {joined}");
        }
        // Children offsets point at their own text in the document.
        for chunk in &chunks {
            assert_eq!(own_text(&text, chunk), chunk.text);
        }
    }

    #[test]
    fn parent_size_not_above_child_size_means_flat_chunks() {
        let cfg = ChunkerConfig { parent_max_chars: Some(50), ..flat(50, 0) };
        assert!(chunk_text("one two three four five six seven", &cfg).iter().all(|c| c.parent.is_none()));
    }

    #[test]
    fn offsets_locate_each_chunk_without_its_overlap() {
        let text = "# Título\n\nPrimera frase aquí. Segunda frase más larga aquí.\n\nOtro párrafo con más palabras.";
        let chunks = chunk_text(text, &ChunkerConfig { max_chars: 30, overlap: 10, ..ChunkerConfig::default() });
        assert!(chunks.len() >= 3);
        for chunk in &chunks {
            let own = own_text(text, chunk);
            assert!(!own.is_empty());
            assert!(chunk.text.ends_with(&own), "{:?} should end with {own:?}", chunk.text);
        }
        assert!(chunks.windows(2).all(|w| w[0].end <= w[1].start), "offsets ascend without overlap");
    }

    #[test]
    fn offsets_skip_page_markers() {
        let marker = |n: u32| format!("{PAGE_MARKER_START}{n}{PAGE_MARKER_END}");
        let text = format!("{}Page one text.\n\n{}Page two text.", marker(1), marker(2));
        let cfg = ChunkerConfig { split_pages: true, ..flat(1000, 0) };
        let chunks = chunk_text(&text, &cfg);
        assert_eq!(chunks.len(), 2, "{chunks:?}");
        assert_eq!(own_text(&text, &chunks[0]), "Page one text.");
        assert_eq!(own_text(&text, &chunks[1]), "Page two text.");
        // Without split_pages both pages fit in one chunk.
        assert_eq!(chunk_text(&text, &flat(1000, 0)).len(), 1);
    }

    #[test]
    fn custom_literal_separators_are_used_in_order() {
        let text = "a1 a2 a3 | b1 b2 b3 | c1 c2 c3";
        let cfg = ChunkerConfig { separators: vec![Separator::Text(" | ".into())], ..flat(12, 0) };
        let texts: Vec<String> = chunk_text(text, &cfg).into_iter().map(|c| c.text).collect();
        assert_eq!(texts, vec!["a1 a2 a3 |", "b1 b2 b3 |", "c1 c2 c3"]);
    }

    #[test]
    fn character_split_stays_the_last_resort() {
        let cfg = ChunkerConfig { separators: vec![Separator::Text("|".into())], ..flat(4, 0) };
        let chunks = chunk_text("abcdefghij", &cfg);
        assert_eq!(chunks.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(), vec!["abcd", "efgh", "ij"]);
    }

    #[cfg(feature = "regex")]
    #[test]
    fn regex_separators_split_before_each_match() {
        let text = "Pasal 1 Ketentuan umum berlaku. Pasal 2 Hak dan kewajiban. Pasal 3 Penutup.";
        let cfg = ChunkerConfig { separators: vec![Separator::Regex { regex: r"Pasal \d+".into() }], ..flat(40, 0) };
        let texts: Vec<String> = try_chunk_text(text, &cfg).unwrap_or_default().into_iter().map(|c| c.text).collect();
        assert_eq!(texts, vec!["Pasal 1 Ketentuan umum berlaku.", "Pasal 2 Hak dan kewajiban.", "Pasal 3 Penutup."]);
    }

    #[test]
    fn min_chars_merges_short_pieces_when_they_fit() {
        // The first paragraph is too long and leaves a short remainder, which the
        // greedy split never joins with the next paragraph.
        let text = "aaaa bbbb cccc dddd eeee ffff gggg hhhh iiii jjjj kkkk\n\nzzzz yyyy xxxx";
        let texts = |cfg: ChunkerConfig| chunk_text(text, &cfg).into_iter().map(|c| c.text).collect::<Vec<_>>();
        assert_eq!(
            texts(flat(40, 0)),
            vec!["aaaa bbbb cccc dddd eeee ffff gggg hhhh", "iiii jjjj kkkk", "zzzz yyyy xxxx"]
        );
        let merged = chunk_text(text, &ChunkerConfig { min_chars: 15, ..flat(40, 0) });
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[1].text, "iiii jjjj kkkk\n\nzzzz yyyy xxxx");
        assert_eq!(own_text(text, &merged[1]), merged[1].text);
        // Never above max_chars, even when that leaves a short piece.
        let tight = chunk_text(text, &ChunkerConfig { min_chars: 15, ..flat(20, 0) });
        assert!(tight.iter().all(|c| char_len(&c.text) <= 20));
    }

    #[test]
    fn validation_rejects_unusable_settings() {
        let check = |cfg: ChunkerConfig| cfg.validate().is_err();
        assert!(check(ChunkerConfig { max_chars: 0, ..ChunkerConfig::default() }));
        assert!(check(ChunkerConfig { min_chars: 2000, ..ChunkerConfig::default() }));
        assert!(check(ChunkerConfig { parent_max_chars: Some(500), ..ChunkerConfig::default() }));
        assert!(check(ChunkerConfig { separators: vec![Separator::Text(String::new())], ..ChunkerConfig::default() }));
        assert!(check(ChunkerConfig {
            separators: vec![Separator::Text("x".into()); MAX_SEPARATORS + 1],
            ..ChunkerConfig::default()
        }));
        assert!(check(ChunkerConfig {
            separators: vec![Separator::Text("x".repeat(MAX_SEPARATOR_CHARS + 1))],
            ..ChunkerConfig::default()
        }));
        assert!(check(ChunkerConfig {
            separators: vec![Separator::Regex { regex: "(".into() }],
            ..ChunkerConfig::default()
        }));
        assert!(check(ChunkerConfig {
            separators: vec![Separator::Regex { regex: "x*".into() }],
            ..ChunkerConfig::default()
        }));
        assert!(ChunkerConfig::default().validate().is_ok());
        assert!(try_chunk_text("text", &ChunkerConfig { max_chars: 0, ..ChunkerConfig::default() }).is_err());
    }

    #[test]
    fn invalid_separators_are_skipped_by_chunk_text() {
        let cfg = ChunkerConfig { separators: vec![Separator::Text(String::new())], ..flat(10, 0) };
        let chunks = chunk_text("one two three four five", &cfg);
        assert!(chunks.len() >= 2);
        assert!(chunks.iter().all(|c| char_len(&c.text) <= 10));
    }

    #[test]
    fn config_round_trips_through_json() {
        let cfg = ChunkerConfig {
            separators: vec![Separator::Text("\n\n".into()), Separator::Regex { regex: r"(?m)^\d+\.".into() }],
            min_chars: 50,
            split_pages: true,
            ..ChunkerConfig::default()
        };
        let json = serde_json::to_value(&cfg).unwrap_or_default();
        assert_eq!(json["separators"][1]["regex"], r"(?m)^\d+\.");
        let back: ChunkerConfig = serde_json::from_value(json).unwrap_or_default();
        assert_eq!(back, cfg);
        // Older configs without the new keys keep their meaning.
        let old: ChunkerConfig = serde_json::from_str(r#"{"max_chars": 400}"#).unwrap_or_default();
        assert_eq!(old, ChunkerConfig { max_chars: 400, ..ChunkerConfig::default() });
        // Defaults are not written out.
        let written = serde_json::to_value(ChunkerConfig::default()).unwrap_or_default();
        assert!(written.get("separators").is_none() && written.get("min_chars").is_none());
    }
}
