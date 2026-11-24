//! Server-sent events: an incremental parser for agent streams and the
//! encoder for client streams.
//!
//! The parser is bounded: one event may not exceed [`MAX_EVENT_BYTES`]
//! (a larger one fails the stream instead of growing the buffer), which with
//! the bounded per-run channel caps gateway memory per stream (plan D10).

/// Largest single event accepted from an agent.
pub const MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;

/// A partial-line buffer that grew past this is freed once the line is done
/// instead of kept for the rest of the stream.
const SHRINK_ABOVE: usize = 16 * 1024;

/// One SSE event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// `event:` field (`message` when absent).
    pub event: String,
    /// `id:` field.
    pub id: Option<String>,
    /// `data:` lines joined with `\n`.
    pub data: String,
}

impl Frame {
    /// The engine event index when `id` is an integer (`response` chunks are `N.k`).
    pub fn event_index(&self) -> Option<u64> {
        self.id.as_deref().filter(|id| !id.contains('.')).and_then(|id| id.parse().ok())
    }

    /// Encodes the frame for a client stream.
    pub fn encode(&self) -> String {
        let mut out = String::with_capacity(self.data.len() + 48);
        if self.event != "message" {
            out.push_str("event: ");
            out.push_str(&self.event);
            out.push('\n');
        }
        if let Some(id) = &self.id {
            out.push_str("id: ");
            out.push_str(id);
            out.push('\n');
        }
        for line in self.data.split('\n') {
            out.push_str("data: ");
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
        out
    }
}

/// Parse failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SseError {
    /// An event exceeded [`MAX_EVENT_BYTES`].
    #[error("SSE event larger than {MAX_EVENT_BYTES} bytes")]
    TooLarge,
}

/// Incremental parser: feed bytes, take frames.
#[derive(Debug, Default)]
pub struct Parser {
    buffer: Vec<u8>,
    event: Option<String>,
    id: Option<String>,
    data: Vec<String>,
    data_bytes: usize,
}

impl Parser {
    /// Feeds a chunk and returns the frames it completed.
    ///
    /// # Errors
    /// [`SseError::TooLarge`].
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Frame>, SseError> {
        let mut frames = std::collections::VecDeque::new();
        self.feed_into(chunk, &mut frames)?;
        Ok(frames.into())
    }

    /// Feeds a chunk and appends the frames it completed to `frames`.
    ///
    /// Lines are parsed straight out of `chunk`; only a trailing partial
    /// line is copied (into the parser's own buffer, until its end arrives).
    ///
    /// # Errors
    /// [`SseError::TooLarge`].
    pub fn feed_into(&mut self, chunk: &[u8], frames: &mut std::collections::VecDeque<Frame>) -> Result<(), SseError> {
        let mut rest = chunk;
        while let Some(end) = rest.iter().position(|byte| *byte == b'\n') {
            let (line, tail) = rest.split_at(end);
            rest = tail.get(1..).unwrap_or_default();
            if self.buffer.is_empty() {
                self.raw_line(line, frames)?;
            } else {
                let mut joined = std::mem::take(&mut self.buffer);
                joined.extend_from_slice(line);
                self.raw_line(&joined, frames)?;
                joined.clear();
                if joined.capacity() <= SHRINK_ABOVE {
                    // Reuse the allocation for the next partial line.
                    self.buffer = joined;
                }
            }
        }
        if self.buffer.len() + rest.len() + self.data_bytes > MAX_EVENT_BYTES {
            return Err(SseError::TooLarge);
        }
        self.buffer.extend_from_slice(rest);
        Ok(())
    }

    fn raw_line(&mut self, line: &[u8], frames: &mut std::collections::VecDeque<Frame>) -> Result<(), SseError> {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let line = String::from_utf8_lossy(line);
        if let Some(frame) = self.line(&line)? {
            frames.push_back(frame);
        }
        Ok(())
    }

