//! Minimal Kubernetes REST client: JSON over reqwest + rustls, bearer token or
//! client certificate auth, a token bucket per API server, retries with full
//! jitter on 429 / 5xx / transport errors, one token refresh on 401, and
//! newline-delimited JSON watch streams.
//!
//! Every operation the provider sends is idempotent (CAS scale patches with a
//! `resourceVersion`, annotation merge patches, server-side apply), so all of
//! them are retried.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;

use crate::api::ProviderError;
use crate::auth::{AuthError, CachingTokenSource, ClusterConfig, SecretString};

/// Tuning of the REST client.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// Sustained requests per second to the API server (client-go default is 5).
    pub qps: f64,
    /// Burst above `qps`.
    pub burst: u32,
    /// Attempts per request, including the first.
    pub max_attempts: u32,
    /// First retry delay; doubles per attempt, full jitter.
    pub backoff_base: Duration,
    /// Upper bound of one retry delay.
    pub backoff_cap: Duration,
    /// Timeout of one non-watch request.
    pub request_timeout: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            qps: 20.0,
            burst: 40,
            max_attempts: 4,
            backoff_base: Duration::from_millis(100),
            backoff_cap: Duration::from_secs(2),
            request_timeout: Duration::from_secs(10),
        }
    }
}

/// Errors of one API call after retries.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KubeError {
    /// The API server answered with a non-success status.
    #[error("{method} {path}: HTTP {code} {reason}: {message}")]
    Status { method: String, path: String, code: u16, reason: String, message: String, retry_after: Option<Duration> },
    #[error("{method} {path}: {message}")]
    Transport { method: String, path: String, message: String },
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error("decode {path}: {message}")]
    Decode { path: String, message: String },
    #[error("client setup: {0}")]
    Setup(String),
}

impl KubeError {
    /// HTTP status code, if the server answered.
    pub fn status_code(&self) -> Option<u16> {
        match self {
            Self::Status { code, .. } => Some(*code),
            _ => None,
        }
    }

    /// Kubernetes `Status.reason` (e.g. `Conflict`, `Expired`), if any.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Status { reason, .. } => Some(reason),
            _ => None,
        }
    }
}

impl From<KubeError> for ProviderError {
    fn from(error: KubeError) -> Self {
        let message = error.to_string();
        match &error {
            KubeError::Status { code: 401 | 403, .. } | KubeError::Auth(_) => Self::Auth(message),
            KubeError::Status { code: 404, .. } => Self::NotFound(message),
            KubeError::Status { code: 429, retry_after, .. } => Self::RateLimited { message, retry_after: *retry_after },
            KubeError::Status { code: 400 | 405 | 415 | 422, .. } | KubeError::Setup(_) | KubeError::Decode { .. } => {
                Self::Fatal(message)
            }
            KubeError::Status { .. } | KubeError::Transport { .. } => Self::Transient(message),
        }
    }
}

/// Body of a request: content type and bytes.
#[derive(Debug, Clone)]
pub struct RequestBody {
    pub content_type: &'static str,
    pub bytes: Vec<u8>,
}

impl RequestBody {
    /// JSON merge patch (`application/merge-patch+json`).
    pub fn merge_patch(value: &serde_json::Value) -> Self {
        Self { content_type: "application/merge-patch+json", bytes: value.to_string().into_bytes() }
    }

    /// Server-side apply (`application/apply-patch+yaml`; JSON is valid YAML).
    pub fn apply_patch(value: &serde_json::Value) -> Self {
        Self { content_type: "application/apply-patch+yaml", bytes: value.to_string().into_bytes() }
    }
}

/// Client for one API server.
#[derive(Debug, Clone)]
pub struct KubeClient {
    inner: Arc<ClientInner>,
}

#[derive(Debug)]
struct ClientInner {
    http: reqwest::Client,
    server: String,
    tokens: Option<CachingTokenSource>,
    limiter: TokenBucket,
    options: ClientOptions,
}

