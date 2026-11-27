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

/// What a search looks for in a line.
#[derive(Clone, Debug)]
pub(crate) enum Matcher {
    /// A text. `needle` is already lowercased when `case_sensitive` is off.
    Literal { needle: String, case_sensitive: bool },
    /// A regular expression (linear time: no backtracking).
    #[cfg(feature = "regex")]
    Pattern(regex::Regex),
}

impl Matcher {
    /// A text to find.
    pub(crate) fn literal(query: &str, case_sensitive: bool) -> Self {
        let needle = if case_sensitive { query.to_owned() } else { query.to_lowercase() };
        Matcher::Literal { needle, case_sensitive }
    }

    /// A regular expression to find. The compiled program and its matching
    /// cache are bounded, so a pattern cannot ask for unbounded memory, and
    /// the engine matches in time linear in the text whatever the pattern is.
    ///
    /// @throws String - A message for the model: a syntax error, or a pattern that is too large.
    #[cfg(feature = "regex")]
    pub(crate) fn pattern(query: &str, case_sensitive: bool) -> Result<Self, String> {
        regex::RegexBuilder::new(query)
            .case_insensitive(!case_sensitive)
            .size_limit(MAX_PATTERN_PROGRAM_BYTES)
            .dfa_size_limit(MAX_PATTERN_CACHE_BYTES)
            .nest_limit(MAX_PATTERN_NESTING)
            .build()
            .map(Matcher::Pattern)
            .map_err(|error| match error {
                regex::Error::CompiledTooBig(_) => {
                    "the regular expression is too large: use smaller repetition counts or a simpler pattern".to_owned()
                }
                other => {
                    // The last line of the report says what is wrong, without the picture above it.
                    let report = other.to_string();
                    let reason = report.lines().last().unwrap_or_default().trim_start_matches("error: ");
                    format!("invalid regular expression: {}", reason.chars().take(200).collect::<String>())
                }
            })
    }

    /// Byte position and length of the first match in `line`.
    fn find(&self, line: &str) -> Option<(usize, usize)> {
        match self {
            Matcher::Literal { needle, case_sensitive: true } => {
                line.find(needle.as_str()).map(|at| (at, needle.len()))
            }
            Matcher::Literal { needle, case_sensitive: false } => {
                line.to_lowercase().find(needle.as_str()).map(|at| (at, needle.len()))
            }
            #[cfg(feature = "regex")]
            Matcher::Pattern(pattern) => pattern.find(line).map(|found| (found.start(), found.len())),
        }
    }
}

/// Largest compiled program of a pattern.
#[cfg(feature = "regex")]
const MAX_PATTERN_PROGRAM_BYTES: usize = 1024 * 1024;
/// Largest matching cache of a pattern.
#[cfg(feature = "regex")]
const MAX_PATTERN_CACHE_BYTES: usize = 2 * 1024 * 1024;
/// Deepest nesting of groups in a pattern.
#[cfg(feature = "regex")]
const MAX_PATTERN_NESTING: u32 = 50;
/// A step at least this large is matched on the blocking pool.
const BLOCKING_STEP_BYTES: usize = 64 * 1024;
/// The clock is read once per this many lines.
const LINES_PER_CLOCK_CHECK: usize = 256;

/// How far one search may go.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SearchBudget {
    pub max_matches: usize,
    pub max_scan_bytes: u64,
    pub snippet_chars: usize,
    /// When the search has to be over, whatever is left.
    pub deadline: Option<std::time::Instant>,
}

/// What a search of one file found.
#[derive(Debug, Default)]
pub(crate) struct Found {
    pub matches: Vec<Match>,
    /// Bytes looked at.
    pub scanned: u64,
    /// The deadline passed before the end of the file.
    pub timed_out: bool,
}

/// Matches lines as they arrive, and knows when to stop.
struct Scanner {
    matcher: std::sync::Arc<Matcher>,
    budget: SearchBudget,
    line_number: usize,
    matches: Vec<Match>,
    timed_out: bool,
}

impl Scanner {
    fn new(matcher: std::sync::Arc<Matcher>, budget: SearchBudget) -> Self {
        Self { matcher, budget, line_number: 0, matches: Vec::new(), timed_out: false }
    }

    fn out_of_time(&mut self) -> bool {
        if self.budget.deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            self.timed_out = true;
        }
        self.timed_out
    }

    /// Look at one line. Returns whether to go on.
    fn line(&mut self, line: &str) -> bool {
        self.line_number += 1;
        if self.line_number.is_multiple_of(LINES_PER_CLOCK_CHECK) && self.out_of_time() {
            return false;
        }
        if let Some((position, length)) = self.matcher.find(line) {
            let text = snippet(line, position, length, self.budget.snippet_chars);
            self.matches.push(Match { line: self.line_number, text });
        }
        self.matches.len() < self.budget.max_matches
    }
}

