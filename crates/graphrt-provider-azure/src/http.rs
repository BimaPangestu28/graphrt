//! The one HTTP layer of the crate: client construction, bounded body reads,
//! form encoding, retry with jittered backoff and a per-API token bucket.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Largest response body read from any Azure endpoint (ARM pages are well below).
pub(crate) const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// Build a reqwest client on rustls/ring that never follows redirects, so a
/// bearer token can never be replayed to another host.
pub fn build_client(request_timeout: Duration) -> Result<reqwest::Client, String> {
    // reqwest is built without a bundled provider; an error only means one is installed.
    let _already_installed = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(request_timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("cannot build HTTP client: {error}"))
}

static JSON_NULL: serde_json::Value = serde_json::Value::Null;

/// The value at a JSON pointer (`/a/b`), or `null`; never panics.
pub(crate) fn json_at<'a>(value: &'a serde_json::Value, pointer: &str) -> &'a serde_json::Value {
    value.pointer(pointer).unwrap_or(&JSON_NULL)
}

/// Read at most `MAX_BODY_BYTES` of a response body.
pub(crate) async fn read_body(mut response: reqwest::Response) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| format!("reading body: {error}"))? {
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

/// Whether a status is worth retrying (throttling, server errors, IMDS 404/410 while it boots).
pub(crate) fn is_retryable_status(status: u16) -> bool {
    status == 429 || status == 408 || (500..600).contains(&status)
}

/// `Retry-After` in seconds (the only form Azure sends), capped at 60 s.
pub(crate) fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let seconds: u64 = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?.trim().parse().ok()?;
    Some(Duration::from_secs(seconds.min(60)))
}

/// Retry schedule: full jitter over an exponential base, never below the server's hint.
#[derive(Clone, Debug)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_attempts: 4, base_delay: Duration::from_millis(200), max_delay: Duration::from_secs(8) }
    }
}

impl RetryPolicy {
    /// Delay before retry number `attempt` (1-based).
    pub(crate) fn delay(&self, attempt: u32, server_hint: Option<Duration>) -> Duration {
        let exponential = self.base_delay.saturating_mul(1u32 << attempt.min(16)).min(self.max_delay);
        let jittered = exponential.mul_f64(jitter_fraction());
        match server_hint {
            Some(hint) => hint.max(jittered),
            None => jittered.max(Duration::from_millis(10)),
        }
    }
}

/// A uniform-ish fraction in [0, 1) from the std hasher's random keys; good
/// enough to spread retries, and needs no RNG crate.
fn jitter_fraction() -> f64 {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u128(Instant::now().elapsed().as_nanos());
    (hasher.finish() >> 11) as f64 / (1u64 << 53) as f64
}

/// Token bucket shared by every call to one provider API, so a wake storm
/// cannot exhaust the subscription's ARM quota.
#[derive(Debug)]
pub struct TokenBucket {
    rate_per_sec: f64,
    burst: f64,
    state: Mutex<(f64, Instant)>,
}

impl TokenBucket {
    pub fn new(rate_per_sec: f64, burst: u32) -> Self {
        let burst = f64::from(burst.max(1));
        Self { rate_per_sec: rate_per_sec.max(0.001), burst, state: Mutex::new((burst, Instant::now())) }
    }

    /// Wait until a token is available, then take it.
    pub async fn acquire(&self) {
        loop {
            let wait = self.try_take();
            match wait {
                None => return,
                Some(duration) => tokio::time::sleep(duration).await,
            }
        }
    }

    /// Take a token now, or say how long until one is available.
    fn try_take(&self) -> Option<Duration> {
        let mut guard = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let (tokens, last_refill) = &mut *guard;
        let now = Instant::now();
        *tokens = (*tokens + now.duration_since(*last_refill).as_secs_f64() * self.rate_per_sec).min(self.burst);
        *last_refill = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            return None;
        }
        Some(Duration::from_secs_f64((1.0 - *tokens) / self.rate_per_sec))
    }
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
    fn retry_delay_respects_server_hint_and_cap() {
        let policy = RetryPolicy::default();
        assert!(policy.delay(1, Some(Duration::from_secs(3))) >= Duration::from_secs(3));
        for attempt in 1..20 {
            assert!(policy.delay(attempt, None) <= policy.max_delay);
        }
    }

    #[test]
    fn jitter_spreads_delays() {
        let policy = RetryPolicy::default();
        let delays: std::collections::HashSet<_> = (0..20).map(|_| policy.delay(3, None)).collect();
        assert!(delays.len() > 1, "jitter must not produce one fixed delay");
    }

    #[tokio::test]
    async fn token_bucket_limits_rate_after_burst() {
        let bucket = TokenBucket::new(50.0, 5);
        let started = Instant::now();
        for _ in 0..10 {
            bucket.acquire().await;
        }
        // 5 from the burst, 5 more at 50/s = ~100 ms.
        assert!(started.elapsed() >= Duration::from_millis(80), "elapsed {:?}", started.elapsed());
    }
}