impl KubeClient {
    /// Build a client from resolved cluster settings.
    ///
    /// @throws KubeError::Setup - bad CA / identity PEM, or a server URL that is not http(s)
    pub fn new(config: &ClusterConfig, options: ClientOptions) -> Result<Self, KubeError> {
        let server = config.server.trim_end_matches('/').to_owned();
        if !(server.starts_with("https://") || server.starts_with("http://")) {
            return Err(KubeError::Setup(format!("server {server:?} is not an http(s) URL")));
        }
        let _already_installed = rustls::crypto::ring::default_provider().install_default();
        // No total timeout on the client: watches are long-lived. Requests set their own.
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .pool_idle_timeout(Duration::from_secs(90))
            // Never follow redirects: the bearer token must only reach the API server.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("graphrt-gateway/", env!("CARGO_PKG_VERSION")));
        if let Some(ca_pem) = &config.ca_pem {
            let certificates = reqwest::Certificate::from_pem_bundle(ca_pem)
                .map_err(|error| KubeError::Setup(format!("cluster CA: {error}")))?;
            builder = builder.tls_certs_only(certificates);
        }
        if let Some(identity_pem) = &config.client_identity_pem {
            let identity = reqwest::Identity::from_pem(identity_pem.expose().as_bytes())
                .map_err(|_| KubeError::Setup("client certificate / key is not valid PEM".into()))?;
            builder = builder.identity(identity);
        }
        let http = builder.build().map_err(|error| KubeError::Setup(error.to_string()))?;
        Ok(Self {
            inner: Arc::new(ClientInner {
                http,
                server,
                tokens: config.token_source.clone().map(CachingTokenSource::new),
                limiter: TokenBucket::new(options.qps, options.burst),
                options,
            }),
        })
    }

    /// Send a request, retrying transient failures; returns the success body.
    pub async fn send(&self, method: Method, path: &str, body: Option<RequestBody>) -> Result<Vec<u8>, KubeError> {
        let response = self.send_raw(&method, path, body.as_ref(), self.inner.options.request_timeout).await?;
        response.bytes().await.map(|bytes| bytes.to_vec()).map_err(|error| transport(&method, path, &error))
    }

    /// `send` and decode the JSON body.
    pub async fn send_json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<RequestBody>,
    ) -> Result<T, KubeError> {
        let bytes = self.send(method, path, body).await?;
        serde_json::from_slice(&bytes)
            .map_err(|error| KubeError::Decode { path: path.to_owned(), message: error.to_string() })
    }

    /// `GET` and decode.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, KubeError> {
        self.send_json(Method::GET, path, None).await
    }

    /// Open a watch (`?watch=1&timeoutSeconds=N` must be in `path`).
    pub async fn watch(&self, path: &str, timeout: Duration) -> Result<WatchStream, KubeError> {
        let response = self.send_raw(&Method::GET, path, None, timeout + Duration::from_secs(10)).await?;
        Ok(WatchStream { response, buffer: Vec::new(), path: path.to_owned() })
    }

    async fn send_raw(
        &self,
        method: &Method,
        path: &str,
        body: Option<&RequestBody>,
        timeout: Duration,
    ) -> Result<reqwest::Response, KubeError> {
        let options = &self.inner.options;
        let mut refreshed_token = false;
        let mut attempt = 0;
        loop {
            attempt += 1;
            self.inner.limiter.acquire().await;
            let token = match &self.inner.tokens {
                Some(tokens) => Some(tokens.token().await?),
                None => None,
            };
            let outcome = self.attempt(method, path, body, token.as_ref(), timeout).await;
            let error = match outcome {
                Ok(response) => return Ok(response),
                Err(error) => error,
            };
            if error.status_code() == Some(401)
                && !refreshed_token
                && let (Some(tokens), Some(token)) = (&self.inner.tokens, &token)
            {
                // Rotated or expired token: refresh once, then give up (plan §6).
                tracing::debug!(%method, path, "kubernetes API returned 401, refreshing token");
                tokens.invalidate(token).await;
                refreshed_token = true;
                continue;
            }
            if attempt >= options.max_attempts || !is_retryable(&error) {
                return Err(error);
            }
            let delay = retry_delay(&error, attempt, options);
            tracing::debug!(%method, path, attempt, ?delay, %error, "retrying kubernetes API call");
            tokio::time::sleep(delay).await;
        }
    }

    async fn attempt(
        &self,
        method: &Method,
        path: &str,
        body: Option<&RequestBody>,
        token: Option<&SecretString>,
        timeout: Duration,
    ) -> Result<reqwest::Response, KubeError> {
        let url = format!("{}{}", self.inner.server, path);
        let mut request =
            self.inner.http.request(method.clone(), url).timeout(timeout).header("Accept", "application/json");
        if let Some(token) = token {
            request = request.bearer_auth(token.expose());
        }
        if let Some(body) = body {
            request = request.header("Content-Type", body.content_type).body(body.bytes.clone());
        }
        let response = request.send().await.map_err(|error| transport(method, path, &error))?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let retry_after = response
            .headers()
            .get("Retry-After")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let body = response.bytes().await.unwrap_or_default();
        Err(status_error(method, path, status, &body, retry_after))
    }
}

