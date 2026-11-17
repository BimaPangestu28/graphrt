//! Minimal Server-Sent Events parser.
//!
//! Incremental: feed arbitrary byte chunks with [`SseParser::push`], receive
//! complete events. Handles `\n`, `\r\n` and `\r` line endings (including a
//! `\r\n` split across chunks), multi-line `data:`, `event:`, `id:`, comments
//! and UTF-8 sequences split across chunks.

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
#[derive(Debug, Default)]
pub struct SseParser {
    line: Vec<u8>,
    skip_lf: bool,
    event: Option<String>,
    data: String,
    has_data: bool,
    id: Option<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes, returning every event completed by them.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        let mut events = Vec::new();
        for &b in bytes {
            if self.skip_lf {
                self.skip_lf = false;
                if b == b'\n' {
                    continue;
                }
            }
            match b {
                b'\n' => self.end_line(&mut events),
                b'\r' => {
                    self.end_line(&mut events);
                    self.skip_lf = true;
                }
                other => self.line.push(other),
            }
        }
        events
    }

    /// Flush a final event not terminated by a blank line (end of stream).
    pub fn finish(&mut self) -> Option<SseEvent> {
        let mut events = Vec::new();
        if !self.line.is_empty() {
            self.end_line(&mut events);
        }
        self.dispatch(&mut events);
        events.pop()
    }

    fn end_line(&mut self, events: &mut Vec<SseEvent>) {
        let raw = std::mem::take(&mut self.line);
        let line = String::from_utf8_lossy(&raw);
        if line.is_empty() {
            self.dispatch(events);
            return;
        }
        if line.starts_with(':') {
            return; // comment / keep-alive
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
        let ev = p.push(b"event: ping\ndata: {}\n\ndata: a\ndata: b\n\n");
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
            all.extend(p.push(chunk));
        }
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].data, "hello");
        assert_eq!(all[1].data, "x");
    }

    #[test]
    fn utf8_split_across_chunks() {
        let text = "data: héllo\n\n".as_bytes();
        let mut p = SseParser::new();
        let mut all = p.push(&text[..8]);
        all.extend(p.push(&text[8..]));
        assert_eq!(all[0].data, "héllo");
    }

    #[test]
    fn event_without_data_is_not_dispatched_and_finish_flushes() {
        let mut p = SseParser::new();
        assert!(p.push(b"event: x\n\n").is_empty());
        assert!(p.push(b"data: tail").is_empty());
        assert_eq!(p.finish().map(|e| e.data), Some("tail".to_owned()));
    }
}
