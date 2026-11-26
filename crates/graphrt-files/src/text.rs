//! Reading and searching text without loading whole files.
//!
//! Both walk a file in fixed steps through [`Workspace::read`], keep only the
//! unfinished last line between steps, and stop as soon as they have what was
//! asked for. Memory stays at one step however large the file is.

use crate::path::WorkspacePath;
use crate::workspace::{FilesError, Workspace};

/// Bytes read from the store per step.
pub(crate) const STEP_BYTES: u64 = 256 * 1024;
/// A line longer than this is cut (the rest of it is skipped).
const MAX_LINE_BYTES: usize = 16 * 1024;

/// A window of lines of a text file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LineWindow {
    /// The lines, without their line endings.
    pub lines: Vec<String>,
    /// 1-based number of the first line returned.
    pub first_line: usize,
    /// Whether the file has lines after the window.
    pub more: bool,
}

/// Feeds complete lines of a byte stream to a callback.
struct LineSplitter {
    carry: Vec<u8>,
    /// The current line already passed [`MAX_LINE_BYTES`]: drop bytes until its end.
    skipping: bool,
}

impl LineSplitter {
    fn new() -> Self {
        Self { carry: Vec::new(), skipping: false }
    }

    /// Push a chunk; `on_line` returns `false` to stop early. Returns whether to go on.
    fn push(&mut self, chunk: &[u8], on_line: &mut dyn FnMut(&[u8]) -> bool) -> bool {
        let mut rest = chunk;
        while let Some(newline) = rest.iter().position(|byte| *byte == b'\n') {
            let (head, tail) = rest.split_at(newline);
            rest = tail.get(1..).unwrap_or_default();
            let keep_going = if self.skipping {
                self.skipping = false;
                self.carry.clear();
                true
            } else if self.carry.is_empty() {
                on_line(trim_carriage_return(head))
            } else {
                self.carry.extend_from_slice(head);
                let line = std::mem::take(&mut self.carry);
                on_line(trim_carriage_return(&line))
            };
            if !keep_going {
                return false;
            }
        }
        if !self.skipping {
            self.carry.extend_from_slice(rest);
            if self.carry.len() > MAX_LINE_BYTES {
                self.carry.truncate(MAX_LINE_BYTES);
                let line = std::mem::take(&mut self.carry);
                self.skipping = true;
                return on_line(&line);
            }
        }
        true
    }

    /// The last line when the stream does not end with a line ending.
    fn finish(&mut self, on_line: &mut dyn FnMut(&[u8]) -> bool) {
        if !self.skipping && !self.carry.is_empty() {
            let line = std::mem::take(&mut self.carry);
            on_line(trim_carriage_return(&line));
        }
    }
}

fn trim_carriage_return(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Call `on_line` for every line of the file until it returns `false`.
/// Reads at most `max_scan_bytes`. Returns the bytes read.
async fn for_each_line(
    workspace: &Workspace,
    path: &WorkspacePath,
    max_scan_bytes: u64,
    on_line: &mut (dyn FnMut(&[u8]) -> bool + Send),
) -> Result<u64, FilesError> {
    let mut splitter = LineSplitter::new();
    let mut offset = 0u64;
    while offset < max_scan_bytes {
        let step = STEP_BYTES.min(max_scan_bytes - offset);
        let chunk = workspace.read(path, offset, step).await?;
        if chunk.is_empty() {
            break;
        }
        offset = offset.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        // A byte order mark is not part of the first line.
        let chunk = if offset == u64::try_from(chunk.len()).unwrap_or(u64::MAX) {
            chunk.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&chunk)
        } else {
            &chunk
        };
        if !splitter.push(chunk, on_line) {
            return Ok(offset);
        }
    }
    splitter.finish(on_line);
    Ok(offset)
}

