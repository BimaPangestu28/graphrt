//! Page numbers of chunks, read from the page markers `graphrt-doc` writes into
//! PDF text (`Options::page_markers`, on in [`crate::DocExtractor`]).
//!
//! A marker is [`PAGE_MARKER_START`], the page number in decimal and
//! [`PAGE_MARKER_END`], placed right before the page's text. The [`Ingestor`]
//! removes markers from every chunk before hashing, embedding and storing it,
//! and records the pages the chunk's text comes from as metadata:
//! [`PAGE_KEY`] (the first page) and, when the chunk spans several pages,
//! [`PAGES_KEY`] (every page, ascending). Text without markers is stored
//! exactly as before and gets no page metadata.
//!
//! [`Ingestor`]: crate::Ingestor

/// First character of a page marker (Unicode private use, never in real text).
pub const PAGE_MARKER_START: char = '\u{E000}';
/// Last character of a page marker.
pub const PAGE_MARKER_END: char = '\u{E001}';
/// Chunk metadata: first page (1-based) the chunk's text comes from.
pub const PAGE_KEY: &str = "page";
/// Chunk metadata: every page the chunk's text comes from, when more than one.
pub const PAGES_KEY: &str = "pages";

/// `text` without page markers (and without stray marker characters).
pub fn strip_page_markers(text: &str) -> String {
    if !text.contains([PAGE_MARKER_START, PAGE_MARKER_END]) {
        return text.to_owned();
    }
    split_markers(text).filter_map(|piece| piece.text()).collect()
}

/// One piece of marked-up text.
#[derive(Debug, PartialEq, Eq)]
enum Piece<'a> {
    Text(&'a str),
    /// A well-formed marker with its page number.
    Marker(u32),
    /// Marker characters that do not form a marker (dropped).
    Junk,
}

impl<'a> Piece<'a> {
    fn text(&self) -> Option<&'a str> {
        match self {
            Piece::Text(text) => Some(text),
            Piece::Marker(_) | Piece::Junk => None,
        }
    }
}

fn split_markers(text: &str) -> impl Iterator<Item = Piece<'_>> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let Some(start) = rest.find([PAGE_MARKER_START, PAGE_MARKER_END]) else {
            let text = rest;
            rest = "";
            return Some(Piece::Text(text));
        };
        if start > 0 {
            let (text, tail) = rest.split_at(start);
            rest = tail;
            return Some(Piece::Text(text));
        }
        let mut chars = rest.chars();
        let first = chars.next()?;
        let after = chars.as_str();
        let marker = (first == PAGE_MARKER_START)
            .then(|| {
                let end = after.find(PAGE_MARKER_END)?;
                let number = after.get(..end)?.parse::<u32>().ok()?;
                Some((end, number))
            })
            .flatten();
        if let Some((end, number)) = marker {
            rest = after.get(end + PAGE_MARKER_END.len_utf8()..).unwrap_or_default();
            return Some(Piece::Marker(number));
        }
        rest = after;
        Some(Piece::Junk)
    })
}

/// Follows the page across a document's chunks, which must be passed in order.
///
/// Chunks may repeat the end of the previous chunk (overlap), so a chunk can
/// begin with the text before a marker already seen: that text belongs to the
/// page before it.
#[derive(Debug, Default)]
pub(crate) struct PageTracker {
    /// Page of the text after the last marker seen.
    current: Option<u32>,
    /// Page before `current`.
    previous: Option<u32>,
}

impl PageTracker {
    /// The pages the chunk's text comes from (ascending, no repeats), and its
    /// text without markers (`None` when it had none).
    pub(crate) fn take(&mut self, chunk_text: &str) -> (Option<String>, Vec<u32>) {
        if !chunk_text.contains([PAGE_MARKER_START, PAGE_MARKER_END]) {
            let pages = if chunk_text.trim().is_empty() { Vec::new() } else { self.current.into_iter().collect() };
            return (None, pages);
        }
        let first_marker = split_markers(chunk_text).find_map(|piece| match piece {
            Piece::Marker(number) => Some(number),
            Piece::Text(_) | Piece::Junk => None,
        });
        // Overlap repeated the previous chunk's last marker: what precedes it is the page before.
        let mut page =
            if first_marker.is_some() && first_marker == self.current { self.previous } else { self.current };
        let mut text = String::with_capacity(chunk_text.len());
        let mut pages: Vec<u32> = Vec::new();
        for piece in split_markers(chunk_text) {
            match piece {
                Piece::Text(segment) => {
                    if let Some(number) = page.filter(|_| !segment.trim().is_empty()) {
                        pages.push(number);
                    }
                    text.push_str(segment);
                }
                Piece::Marker(number) => {
                    if Some(number) != self.current {
                        self.previous = self.current;
                        self.current = Some(number);
                    }
                    page = Some(number);
                }
                Piece::Junk => {}
            }
        }
        pages.sort_unstable();
        pages.dedup();
        (Some(text), pages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker(number: u32) -> String {
        format!("{PAGE_MARKER_START}{number}{PAGE_MARKER_END}")
    }

    #[cfg(feature = "doc")]
    #[test]
    fn markers_match_graphrt_doc() {
        assert_eq!(PAGE_MARKER_START, graphrt_doc::PAGE_MARKER_START);
        assert_eq!(PAGE_MARKER_END, graphrt_doc::PAGE_MARKER_END);
        assert_eq!(marker(12), graphrt_doc::page_marker(12));
    }

    #[test]
    fn strips_markers_and_junk() {
        let text = format!("{}Hours{}\n\n{}Fees {PAGE_MARKER_END}x", marker(1), PAGE_MARKER_START, marker(3));
        assert_eq!(strip_page_markers(&text), "Hours\n\nFees x");
        assert_eq!(strip_page_markers("plain"), "plain");
    }

    #[test]
    fn tracks_pages_across_chunks_with_overlap() {
        let mut tracker = PageTracker::default();
        let first = format!("{}Opening hours are nine to five.", marker(1));
        assert_eq!(tracker.take(&first), (Some("Opening hours are nine to five.".to_owned()), vec![1]));
        // A chunk crossing into page 3 (page 2 had no text).
        let second = format!("Closed on Sunday.\n\n{}Delivery fees depend", marker(3));
        assert_eq!(tracker.take(&second), (Some("Closed on Sunday.\n\nDelivery fees depend".to_owned()), vec![1, 3]));
        // Overlap repeats the page 3 marker: the text before it is still page 1.
        let third = format!("Sunday.\n\n{}Delivery fees depend on distance.", marker(3));
        assert_eq!(tracker.take(&third).1, vec![1, 3]);
        // No marker: the current page.
        assert_eq!(tracker.take("Free above 50 boxes.").1, vec![3]);
    }

    #[test]
    fn text_without_markers_has_no_pages() {
        let mut tracker = PageTracker::default();
        assert_eq!(tracker.take("Just text"), (None, Vec::new()));
    }
}
