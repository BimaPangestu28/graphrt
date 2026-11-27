//! The HTTP calls both channels share: send a request, read a bounded answer,
//! and never let a URL into an error (a Telegram URL carries the bot token).

use std::time::Duration;

use crate::model::{ChannelError, ChannelKind};

/// Longest wait for an ordinary API call.
pub(crate) const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest wait for a file transfer.
pub(crate) const TRANSFER_TIMEOUT: Duration = Duration::from_secs(120);
/// Largest API answer read (file downloads have their own limit).
pub(crate) const MAX_ANSWER_BYTES: u64 = 4 * 1024 * 1024;

/// An answer: its status, content type and body.
#[derive(Debug)]
pub(crate) struct Answer {
    pub status: u16,
    #[cfg_attr(not(any(feature = "whatsapp", feature = "slack")), allow(dead_code))]
    pub content_type: Option<String>,
    /// The `Retry-After` header in seconds (Slack and Teams say how long to wait there).
    #[cfg_attr(not(any(feature = "slack", feature = "teams")), allow(dead_code))]
    pub retry_after: Option<u64>,
    pub body: Vec<u8>,
}

impl Answer {
    /// The body as JSON, or `Null` when it is not.
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

fn transport(channel: ChannelKind, error: reqwest::Error) -> ChannelError {
    let reason = if error.is_timeout() {
        "the request timed out"
    } else if error.is_connect() {
        "the connection failed"
    } else if error.is_body() || error.is_decode() {
        "the answer was cut off"
    } else {
        "the request failed"
    };
    // Never `error.to_string()`: it names the URL.
    ChannelError::Transport { channel, reason: reason.to_owned() }
}

/// Send `request` and read at most `max_bytes` of the answer.
///
/// @throws ChannelError - `Transport` when no answer arrived, `TooLarge` beyond `max_bytes`.
pub(crate) async fn send(
    channel: ChannelKind,
    request: reqwest::RequestBuilder,
    timeout: Duration,
    max_bytes: u64,
) -> Result<Answer, ChannelError> {
    let mut response = request.timeout(timeout).send().await.map_err(|e| transport(channel, e))?;
    let status = response.status().as_u16();
    let content_type =
        response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    if response.content_length().is_some_and(|length| length > max_bytes) {
        return Err(ChannelError::TooLarge { limit: max_bytes });
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| transport(channel, e))? {
        if u64::try_from(body.len() + chunk.len()).unwrap_or(u64::MAX) > max_bytes {
            return Err(ChannelError::TooLarge { limit: max_bytes });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Answer { status, content_type, retry_after, body })
}

/// `text` with every byte but the unreserved ones (`A-Z a-z 0-9 - . _ ~`)
/// percent encoded: safe as a path segment, a query value or a form value.
#[cfg_attr(not(any(feature = "slack", feature = "teams")), allow(dead_code))]
pub(crate) fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// An `application/x-www-form-urlencoded` body of `fields`.
#[cfg_attr(not(any(feature = "slack", feature = "teams")), allow(dead_code))]
pub(crate) fn form(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(name, value)| format!("{}={}", percent_encode(name), percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// A `multipart/form-data` body with text fields and one file.
#[cfg_attr(not(any(feature = "telegram", feature = "whatsapp")), allow(dead_code))]
#[derive(Debug)]
pub(crate) struct Multipart {
    boundary: String,
    body: Vec<u8>,
}

#[cfg_attr(not(any(feature = "telegram", feature = "whatsapp")), allow(dead_code))]
impl Multipart {
    pub fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        Self { boundary: format!("graphrt-{nanos:x}-{n:x}"), body: Vec::new() }
    }

    /// A header value without the characters that would end it.
    fn quoted(value: &str) -> String {
        value.chars().filter(|c| !matches!(c, '"' | '\r' | '\n' | '\\')).collect()
    }

    pub fn text(mut self, name: &str, value: &str) -> Self {
        let head =
            format!("--{}\r\nContent-Disposition: form-data; name=\"{}\"\r\n\r\n", self.boundary, Self::quoted(name));
        self.body.extend_from_slice(head.as_bytes());
        self.body.extend_from_slice(value.as_bytes());
        self.body.extend_from_slice(b"\r\n");
        self
    }

    pub fn file(mut self, name: &str, file_name: &str, mime_type: &str, bytes: &[u8]) -> Self {
        let head = format!(
            "--{}\r\nContent-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\nContent-Type: {}\r\n\r\n",
            self.boundary,
            Self::quoted(name),
            Self::quoted(file_name),
            Self::quoted(mime_type)
        );
        self.body.extend_from_slice(head.as_bytes());
        self.body.extend_from_slice(bytes);
        self.body.extend_from_slice(b"\r\n");
        self
    }

    /// The `Content-Type` header value and the finished body.
    pub fn finish(mut self) -> (String, Vec<u8>) {
        self.body.extend_from_slice(format!("--{}--\r\n", self.boundary).as_bytes());
        (format!("multipart/form-data; boundary={}", self.boundary), self.body)
    }
}

/// A file name safe to send in a header and to show: the last path segment,
/// without control characters, never empty.
pub(crate) fn safe_file_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = last.chars().filter(|c| !c.is_control()).take(120).collect();
    let cleaned = cleaned.trim().trim_start_matches('.');
    if cleaned.is_empty() { "file".to_owned() } else { cleaned.to_owned() }
}

/// A client for tests. In this workspace reqwest is built without a TLS
/// provider, so one is installed first.
#[cfg(test)]
pub(crate) fn test_client() -> reqwest::Client {
    let _already_installed = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_multipart_body_has_its_fields_and_one_closing_boundary() {
        let (content_type, body) =
            Multipart::new().text("chat_id", "42").file("document", "a\"b\r\n.txt", "text/plain", b"hello").finish();
        let boundary = content_type.strip_prefix("multipart/form-data; boundary=").unwrap_or_default();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("name=\"chat_id\"\r\n\r\n42\r\n"));
        assert!(text.contains("filename=\"ab.txt\"\r\nContent-Type: text/plain\r\n\r\nhello\r\n"), "{text}");
        assert!(text.ends_with(&format!("--{boundary}--\r\n")));
    }

    #[test]
    fn values_are_percent_encoded_for_paths_and_forms() {
        assert_eq!(percent_encode("19:abc@thread.tacv2;messageid=1"), "19%3Aabc%40thread.tacv2%3Bmessageid%3D1");
        assert_eq!(form(&[("a b", "x&y=z"), ("n", "ü")]), "a%20b=x%26y%3Dz&n=%C3%BC");
    }

    #[test]
    fn file_names_lose_their_path_and_are_never_empty() {
        assert_eq!(safe_file_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_file_name("C:\\Users\\x\\report.pdf"), "report.pdf");
        assert_eq!(safe_file_name(".."), "file");
        assert_eq!(safe_file_name("a\u{0}b.png"), "ab.png");
    }
}
