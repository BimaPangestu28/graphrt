//! Azure-specific HTTP helpers on top of the shared provider HTTP layer
//! (`graphrt_provider_api::http`: token bucket, jittered retry, refresh on 401):
//! client construction, bounded body reads, form encoding, JSON pointers.

use std::time::Duration;

/// Largest response body read from any Azure endpoint (ARM pages are well below).
pub(crate) const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// Build a reqwest client on rustls/ring that never follows redirects, so a
/// bearer token can never be replayed to another host.
///
/// @throws a message when the TLS backend cannot be initialised.
pub fn build_client(request_timeout: Duration) -> Result<reqwest::Client, String> {
    build_client_with(request_timeout, &[])
}

/// [`build_client`] with DNS overrides (`host` → address).
pub(crate) fn build_client_with(
    request_timeout: Duration,
    overrides: &[(String, std::net::SocketAddr)],
) -> Result<reqwest::Client, String> {
    // reqwest is built without a bundled provider; an error only means one is installed.
    let _already_installed = rustls::crypto::ring::default_provider().install_default();
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(request_timeout)
        .redirect(reqwest::redirect::Policy::none());
    for (host, address) in overrides {
        builder = builder.resolve(host, *address);
    }
    builder.build().map_err(|error| format!("cannot build HTTP client: {error}"))
}

static JSON_NULL: serde_json::Value = serde_json::Value::Null;

/// The value at a JSON pointer (`/a/b`), or `null`; never panics.
pub(crate) fn json_at<'a>(value: &'a serde_json::Value, pointer: &str) -> &'a serde_json::Value {
    value.pointer(pointer).unwrap_or(&JSON_NULL)
}

/// Read at most `MAX_BODY_BYTES` of a response body.
pub(crate) async fn read_body(mut response: reqwest::Response) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| format!("reading body: {}", error.without_url()))? {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(format!("response body larger than {MAX_BODY_BYTES} bytes"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// `application/x-www-form-urlencoded` body from key/value pairs.
pub(crate) fn form_encode(pairs: &[(&str, &str)]) -> String {
    let mut encoded = String::new();
    for (index, (key, value)) in pairs.iter().enumerate() {
        if index > 0 {
            encoded.push('&');
        }
        encoded.push_str(&percent_encode(key));
        encoded.push('=');
        encoded.push_str(&percent_encode(value));
    }
    encoded
}

/// RFC 3986 unreserved characters stay, everything else is `%XX`.
pub(crate) fn percent_encode(input: &str) -> String {
    let mut encoded = String::with_capacity(input.len());
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Whether a token endpoint status is worth retrying (throttling, server errors).
pub(crate) fn is_retryable_status(status: u16) -> bool {
    status == 429 || status == 408 || (500..600).contains(&status)
}

/// `Retry-After` in seconds (the only form Azure sends), capped at 60 s.
pub(crate) fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    graphrt_provider_api::http::parse_retry_after(value).map(|delay| delay.min(Duration::from_secs(60)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_encoding_escapes_reserved_characters() {
        let body = form_encode(&[("scope", "https://management.azure.com/.default"), ("a b", "c&d")]);
        assert_eq!(body, "scope=https%3A%2F%2Fmanagement.azure.com%2F.default&a%20b=c%26d");
    }

    #[test]
    fn json_pointer_never_panics() {
        let value = serde_json::json!({"a": {"b": 1}});
        assert_eq!(json_at(&value, "/a/b"), 1);
        assert!(json_at(&value, "/a/missing/deeper").is_null());
    }
}