fn transport(method: &Method, path: &str, error: &reqwest::Error) -> KubeError {
    // reqwest errors carry the URL, never headers: safe to keep.
    KubeError::Transport { method: method.to_string(), path: path.to_owned(), message: error.to_string() }
}

fn status_error(
    method: &Method,
    path: &str,
    status: StatusCode,
    body: &[u8],
    retry_after: Option<Duration>,
) -> KubeError {
    #[derive(serde::Deserialize, Default)]
    struct Status {
        #[serde(default)]
        reason: String,
        #[serde(default)]
        message: String,
    }
    let parsed: Status = serde_json::from_slice(body).unwrap_or_default();
    let message = if parsed.message.is_empty() {
        String::from_utf8_lossy(body).chars().take(256).collect()
    } else {
        parsed.message
    };
    KubeError::Status {
        method: method.to_string(),
        path: path.to_owned(),
        code: status.as_u16(),
        reason: parsed.reason,
        message,
        retry_after,
    }
}

fn is_retryable(error: &KubeError) -> bool {
    match error {
        KubeError::Status { code, .. } => matches!(code, 429 | 500 | 502 | 503 | 504),
        KubeError::Transport { .. } => true,
        _ => false,
    }
}

/// Full-jitter exponential backoff; a server `Retry-After` is a lower bound.
fn retry_delay(error: &KubeError, attempt: u32, options: &ClientOptions) -> Duration {
    let exponent = attempt.saturating_sub(1).min(16);
    let ceiling = options.backoff_base.saturating_mul(1 << exponent).min(options.backoff_cap);
    let jittered = ceiling.mul_f64(random_fraction());
    match error {
        KubeError::Status { retry_after: Some(retry_after), .. } => jittered.max(*retry_after.min(&options.backoff_cap)),
        _ => jittered,
    }
}

/// A uniform value in [0, 1) from the std hasher's random keys (no rand crate).
pub(crate) fn random_fraction() -> f64 {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u128(Instant::now().elapsed().as_nanos());
    (hasher.finish() >> 11) as f64 / (1u64 << 53) as f64
}

/// Token bucket limiting calls to one API server.
#[derive(Debug)]
struct TokenBucket {
    rate_per_sec: f64,
    burst: f64,
    state: Mutex<(f64, Instant)>,
}

impl TokenBucket {
    fn new(rate_per_sec: f64, burst: u32) -> Self {
        let burst = f64::from(burst.max(1));
        Self { rate_per_sec: rate_per_sec.max(0.1), burst, state: Mutex::new((burst, Instant::now())) }
    }

