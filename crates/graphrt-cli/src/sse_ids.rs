//! SSE `id:` scheme of `graphrt serve` (protocol `Graphrt-Protocol: 1`).
//!
//! - Every engine event carries `id: <index>`, its index in the run's event log
//!   (the same index as `GET /v1/runs/{id}/events?from=<index>`), so ids keep
//!   counting across resumes of the same run.
//! - A `response` event (assembled answer text) is not stored; it carries
//!   `id: <index>.<k>`, where `<index>` is the engine event it was assembled from
//!   and `k` counts from 0. Response events go out just before that engine event.
//! - Synthetic events (`lagged`, `hello`) carry no id, so a client's
//!   `Last-Event-ID` always names a real event.
//!
//! A client that reconnects with `Last-Event-ID: N` gets events from `N + 1`;
//! with `N.k` (it saw some of event `N`'s response text but not event `N`
//! itself) it gets events from `N`.

/// The `id:` of the engine event at `index`.
pub fn event_id(index: u64) -> String {
    index.to_string()
}

/// The `id:` of the `k`-th `response` event assembled from the event at `index`.
pub fn chunk_id(index: u64, k: usize) -> String {
    format!("{index}.{k}")
}

/// The first event index to send to a client whose `Last-Event-ID` is `last`;
/// `None` when the id is not one this server sends.
///
/// @example
/// assert_eq!(resume_index("7"), Some(8));
/// assert_eq!(resume_index("7.2"), Some(7));
pub fn resume_index(last: &str) -> Option<u64> {
    let last = last.trim();
    match last.split_once('.') {
        None => last.parse::<u64>().ok().map(|index| index.saturating_add(1)),
        Some((index, chunk)) => {
            chunk.parse::<u64>().ok()?;
            index.parse().ok()
        }
    }
}

/// [`resume_index`] of a request's `Last-Event-ID` header, if it has a valid one.
pub fn resume_index_of(headers: &axum::http::HeaderMap) -> Option<u64> {
    last_event_id(headers).and_then(resume_index)
}

fn last_event_id(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers.get("last-event-id").and_then(|value| value.to_str().ok())
}

/// Where a (re)connected stream of events and response text starts: the first
/// event index to send, and how many of that event's `response` events the
/// client already has (`None`: all of them are sent).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub start: u64,
    /// Response events `<start>.0 ..= <start>.<n>` are skipped.
    pub skip_chunks: Option<usize>,
}

impl Cursor {
    /// Everything, from the first event.
    pub const ALL: Cursor = Cursor { start: 0, skip_chunks: None };

    /// From event `index` (and its response text) on.
    pub fn from_index(index: u64) -> Self {
        Self { start: index, skip_chunks: None }
    }

    /// Right after the item a client last saw (`Last-Event-ID`).
    pub fn after(last: &str) -> Option<Self> {
        let last = last.trim();
        match last.split_once('.') {
            None => last.parse::<u64>().ok().map(|index| Self::from_index(index.saturating_add(1))),
            Some((index, chunk)) => Some(Self { start: index.parse().ok()?, skip_chunks: Some(chunk.parse().ok()?) }),
        }
    }

    /// Whether the event at `index` (or its `chunk`-th response event) is sent.
    pub fn admits(&self, index: u64, chunk: Option<usize>) -> bool {
        match (index.cmp(&self.start), chunk, self.skip_chunks) {
            (std::cmp::Ordering::Less, _, _) => false,
            (std::cmp::Ordering::Equal, Some(k), Some(seen)) => k > seen,
            _ => true,
        }
    }
}

/// The [`Cursor`] of a request's `Last-Event-ID` header, if it has a valid one.
pub fn cursor_of(headers: &axum::http::HeaderMap) -> Option<Cursor> {
    last_event_id(headers).and_then(Cursor::after)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_event_ids_resume_after_the_named_event() {
        assert_eq!(resume_index("0"), Some(1));
        assert_eq!(resume_index(" 41 "), Some(42));
        assert_eq!(resume_index("41.0"), Some(41));
        assert_eq!(resume_index("41.17"), Some(41));
        for bad in ["", "x", "-1", "1.", ".1", "1.x", "1.2.3"] {
            assert_eq!(resume_index(bad), None, "{bad}");
        }
        assert_eq!(resume_index(&event_id(9)), Some(10));
        assert_eq!(resume_index(&chunk_id(9, 3)), Some(9));
    }

    #[test]
    fn cursors_skip_exactly_what_the_client_has() {
        let after_event = Cursor::after("4").unwrap();
        assert!(!after_event.admits(4, None) && !after_event.admits(4, Some(0)));
        assert!(after_event.admits(5, Some(0)) && after_event.admits(5, None));
        let mid_text = Cursor::after("4.1").unwrap();
        assert!(!mid_text.admits(4, Some(0)) && !mid_text.admits(4, Some(1)) && !mid_text.admits(3, None));
        assert!(mid_text.admits(4, Some(2)) && mid_text.admits(4, None) && mid_text.admits(5, Some(0)));
        assert!(Cursor::ALL.admits(0, Some(0)) && Cursor::ALL.admits(0, None));
        assert_eq!(Cursor::after("x"), None);
        assert_eq!(Cursor::after("1.x"), None);
    }
}