/// Lines of a text file that `matcher` finds, within `budget`.
///
/// The file is read in steps. A large step is matched on the blocking pool,
/// so a slow pattern over a large file does not hold an async worker.
pub(crate) async fn search_file(
    workspace: &Workspace,
    path: &WorkspacePath,
    matcher: &std::sync::Arc<Matcher>,
    budget: SearchBudget,
) -> Result<Found, FilesError> {
    let mut state = (LineSplitter::new(), Scanner::new(std::sync::Arc::clone(matcher), budget));
    let mut offset = 0u64;
    let mut go_on = !state.1.out_of_time();
    while go_on && offset < budget.max_scan_bytes {
        let step = STEP_BYTES.min(budget.max_scan_bytes - offset);
        let chunk = workspace.read(path, offset, step).await?;
        if chunk.is_empty() {
            break;
        }
        let first = offset == 0;
        let large = chunk.len() >= BLOCKING_STEP_BYTES;
        offset = offset.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        let feed = move |(mut splitter, mut scanner): (LineSplitter, Scanner)| {
            // A byte order mark is not part of the first line.
            let bytes = if first { chunk.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&chunk) } else { &chunk };
            let go_on = !scanner.out_of_time()
                && splitter.push(bytes, &mut |line: &[u8]| scanner.line(&String::from_utf8_lossy(line)));
            ((splitter, scanner), go_on)
        };
        (state, go_on) = if large {
            let moved = state;
            crate::blocking(move || feed(moved))
                .await
                .map_err(|message| crate::store::StoreError::io(path.as_str(), message))?
        } else {
            feed(state)
        };
    }
    let (mut splitter, mut scanner) = state;
    if go_on {
        splitter.finish(&mut |line: &[u8]| scanner.line(&String::from_utf8_lossy(line)));
    }
    Ok(Found { matches: scanner.matches, scanned: offset, timed_out: scanner.timed_out })
}

/// The same search over text that is already in memory (extracted documents).
pub(crate) fn search_text(text: &str, matcher: &std::sync::Arc<Matcher>, budget: SearchBudget) -> Found {
    let mut scanner = Scanner::new(std::sync::Arc::clone(matcher), budget);
    if !scanner.out_of_time() {
        for line in text.lines() {
            // An endless line is cut like in a file.
            let line = match line.char_indices().nth(MAX_LINE_BYTES) {
                Some((cut, _)) => line.get(..cut).unwrap_or(line),
                None => line,
            };
            if !scanner.line(line) {
                break;
            }
        }
    }
    Found {
        matches: scanner.matches,
        scanned: u64::try_from(text.len()).unwrap_or(u64::MAX),
        timed_out: scanner.timed_out,
    }
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

    fn budget(max_matches: usize) -> SearchBudget {
        SearchBudget { max_matches, max_scan_bytes: 1 << 20, snippet_chars: 200, deadline: None }
    }

    async fn lines_found(ws: &Workspace, matcher: Matcher, max_matches: usize) -> Vec<usize> {
        let found = search_file(ws, &path("a.md"), &std::sync::Arc::new(matcher), budget(max_matches)).await.unwrap();
        found.matches.iter().map(|found| found.line).collect()
    }

    #[tokio::test]
    async fn search_finds_lines_case_insensitively_and_stops_at_the_cap() {
        let ws = workspace_with("a.md", "# Invoice\nTotal: 120 EUR\nÉCOLE fee\ntotal again\n".as_bytes()).await;
        assert_eq!(lines_found(&ws, Matcher::literal("total", false), 10).await, [2, 4]);
        assert_eq!(lines_found(&ws, Matcher::literal("Total", true), 10).await, [2]);
        assert_eq!(lines_found(&ws, Matcher::literal("école", false), 10).await, [3]);
        assert_eq!(lines_found(&ws, Matcher::literal("total", false), 1).await, [2]);
    }

    #[cfg(feature = "regex")]
    #[tokio::test]
    async fn patterns_match_lines_and_are_bounded_when_they_are_built() {
        let ws =
            workspace_with("a.md", "# Invoice INV-0042\nTotal: 120 EUR\nref inv-7\ntotal again\n".as_bytes()).await;
        assert_eq!(lines_found(&ws, Matcher::pattern(r"inv-\d{4}", false).unwrap(), 10).await, [1]);
        assert_eq!(lines_found(&ws, Matcher::pattern(r"inv-\d+", true).unwrap(), 10).await, [3]);
        assert_eq!(lines_found(&ws, Matcher::pattern(r"^total|EUR$", false).unwrap(), 10).await, [2, 4]);
        // A pattern is one line of text to the model, whatever is wrong with it.
        let invalid = Matcher::pattern("(unclosed", false).unwrap_err();
        assert!(invalid.starts_with("invalid regular expression: ") && !invalid.contains('\n'), "{invalid}");
        // Repetition that would compile into a huge program, and nesting without end, are refused.
        for hostile in [r"(\w{1,500}){500}", r"((a{100}){100}){100}", &format!("{}a{}", "(".repeat(60), ")".repeat(60))]
        {
            let refused = Matcher::pattern(hostile, false).unwrap_err();
            assert!(refused.contains("too large") || refused.contains("invalid regular expression"), "{refused}");
        }
        // Features that need backtracking do not exist in this engine.
        assert!(Matcher::pattern(r"(a)\1", false).is_err());
        assert!(Matcher::pattern(r"a(?=b)", false).is_err());
    }

    #[test]
    fn a_search_stops_at_its_deadline() {
        let text = "needle\n".repeat(10_000);
        let matcher = std::sync::Arc::new(Matcher::literal("needle", true));
        let past = std::time::Instant::now().checked_sub(std::time::Duration::from_secs(1));
        let late = SearchBudget { max_matches: usize::MAX, deadline: past, ..budget(0) };
        let found = search_text(&text, &matcher, late);
        assert!(found.timed_out && found.matches.is_empty());
        let in_time = search_text(&text, &matcher, SearchBudget { max_matches: usize::MAX, ..budget(0) });
        assert_eq!((in_time.matches.len(), in_time.timed_out), (10_000, false));
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