/// Lines `first_line..first_line + max_lines` of a text file (1-based), cut
/// when they pass `max_chars` in total.
pub(crate) async fn read_lines(
    workspace: &Workspace,
    path: &WorkspacePath,
    first_line: usize,
    max_lines: usize,
    max_chars: usize,
) -> Result<LineWindow, FilesError> {
    let first_line = first_line.max(1);
    let mut window = LineWindow { first_line, ..LineWindow::default() };
    let mut line_number = 0usize;
    let mut chars = 0usize;
    let limit = workspace.limits().max_file_bytes;
    let mut on_line = |line: &[u8]| {
        line_number += 1;
        if line_number < first_line {
            return true;
        }
        if window.lines.len() >= max_lines || chars >= max_chars {
            window.more = true;
            return false;
        }
        let text = String::from_utf8_lossy(line);
        chars += text.chars().count() + 1;
        window.lines.push(text.into_owned());
        true
    };
    for_each_line(workspace, path, limit, &mut on_line).await?;
    Ok(window)
}

/// The same window over text that is already in memory (extracted documents).
pub(crate) fn window_of(text: &str, first_line: usize, max_lines: usize, max_chars: usize) -> LineWindow {
    let first_line = first_line.max(1);
    let mut window = LineWindow { first_line, ..LineWindow::default() };
    let mut chars = 0usize;
    for line in text.lines().skip(first_line - 1) {
        if window.lines.len() >= max_lines || chars >= max_chars {
            window.more = true;
            break;
        }
        chars += line.chars().count() + 1;
        window.lines.push(line.to_owned());
    }
    window
}

/// One line that contains the search text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Match {
    pub line: usize,
    pub text: String,
}

/// Lines of a text file containing `needle` (already lowercased when
/// `case_sensitive` is off), at most `max_matches`. Returns the matches and
/// the bytes scanned.
pub(crate) async fn search_file(
    workspace: &Workspace,
    path: &WorkspacePath,
    needle: &str,
    case_sensitive: bool,
    max_matches: usize,
    max_scan_bytes: u64,
    snippet_chars: usize,
) -> Result<(Vec<Match>, u64), FilesError> {
    let mut matches = Vec::new();
    let mut line_number = 0usize;
    let mut on_line = |line: &[u8]| {
        line_number += 1;
        let text = String::from_utf8_lossy(line);
        let found = if case_sensitive { text.find(needle) } else { text.to_lowercase().find(needle) };
        if let Some(position) = found {
            matches.push(Match { line: line_number, text: snippet(&text, position, needle.len(), snippet_chars) });
        }
        matches.len() < max_matches
    };
    let scanned = for_each_line(workspace, path, max_scan_bytes, &mut on_line).await?;
    Ok((matches, scanned))
}