    async fn acquire(&self) {
        let wait = {
            let mut state = self.state.lock().await;
            let (tokens, last) = &mut *state;
            let now = Instant::now();
            *tokens = (*tokens + now.duration_since(*last).as_secs_f64() * self.rate_per_sec).min(self.burst);
            *last = now;
            *tokens -= 1.0;
            if *tokens >= 0.0 { Duration::ZERO } else { Duration::from_secs_f64(-*tokens / self.rate_per_sec) }
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

/// One event of a watch stream.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct WatchEvent<T> {
    #[serde(rename = "type")]
    pub kind: String,
    pub object: T,
}

/// Newline-delimited JSON watch events, read chunk by chunk.
#[derive(Debug)]
pub struct WatchStream {
    response: reqwest::Response,
    buffer: Vec<u8>,
    path: String,
}

impl WatchStream {
    /// The next event, or `None` when the server closed the watch.
    pub async fn next<T: DeserializeOwned>(&mut self) -> Result<Option<WatchEvent<T>>, KubeError> {
        loop {
            if let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=newline).collect();
                let line = line.trim_ascii();
                if line.is_empty() {
                    continue;
                }
                return serde_json::from_slice(line)
                    .map(Some)
                    .map_err(|error| KubeError::Decode { path: self.path.clone(), message: error.to_string() });
            }
            match self.response.chunk().await {
                Ok(Some(chunk)) => self.buffer.extend_from_slice(&chunk),
                Ok(None) => return Ok(None),
                Err(error) => return Err(transport(&Method::GET, &self.path, &error)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(code: u16, retry_after: Option<Duration>) -> KubeError {
        KubeError::Status {
            method: "GET".into(),
            path: "/x".into(),
            code,
            reason: String::new(),
            message: String::new(),
            retry_after,
        }
    }

    #[test]
    fn maps_statuses_to_provider_errors() {
        assert!(matches!(ProviderError::from(status(401, None)), ProviderError::Auth(_)));
        assert!(matches!(ProviderError::from(status(403, None)), ProviderError::Auth(_)));
        assert!(matches!(ProviderError::from(status(404, None)), ProviderError::NotFound(_)));
        assert!(matches!(
            ProviderError::from(status(429, Some(Duration::from_secs(3)))),
            ProviderError::RateLimited { retry_after: Some(_), .. }
        ));
        assert!(matches!(ProviderError::from(status(409, None)), ProviderError::Transient(_)));
        assert!(matches!(ProviderError::from(status(503, None)), ProviderError::Transient(_)));
        assert!(matches!(ProviderError::from(status(422, None)), ProviderError::Fatal(_)));
    }

    #[test]
    fn backoff_is_jittered_bounded_and_respects_retry_after() {
        let options = ClientOptions::default();
        for attempt in 1..10 {
            let delay = retry_delay(&status(503, None), attempt, &options);
            assert!(delay <= options.backoff_cap, "{delay:?}");
        }
        let throttled = retry_delay(&status(429, Some(Duration::from_secs(1))), 1, &options);
        assert!(throttled >= Duration::from_secs(1));
        let fractions: Vec<f64> = (0..32).map(|_| random_fraction()).collect();
        assert!(fractions.iter().all(|fraction| (0.0..1.0).contains(fraction)));
        assert!(fractions.windows(2).any(|pair| pair[0] != pair[1]));
    }

    #[test]
    fn rejects_non_http_servers() {
        assert!(matches!(KubeClient::new(&ClusterConfig::new("ftp://k8s"), ClientOptions::default()), Err(KubeError::Setup(_))));
    }

    #[tokio::test]
    async fn token_bucket_throttles_after_burst() {
        let bucket = TokenBucket::new(50.0, 2);
        let started = Instant::now();
        for _ in 0..4 {
            bucket.acquire().await;
        }
        // Two tokens of burst, then two more at 50/s: at least ~40 ms.
        assert!(started.elapsed() >= Duration::from_millis(35), "{:?}", started.elapsed());
    }
}
