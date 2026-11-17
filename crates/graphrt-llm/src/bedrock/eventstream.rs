//! `application/vnd.amazon.eventstream` binary framing.
//!
//! Each message: total length (u32 BE), headers length (u32 BE), prelude CRC32,
//! headers, payload, message CRC32. Both CRCs are verified.

use crate::error::LlmError;

/// Largest accepted message (the service limit is far below this).
pub(crate) const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const PRELUDE: usize = 12;
const MIN_MESSAGE: usize = PRELUDE + 4;

/// A decoded message. Only string-valued headers are kept.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Message {
    pub headers: Vec<(String, String)>,
    pub payload: Vec<u8>,
}

impl Message {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

// `i < 256` always holds, so the index cannot panic.
#[allow(clippy::indexing_slicing)]
const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 == 1 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static CRC_TABLE: [u32; 256] = crc_table();

/// CRC-32 (IEEE 802.3), as used by the event stream format.
pub(crate) fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for b in bytes {
        let idx = usize::from((crc as u8) ^ b);
        crc = CRC_TABLE.get(idx).copied().unwrap_or(0) ^ (crc >> 8);
    }
    !crc
}

fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 4)?;
    Some(u32::from_be_bytes([*slice.first()?, *slice.get(1)?, *slice.get(2)?, *slice.get(3)?]))
}

fn be_u16(bytes: &[u8], at: usize) -> Option<u16> {
    let slice = bytes.get(at..at + 2)?;
    Some(u16::from_be_bytes([*slice.first()?, *slice.get(1)?]))
}

fn malformed(what: &str) -> LlmError {
    LlmError::Decode(format!("malformed event stream message: {what}"))
}

/// Parse the header block.
fn parse_headers(mut bytes: &[u8]) -> Result<Vec<(String, String)>, LlmError> {
    let mut out = Vec::new();
    while !bytes.is_empty() {
        let name_len = usize::from(*bytes.first().ok_or_else(|| malformed("header name"))?);
        let name = bytes.get(1..1 + name_len).ok_or_else(|| malformed("header name"))?;
        let name = String::from_utf8_lossy(name).into_owned();
        let kind = *bytes.get(1 + name_len).ok_or_else(|| malformed("header type"))?;
        let value_start = 2 + name_len;
        let (value, used) = match kind {
            0 | 1 => (None, 0),
            2 => (None, 1),
            3 => (None, 2),
            4 => (None, 4),
            5 | 8 => (None, 8),
            9 => (None, 16),
            6 | 7 => {
                let len = usize::from(be_u16(bytes, value_start).ok_or_else(|| malformed("header value length"))?);
                let raw = bytes.get(value_start + 2..value_start + 2 + len).ok_or_else(|| malformed("header value"))?;
                let value = if kind == 7 { Some(String::from_utf8_lossy(raw).into_owned()) } else { None };
                (value, 2 + len)
            }
            other => return Err(malformed(&format!("unknown header type {other}"))),
        };
        if let Some(value) = value {
            out.push((name, value));
        }
        bytes = bytes.get(value_start + used..).ok_or_else(|| malformed("header value"))?;
    }
    Ok(out)
}

