//! In-memory span model, ids and W3C trace context.

use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

/// 16-byte trace id.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TraceId(pub [u8; 16]);

/// 8-byte span id.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpanId(pub [u8; 8]);

impl TraceId {
    /// A random, non-zero trace id.
    pub fn random() -> Self {
        let a = random_u64().to_be_bytes();
        let b = random_u64().to_be_bytes();
        let mut out = [0u8; 16];
        out[..8].copy_from_slice(&a);
        out[8..].copy_from_slice(&b);
        if out == [0u8; 16] {
            out[15] = 1;
        }
        Self(out)
    }

    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }
}

impl SpanId {
    /// A random, non-zero span id.
    pub fn random() -> Self {
        let mut out = random_u64().to_be_bytes();
        if out == [0u8; 8] {
            out[7] = 1;
        }
        Self(out)
    }

    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }
}

impl fmt::Debug for TraceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TraceId({})", self.to_hex())
    }
}

impl fmt::Debug for SpanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SpanId({})", self.to_hex())
    }
}

/// Random 64 bits from SipHash keyed by the OS-seeded [`RandomState`], mixed with a
/// process-wide counter and the clock. Unpredictable enough for trace ids, with no
/// extra dependency.
fn random_u64() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    hasher.write_u64(now_unix_nanos());
    hasher.finish()
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(DIGITS.get(usize::from(b >> 4)).copied().unwrap_or(b'0')));
        out.push(char::from(DIGITS.get(usize::from(b & 0x0f)).copied().unwrap_or(b'0')));
    }
    out
}

fn parse_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; N];
    for (i, slot) in out.iter_mut().enumerate() {
        let pair = text.get(i * 2..i * 2 + 2)?;
        *slot = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

/// Current Unix time in nanoseconds (0 where no system clock exists).
pub fn now_unix_nanos() -> u64 {
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        0
    }
    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }
}

/// A parsed W3C `traceparent` header (`00-<trace-id>-<parent-id>-<flags>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceParent {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    pub sampled: bool,
}

impl TraceParent {
    /// Parse a `traceparent` header. Returns `None` for malformed or invalid values
    /// (version `ff`, all-zero ids), in which case a new trace should be started.
    pub fn parse(header: &str) -> Option<Self> {
        let mut parts = header.trim().split('-');
        let version = parts.next()?;
        let trace = parts.next()?;
        let span = parts.next()?;
        let flags = parts.next()?;
        if version.len() != 2 || version.eq_ignore_ascii_case("ff") || parse_hex::<1>(version).is_none() {
            return None;
        }
        // Version 00 has exactly four fields; later versions may append more.
        if version == "00" && parts.next().is_some() {
            return None;
        }
        let trace_id = parse_hex::<16>(&trace.to_ascii_lowercase())?;
        let span_id = parse_hex::<8>(&span.to_ascii_lowercase())?;
        let flags = parse_hex::<1>(&flags.to_ascii_lowercase())?;
        if trace_id == [0u8; 16] || span_id == [0u8; 8] {
            return None;
        }
        Some(Self { trace_id: TraceId(trace_id), span_id: SpanId(span_id), sampled: flags[0] & 1 == 1 })
    }

    /// Render as a `traceparent` header value.
    pub fn to_header(&self) -> String {
        format!("00-{}-{}-{}", self.trace_id.to_hex(), self.span_id.to_hex(), if self.sampled { "01" } else { "00" })
    }
}

/// An attribute value.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum AttrValue {
    Str(String),
    Int(i64),
    Double(f64),
    Bool(bool),
    /// A structured value (OTLP `AnyValue` with `arrayValue` / `kvlistValue`),
    /// as the GenAI conventions require for messages and documents.
    Structured(serde_json::Value),
}

impl From<&str> for AttrValue {
    fn from(v: &str) -> Self {
        AttrValue::Str(v.to_owned())
    }
}
impl From<String> for AttrValue {
    fn from(v: String) -> Self {
        AttrValue::Str(v)
    }
}
impl From<i64> for AttrValue {
    fn from(v: i64) -> Self {
        AttrValue::Int(v)
    }
}
impl From<u64> for AttrValue {
    fn from(v: u64) -> Self {
        AttrValue::Int(i64::try_from(v).unwrap_or(i64::MAX))
    }
}
impl From<u32> for AttrValue {
    fn from(v: u32) -> Self {
        AttrValue::Int(i64::from(v))
    }
}
impl From<bool> for AttrValue {
    fn from(v: bool) -> Self {
        AttrValue::Bool(v)
    }
}
impl From<f64> for AttrValue {
    fn from(v: f64) -> Self {
        AttrValue::Double(v)
    }
}

