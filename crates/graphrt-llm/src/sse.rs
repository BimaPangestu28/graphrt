//! Minimal Server-Sent Events parser.
//!
//! A transport utility used by the built-in providers, public so custom
//! [`crate::ChatModel`] implementations can reuse it.
//!
//! Incremental: feed arbitrary byte chunks with [`SseParser::push`], receive
//! complete events. Handles `\n`, `\r\n` and `\r` line endings (including a
//! `\r\n` split across chunks), multi-line `data:`, `event:`, `id:`, comments
//! and UTF-8 sequences split across chunks.
//!
//! Memory is bounded: a single line or event larger than the configured limit
//! (default [`DEFAULT_MAX_EVENT_BYTES`]) is an error instead of growing the
//! buffer without bound.

/// Default upper bound for one line or one event's accumulated data (16 MiB).
pub const DEFAULT_MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;

/// The stream exceeded the parser's size limit.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("server-sent event exceeds {limit} bytes")]
pub struct SseTooLarge {
    pub limit: usize,
}

/// One dispatched event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` field, if any (otherwise the default `message` type).
    pub event: Option<String>,
    /// Concatenated `data:` lines, joined with `\n`.
    pub data: String,
    pub id: Option<String>,
}

/// Incremental SSE parser.
#[derive(Debug)]
pub struct SseParser {
    line: Vec<u8>,
    skip_lf: bool,
    event: Option<String>,
    data: String,
    has_data: bool,
    id: Option<String>,
    max_bytes: usize,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::with_limit(DEFAULT_MAX_EVENT_BYTES)
    }
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parser rejecting lines or events larger than `max_bytes`.
    pub fn with_limit(max_bytes: usize) -> Self {
        Self {
            line: Vec::new(),
            skip_lf: false,
            event: None,
            data: String::new(),
            has_data: false,
            id: None,
            max_bytes,
        }
    }

    /// Feed bytes, returning every event completed by them.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, SseTooLarge> {
        let mut events = Vec::new();
        for &b in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if b == b'\n' {
                    continue;
                }
            }
            match b {
                b'\n' => self.end_line(&mut events)?,
                b'\r' => {
                    self.end_line(&mut events)?;
                    self.skip_lf = true;
                }
                other => {
                    if self.line.len() >= self.max_bytes {
                        return Err(SseTooLarge { limit: self.max_bytes });
                    }
                    self.line.push(other);
                }
            }
        }
        Ok(events)
    }

    /// Flush a final event not terminated by a blank line (end of stream).
    pub fn finish(&mut self) -> Result<Option<SseEvent>, SseTooLarge> {
        let mut events = Vec::new();
        if !self.line.is_empty() {
            self.end_line(&mut events)?;
        }
        self.dispatch(&mut events);
        Ok(events.pop())
    }

    fn end_line(&mut self, events: &mut Vec<SseEvent>) -> Result<(), SseTooLarge> {
        let raw = std::mem::take(&mut self.line);
        let line = String::from_utf8_lossy(&raw);
        if line.is_empty() {
            self.dispatch(events);
            return Ok(());
        }
        if line.starts_with(':') {
            return Ok(()); // comment / keep-alive
        }
        let (field, value) = match line.find(':') {
            Some(pos) => {
                let (f, rest) = line.split_at(pos);
                let v = rest.strip_prefix(':').unwrap_or(rest);
                (f, v.strip_prefix(' ').unwrap_or(v))
            }
            None => (line.as_ref(), ""),
        };
        match field {
            "data" => {
                if self.data.len().saturating_add(value.len()).saturating_add(1) > self.max_bytes {
                    return Err(SseTooLarge { limit: self.max_bytes });
                }
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            "event" => self.event = Some(value.to_owned()),
            "id" => self.id = Some(value.to_owned()),
            _ => {} // `retry` and unknown fields are ignored
        }
        Ok(())
    }

    fn dispatch(&mut self, events: &mut Vec<SseEvent>) {
        if self.has_data {
            events.push(SseEvent {
                event: self.event.take(),
                data: std::mem::take(&mut self.data),
                id: self.id.clone(),
            });
        } else {
            self.event = None;
        }
        self.has_data = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_events() {
        let mut p = SseParser::new();
        let ev = p.push(b"event: ping\ndata: {}\n\ndata: a\ndata: b\n\n").unwrap();
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].event.as_deref(), Some("ping"));
        assert_eq!(ev[0].data, "{}");
        assert_eq!(ev[1].event, None);
        assert_eq!(ev[1].data, "a\nb");
    }

    #[test]
    fn split_chunks_and_crlf() {
        let mut p = SseParser::new();
        let mut all = Vec::new();
        for chunk in [&b"da"[..], b"ta: hel", b"lo\r", b"\n\r", b"\n: comment\r\ndata:x\r\r"] {
            all.extend(p.push(chunk).unwrap());
        }
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].data, "hello");
        assert_eq!(all[1].data, "x");
    }

    #[test]
    fn utf8_split_across_chunks() {
        let text = "data: héllo\n\n".as_bytes();
        let mut p = SseParser::new();
        let mut all = p.push(&text[..8]).unwrap();
        all.extend(p.push(&text[8..]).unwrap());
        assert_eq!(all[0].data, "héllo");
    }

    #[test]
    fn event_without_data_is_not_dispatched_and_finish_flushes() {
        let mut p = SseParser::new();
        assert!(p.push(b"event: x\n\n").unwrap().is_empty());
        assert!(p.push(b"data: tail").unwrap().is_empty());
        assert_eq!(p.finish().unwrap().map(|e| e.data), Some("tail".to_owned()));
    }

    #[test]
    fn oversized_lines_and_events_are_rejected() {
        let mut p = SseParser::with_limit(16);
        assert_eq!(p.push(&[b'x'; 17]), Err(SseTooLarge { limit: 16 }));
        let mut p = SseParser::with_limit(16);
        assert!(p.push(b"data: 0123456789\n").is_ok());
        assert_eq!(p.push(b"data: 0123456789\n"), Err(SseTooLarge { limit: 16 }));
    }
}