/// Incremental decoder.
#[derive(Debug, Default)]
pub(crate) struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    /// Feed bytes; returns every complete message.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Message>, LlmError> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            if self.buf.len() < PRELUDE {
                return Ok(out);
            }
            let total = be_u32(&self.buf, 0).ok_or_else(|| malformed("prelude"))?;
            let total = usize::try_from(total).map_err(|_| malformed("length"))?;
            let headers_len = be_u32(&self.buf, 4).ok_or_else(|| malformed("prelude"))?;
            let headers_len = usize::try_from(headers_len).map_err(|_| malformed("length"))?;
            let prelude_crc = be_u32(&self.buf, 8).ok_or_else(|| malformed("prelude"))?;
            if crc32(self.buf.get(..8).unwrap_or_default()) != prelude_crc {
                return Err(malformed("prelude checksum mismatch"));
            }
            if !(MIN_MESSAGE..=MAX_MESSAGE_BYTES).contains(&total) || headers_len > total - MIN_MESSAGE {
                return Err(malformed(&format!("invalid lengths (total {total}, headers {headers_len})")));
            }
            if self.buf.len() < total {
                return Ok(out);
            }
            let message_crc = be_u32(&self.buf, total - 4).ok_or_else(|| malformed("checksum"))?;
            if crc32(self.buf.get(..total - 4).unwrap_or_default()) != message_crc {
                return Err(malformed("message checksum mismatch"));
            }
            let headers = parse_headers(self.buf.get(PRELUDE..PRELUDE + headers_len).unwrap_or_default())?;
            let payload = self.buf.get(PRELUDE + headers_len..total - 4).unwrap_or_default().to_vec();
            self.buf.drain(..total);
            out.push(Message { headers, payload });
        }
    }

    /// Whether undecoded bytes remain.
    pub fn has_partial(&self) -> bool {
        !self.buf.is_empty()
    }
}

/// Encode a message with string headers (used by tests and fixtures).
#[cfg(test)]
pub(crate) fn encode(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
    let mut h = Vec::new();
    for (name, value) in headers {
        h.push(u8::try_from(name.len()).unwrap_or(u8::MAX));
        h.extend_from_slice(name.as_bytes());
        h.push(7);
        h.extend_from_slice(&u16::try_from(value.len()).unwrap_or(u16::MAX).to_be_bytes());
        h.extend_from_slice(value.as_bytes());
    }
    let total = u32::try_from(PRELUDE + h.len() + payload.len() + 4).unwrap_or(u32::MAX);
    let mut out = Vec::new();
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&u32::try_from(h.len()).unwrap_or(u32::MAX).to_be_bytes());
    let prelude_crc = crc32(&out);
    out.extend_from_slice(&prelude_crc.to_be_bytes());
    out.extend_from_slice(&h);
    out.extend_from_slice(payload);
    let crc = crc32(&out);
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn roundtrip_split_across_chunks() {
        let a = encode(&[(":event-type", "contentBlockDelta"), (":message-type", "event")], b"{\"x\":1}");
        let b = encode(&[(":event-type", "messageStop")], b"{}");
        let all: Vec<u8> = a.iter().chain(b.iter()).copied().collect();
        let mut d = Decoder::default();
        let mut msgs = Vec::new();
        for chunk in all.chunks(5) {
            msgs.extend(d.push(chunk).unwrap());
        }
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].header(":event-type"), Some("contentBlockDelta"));
        assert_eq!(msgs[0].payload, b"{\"x\":1}");
        assert_eq!(msgs[1].header(":event-type"), Some("messageStop"));
        assert!(!d.has_partial());
    }

    #[test]
    fn corrupted_and_oversized_messages_are_rejected() {
        let mut m = encode(&[(":event-type", "x")], b"{}");
        let last = m.len() - 6;
        m[last] ^= 0xff;
        assert!(Decoder::default().push(&m).is_err());

        let mut huge = Vec::new();
        huge.extend_from_slice(&(u32::MAX).to_be_bytes());
        huge.extend_from_slice(&0u32.to_be_bytes());
        let crc = crc32(&huge);
        huge.extend_from_slice(&crc.to_be_bytes());
        assert!(Decoder::default().push(&huge).is_err());
    }

    #[test]
    fn non_string_headers_are_skipped() {
        // header "n" of type int32 (4), then a string header.
        let mut h = vec![1, b'n', 4, 0, 0, 0, 5];
        h.extend_from_slice(&[1, b's', 7, 0, 2, b'o', b'k']);
        assert_eq!(parse_headers(&h).unwrap(), vec![("s".to_owned(), "ok".to_owned())]);
        assert!(parse_headers(&[5, b'a']).is_err());
    }
}