/// Ordered key/value attributes.
pub type Attributes = Vec<(String, AttrValue)>;

/// Span kind (subset used here).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpanKind {
    Internal,
    Server,
}

impl SpanKind {
    /// OTLP enum value.
    pub fn code(self) -> i64 {
        match self {
            SpanKind::Internal => 1,
            SpanKind::Server => 2,
        }
    }
}

/// Span status code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum StatusCode {
    #[default]
    Unset,
    Ok,
    Error,
}

impl StatusCode {
    /// OTLP enum value.
    pub fn code(self) -> i64 {
        match self {
            StatusCode::Unset => 0,
            StatusCode::Ok => 1,
            StatusCode::Error => 2,
        }
    }
}

/// A timestamped event inside a span.
#[derive(Clone, Debug, PartialEq)]
pub struct SpanEvent {
    pub time_unix_nano: u64,
    pub name: String,
    pub attributes: Attributes,
}

/// A finished (or in-progress) span.
#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    pub parent_span_id: Option<SpanId>,
    pub name: String,
    pub kind: SpanKind,
    pub start_unix_nano: u64,
    pub end_unix_nano: u64,
    pub attributes: Attributes,
    pub events: Vec<SpanEvent>,
    pub status: StatusCode,
    pub status_message: String,
    /// Events dropped because the per-span limit was reached.
    pub dropped_events: u32,
}

/// Maximum events recorded per span.
pub const MAX_EVENTS_PER_SPAN: usize = 128;

impl Span {
    pub fn new(trace_id: TraceId, parent: Option<SpanId>, name: impl Into<String>, start_unix_nano: u64) -> Self {
        Self {
            trace_id,
            span_id: SpanId::random(),
            parent_span_id: parent,
            name: name.into(),
            kind: SpanKind::Internal,
            start_unix_nano,
            end_unix_nano: start_unix_nano,
            attributes: Vec::new(),
            events: Vec::new(),
            status: StatusCode::Unset,
            status_message: String::new(),
            dropped_events: 0,
        }
    }

    /// Set (or replace) an attribute.
    pub fn set(&mut self, key: &str, value: impl Into<AttrValue>) {
        let value = value.into();
        match self.attributes.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value,
            None => self.attributes.push((key.to_owned(), value)),
        }
    }

    /// Attribute lookup (used by tests and tooling).
    pub fn attr(&self, key: &str) -> Option<&AttrValue> {
        self.attributes.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Record an event, respecting [`MAX_EVENTS_PER_SPAN`].
    pub fn add_event(&mut self, name: &str, time_unix_nano: u64, attributes: Attributes) {
        if self.events.len() >= MAX_EVENTS_PER_SPAN {
            self.dropped_events = self.dropped_events.saturating_add(1);
            return;
        }
        self.events.push(SpanEvent { time_unix_nano, name: name.to_owned(), attributes });
    }

    /// Whether this is a run's root span (it carries the run status). Exporters
    /// that drop spans under pressure keep these: several backends hide or discard
    /// a trace whose root never arrives.
    pub fn is_run_root(&self) -> bool {
        self.attr(crate::semconv::graphrt::RUN_STATUS).is_some()
    }

    /// Mark the span as failed.
    pub fn error(&mut self, message: &str) {
        self.status = StatusCode::Error;
        self.status_message = truncate(message, 1024);
    }
}

/// Truncate to at most `max` bytes on a char boundary, appending `…` when cut.
pub fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = text.get(..end).unwrap_or_default().to_owned();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_random_and_non_zero() {
        let a = TraceId::random();
        let b = TraceId::random();
        assert_ne!(a, b);
        assert_ne!(a.0, [0; 16]);
        assert_eq!(a.to_hex().len(), 32);
        assert_eq!(SpanId::random().to_hex().len(), 16);
    }

    #[test]
    fn traceparent_parsing() {
        let tp = TraceParent::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01").unwrap();
        assert_eq!(tp.trace_id.to_hex(), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(tp.span_id.to_hex(), "00f067aa0ba902b7");
        assert!(tp.sampled);
        assert_eq!(tp.to_header(), "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01");
        assert!(
            TraceParent::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00").is_some_and(|t| !t.sampled)
        );
        for bad in [
            "",
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "00-4bf92f3577b34da6a3ce929d0e0e473-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
            "00-xyz",
        ] {
            assert!(TraceParent::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("héllo", 2), "h…");
        assert_eq!(truncate("abc", 5), "abc");
    }
}