/// At most `max_chars` characters of `line` around the match at byte `position`.
fn snippet(line: &str, position: usize, needle_len: usize, max_chars: usize) -> String {
    let line = line.trim();
    if line.chars().count() <= max_chars {
        return line.to_owned();
    }
    // Lowercasing can move byte offsets a little: only use them as a hint.
    let before = max_chars.saturating_sub(needle_len) / 2;
    let match_char = line.char_indices().take_while(|(index, _)| *index < position).count();
    let start = match_char.saturating_sub(before);
    let cut: String = line.chars().skip(start).take(max_chars).collect();
    let prefix = if start > 0 { "..." } else { "" };
    format!("{prefix}{cut}...")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::WorkspaceLimits;

    fn path(raw: &str) -> WorkspacePath {
        WorkspacePath::parse(raw).unwrap()
    }

    async fn workspace_with(name: &str, content: &[u8]) -> Workspace {
        let ws = Workspace::in_memory(WorkspaceLimits::default());
        ws.write(&path(name), content.to_vec(), false).await.unwrap();
        ws
    }

    fn split(chunks: &[&[u8]]) -> Vec<String> {
        let mut lines = Vec::new();
        let mut splitter = LineSplitter::new();
        let mut on_line = |line: &[u8]| {
            lines.push(String::from_utf8_lossy(line).into_owned());
            true
        };
        for chunk in chunks {
            splitter.push(chunk, &mut on_line);
        }
        splitter.finish(&mut on_line);
        lines
    }

    #[test]
    fn lines_survive_any_chunk_boundary() {
        let text = b"alpha\r\nbeta\n\ngamma";
        let expected = vec!["alpha", "beta", "", "gamma"];
        assert_eq!(split(&[text]), expected);
        for cut in 1..text.len() {
            let (a, b) = text.split_at(cut);
            assert_eq!(split(&[a, b]), expected, "cut at {cut}");
        }
        assert_eq!(split(&[b"one\n"]), vec!["one"]);
        assert!(split(&[b""]).is_empty());
    }

    #[test]
    fn an_endless_line_is_cut_not_buffered() {
        let long = vec![b'x'; MAX_LINE_BYTES * 3];
        let lines = split(&[&long, b"\nnext\n"]);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), MAX_LINE_BYTES);
        assert_eq!(lines[1], "next");
    }

    #[tokio::test]
    async fn windows_page_through_a_file() {
        let content: String = (1..=10).map(|n| format!("line {n}\n")).collect();
        let ws = workspace_with("a.txt", content.as_bytes()).await;
        let first = read_lines(&ws, &path("a.txt"), 1, 4, 10_000).await.unwrap();
        assert_eq!(first.lines, ["line 1", "line 2", "line 3", "line 4"]);
        assert!(first.more);
        let last = read_lines(&ws, &path("a.txt"), 9, 4, 10_000).await.unwrap();
        assert_eq!(
            (last.lines.as_slice(), last.first_line, last.more),
            (&["line 9".to_owned(), "line 10".to_owned()][..], 9, false)
        );
        assert!(read_lines(&ws, &path("a.txt"), 50, 4, 10_000).await.unwrap().lines.is_empty());
        // The character budget cuts the window too.
        let tight = read_lines(&ws, &path("a.txt"), 1, 100, 10).await.unwrap();
        assert_eq!(tight.lines.len(), 2);
        assert!(tight.more);
        assert_eq!(window_of(&content, 9, 4, 10_000), last);
    }

    #[tokio::test]
    async fn a_file_larger_than_one_step_is_read_in_steps() {
        let line = "0123456789abcdef\n";
        let count = usize::try_from(STEP_BYTES).unwrap() * 2 / line.len() + 10;
        let ws = workspace_with("big.txt", line.repeat(count).as_bytes()).await;
        let window = read_lines(&ws, &path("big.txt"), count - 1, 5, 10_000).await.unwrap();
        assert_eq!(window.lines.len(), 2);
        assert!(!window.more);
    }

    #[tokio::test]
    async fn search_finds_lines_case_insensitively_and_stops_at_the_cap() {
        let ws = workspace_with("a.md", "# Invoice\nTotal: 120 EUR\nÉCOLE fee\ntotal again\n".as_bytes()).await;
        let (found, _) = search_file(&ws, &path("a.md"), "total", false, 10, 1 << 20, 200).await.unwrap();
        assert_eq!(found.iter().map(|m| m.line).collect::<Vec<_>>(), [2, 4]);
        let (exact, _) = search_file(&ws, &path("a.md"), "Total", true, 10, 1 << 20, 200).await.unwrap();
        assert_eq!(exact.len(), 1);
        let (unicode, _) = search_file(&ws, &path("a.md"), "école", false, 10, 1 << 20, 200).await.unwrap();
        assert_eq!(unicode[0].line, 3);
        let (capped, _) = search_file(&ws, &path("a.md"), "total", false, 1, 1 << 20, 200).await.unwrap();
        assert_eq!(capped.len(), 1);
    }

    #[test]
    fn snippets_are_cut_around_the_match() {
        let line = format!("{}needle{}", "a".repeat(300), "b".repeat(300));
        let cut = snippet(&line, 300, 6, 40);
        assert!(cut.contains("needle"), "{cut}");
        assert!(cut.chars().count() <= 46);
        assert_eq!(snippet("  short  ", 2, 5, 40), "short");
    }
}