    fn line(&mut self, line: &str) -> Result<Option<Frame>, SseError> {
        if line.is_empty() {
            if self.data.is_empty() && self.event.is_none() {
                self.id = None;
                return Ok(None);
            }
            let frame = Frame {
                event: self.event.take().unwrap_or_else(|| "message".to_owned()),
                id: self.id.take(),
                data: self.data.join("\n"),
            };
            self.data.clear();
            self.data_bytes = 0;
            return Ok(Some(frame));
        }
        if line.starts_with(':') {
            return Ok(None);
        }
        let (field, value) =
            line.split_once(':').map_or((line, ""), |(field, value)| (field, value.strip_prefix(' ').unwrap_or(value)));
        match field {
            "event" => self.event = Some(value.to_owned()),
            "id" => self.id = Some(value.to_owned()),
            "data" => {
                self.data_bytes += value.len() + 1;
                if self.data_bytes > MAX_EVENT_BYTES {
                    return Err(SseError::TooLarge);
                }
                self.data.push(value.to_owned());
            }
            _ => {}
        }
        Ok(None)
    }
}

/// The first event index a client with `Last-Event-ID: last` still needs
/// (same scheme as `graphrt serve`: `N` → `N+1`, `N.k` → `N`), and how many
/// response chunks of that index it already has.
pub fn resume_point(last: &str) -> Option<(u64, Option<u64>)> {
    let last = last.trim();
    match last.split_once('.') {
        None => last.parse::<u64>().ok().map(|index| (index.saturating_add(1), None)),
        Some((index, chunk)) => Some((index.parse().ok()?, Some(chunk.parse().ok()?))),
    }
}

/// Whether frame `id` is after the client's resume point.
pub fn is_after(id: &str, start: u64, chunks_seen: Option<u64>) -> bool {
    match id.split_once('.') {
        None => id.parse::<u64>().is_ok_and(|index| index >= start),
        Some((index, chunk)) => match (index.parse::<u64>(), chunk.parse::<u64>()) {
            (Ok(index), Ok(chunk)) => index > start || (index == start && chunks_seen.is_none_or(|seen| chunk > seen)),
            _ => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_across_chunk_boundaries() {
        let mut parser = Parser::default();
        assert!(parser.feed(b"event: hello\ndata: {\"run_id\"").unwrap().is_empty());
        let frames =
            parser.feed(b":\"r\"}\n\n: heartbeat\n\nid: 3\nevent: node_started\r\ndata: a\ndata: b\n\n").unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], Frame { event: "hello".into(), id: None, data: "{\"run_id\":\"r\"}".into() });
        assert_eq!(frames[1].event_index(), Some(3));
        assert_eq!(frames[1].data, "a\nb");
        assert_eq!(Parser::default().feed(frames[1].encode().as_bytes()).unwrap(), vec![frames[1].clone()]);
    }

    #[test]
    fn same_frames_at_every_split_point() {
        let input = b"event: a\r\nid: 1\ndata: x\ndata: y\n\n: beat\n\nevent: b\ndata: \xff\n\n";
        let whole = Parser::default().feed(input).unwrap();
        assert_eq!(whole.len(), 2);
        assert_eq!(whole[1].data, "\u{fffd}");
        for split in 0..input.len() {
            let mut parser = Parser::default();
            let mut frames = parser.feed(&input[..split]).unwrap();
            frames.extend(parser.feed(&input[split..]).unwrap());
            assert_eq!(frames, whole, "split at {split}");
        }
        // Byte by byte.
        let mut parser = Parser::default();
        let frames: Vec<Frame> = input.iter().flat_map(|byte| parser.feed(&[*byte]).unwrap()).collect();
        assert_eq!(frames, whole);
    }

    #[test]
    fn rejects_oversized_events() {
        let mut parser = Parser::default();
        let big = vec![b'x'; MAX_EVENT_BYTES + 10];
        assert_eq!(parser.feed(&big), Err(SseError::TooLarge));
    }

    #[test]
    fn resume_points() {
        assert_eq!(resume_point("7"), Some((8, None)));
        assert_eq!(resume_point("7.2"), Some((7, Some(2))));
        assert!(is_after("8", 8, None));
        assert!(!is_after("7", 8, None));
        assert!(is_after("7.3", 7, Some(2)));
        assert!(!is_after("7.2", 7, Some(2)));
        assert!(is_after("7", 7, Some(2)));
    }
}
