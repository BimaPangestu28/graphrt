//! HTTP for connectors: every request goes through graphrt-http's
//! egress-policy client (no private or metadata addresses unless a source
//! allows them explicitly, DNS rebinding safe), signs with the source's
//! credential, retries rate limits and server errors a few times with
//! backoff, and reads bodies up to a size limit.

use std::time::Duration;

use graphrt_http::{EgressPolicy, HttpClient};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode};

use crate::auth::Auth;
use crate::error::{ConnectError, Result};

/// Largest JSON page a listing call may return.
pub const DEFAULT_MAX_JSON_BYTES: usize = 8 * 1024 * 1024;
/// Largest file or page downloaded for one item.
pub const DEFAULT_MAX_ITEM_BYTES: usize = 25 * 1024 * 1024;
/// Attempts of one request (first try plus retries).
const ATTEMPTS: u32 = 4;
/// Longest wait between two attempts, whatever `Retry-After` asks for.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Limits shared by every source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Largest JSON response of a listing request.
    pub max_json_bytes: usize,
    /// Largest file or page downloaded for one item.
    pub max_item_bytes: usize,
    /// Timeout of one request.
    pub request_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_json_bytes: DEFAULT_MAX_JSON_BYTES,
            max_item_bytes: DEFAULT_MAX_ITEM_BYTES,
            request_timeout: Duration::from_secs(60),
        }
    }
}

/// The egress policy connectors use by default: public addresses only.
/// `allow_private` lets a source reach the local network (an intranet wiki),
/// which an operator has to switch on per source.
pub fn egress_policy(allow_private: bool, limits: &Limits) -> EgressPolicy {
    EgressPolicy {
        allow_private,
        max_response_bytes: limits.max_item_bytes.max(limits.max_json_bytes),
        total_timeout: limits.request_timeout,
        ..EgressPolicy::default()
    }
}

/// An HTTP client of one source: policy, credential, limits and the name of
/// the service for error messages.
#[derive(Clone, Debug)]
pub struct SourceHttp {
    client: HttpClient,
    auth: Auth,
    limits: Limits,
    service: &'static str,
}

/// A downloaded body.
#[derive(Clone, Debug)]
pub struct Body {
    pub bytes: Vec<u8>,
    pub mime_type: Option<String>,
    pub headers: HeaderMap,
}

impl SourceHttp {
    pub fn new(client: HttpClient, auth: Auth, limits: Limits, service: &'static str) -> Self {
        Self { client, auth, limits, service }
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    pub fn service(&self) -> &'static str {
        self.service
    }

    fn reqwest(&self) -> Result<&reqwest::Client> {
        self.client.client(true).map_err(|e| ConnectError::Network(e.to_string()))
    }

    /// Validate an address against the egress policy before any request.
    ///
    /// @throws `ConnectError::Denied` for private addresses, other schemes or denied hosts.
    pub fn check_url(&self, url: &str) -> Result<reqwest::Url> {
        let parsed =
            reqwest::Url::parse(url).map_err(|e| ConnectError::Config(format!("invalid address `{url}`: {e}")))?;
        self.client.check_url(&parsed).map_err(|e| ConnectError::Denied(format!("`{url}` is not allowed: {e}")))?;
        Ok(parsed)
    }

    async fn authorize(&self, request: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
        Ok(match &self.auth {
            Auth::None => request,
            Auth::Bearer(token) => request.bearer_auth(token.expose()),
            Auth::Header { name, value } => {
                let name = HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| ConnectError::Config(format!("`{name}` is not a valid header name")))?;
                let mut value = HeaderValue::from_str(value.expose()).map_err(|_| {
                    ConnectError::Config("the API key contains characters a header cannot carry".into())
                })?;
                value.set_sensitive(true);
                request.header(name, value)
            }
            Auth::Basic { username, password } => request.basic_auth(username, Some(password.expose())),
            Auth::Oauth(tokens) => request.bearer_auth(tokens.access_token(self.reqwest()?).await?.expose()),
        })
    }

    /// Send a request and return the successful response.
    ///
    /// `build` is called once per attempt. A 401 with an OAuth credential
    /// refreshes the access token once; 429 and 5xx are retried with backoff.
    pub async fn send<F>(&self, method: Method, url: &str, build: F) -> Result<reqwest::Response>
    where
        F: Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
    {
        let parsed = self.check_url(url)?;
        let mut refreshed = false;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let request = self.reqwest()?.request(method.clone(), parsed.clone()).timeout(self.limits.request_timeout);
            let request = build(self.authorize(request).await?);
            let outcome = request.send().await;
            let response = match outcome {
                Ok(response) => response,
                Err(error) if attempt < ATTEMPTS && (error.is_timeout() || error.is_connect()) => {
                    tokio::time::sleep(backoff(attempt, None)).await;
                    continue;
                }
                Err(error) => return Err(network_error(self.service, &error)),
            };
            let status = response.status();
            if status.is_success() {
                return Ok(response);
            }
            if status == StatusCode::UNAUTHORIZED
                && !refreshed
                && let Auth::Oauth(tokens) = &self.auth
            {
                tokens.invalidate();
                refreshed = true;
                continue;
            }
            let retryable = status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
            if retryable && attempt < ATTEMPTS {
                tokio::time::sleep(backoff(attempt, retry_after(response.headers()))).await;
                continue;
            }
            let detail = error_detail(response).await;
            return Err(ConnectError::from_status(self.service, status.as_u16(), &detail));
        }
    }

    /// `GET` a JSON document.
    pub async fn get_json(&self, url: &str) -> Result<serde_json::Value> {
        self.json(Method::GET, url, None, &[]).await
    }

    /// A JSON request with an optional JSON body and extra headers.
    pub async fn json(
        &self,
        method: Method,
        url: &str,
        body: Option<&serde_json::Value>,
        headers: &[(HeaderName, HeaderValue)],
    ) -> Result<serde_json::Value> {
        let response = self
            .send(method, url, |mut request| {
                request = request.header(reqwest::header::ACCEPT, "application/json");
                for (name, value) in headers {
                    request = request.header(name.clone(), value.clone());
                }
                match body {
                    Some(body) => request.json(body),
                    None => request,
                }
            })
            .await?;
        let bytes = read_limited(response, self.limits.max_json_bytes).await?;
        serde_json::from_slice(&bytes)
            .map_err(|e| ConnectError::Decode(format!("{} returned something that is not JSON: {e}", self.service)))
    }

    /// `GET` a file or page, up to [`Limits::max_item_bytes`].
    pub async fn get_bytes(&self, url: &str, headers: &[(HeaderName, HeaderValue)]) -> Result<Body> {
        let response = self
            .send(Method::GET, url, |mut request| {
                for (name, value) in headers {
                    request = request.header(name.clone(), value.clone());
                }
                request
            })
            .await?;
        let headers = response.headers().clone();
        let mime_type = headers
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(';').next().unwrap_or(v).trim().to_ascii_lowercase())
            .filter(|v| !v.is_empty());
        let bytes = read_limited(response, self.limits.max_item_bytes).await?;
        Ok(Body { bytes, mime_type, headers })
    }
}

/// Read a response body, refusing more than `max` bytes.
///
/// @throws `ConnectError::TooLarge` past the limit, `Network` when the body breaks off.
pub async fn read_limited(mut response: reqwest::Response, max: usize) -> Result<Vec<u8>> {
    if response.content_length().is_some_and(|length| length > u64::try_from(max).unwrap_or(u64::MAX)) {
        return Err(too_large(max));
    }
    let mut bytes = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if bytes.len().saturating_add(chunk.len()) > max {
                    return Err(too_large(max));
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(bytes),
            Err(e) => return Err(ConnectError::Network(format!("the download broke off: {}", e.without_url()))),
        }
    }
}

fn too_large(max: usize) -> ConnectError {
    ConnectError::TooLarge(format!("it is larger than {} MB, the limit for one item", max.div_ceil(1024 * 1024)))
}

fn network_error(service: &str, error: &reqwest::Error) -> ConnectError {
    let text = error.without_url_ref();
    if text.contains("not a public address") || text.contains("egress policy") {
        return ConnectError::Denied(format!("{service}: {text}"));
    }
    if error.is_timeout() {
        return ConnectError::Network(format!("{service} did not answer in time"));
    }
    ConnectError::Network(format!("could not reach {service}: {text}"))
}

/// A short, credential free description of an error response (`error.message`
/// of a JSON body, at most 200 characters).
async fn error_detail(response: reqwest::Response) -> String {
    let Ok(bytes) = read_limited(response, 64 * 1024).await else { return String::new() };
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    let message = json
        .pointer("/error/message")
        .or_else(|| json.get("message"))
        .or_else(|| json.get("error_description"))
        .or_else(|| json.get("error"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    message.chars().take(200).collect()
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

fn backoff(attempt: u32, asked: Option<Duration>) -> Duration {
    let exponential = Duration::from_millis(500u64.saturating_mul(1u64 << attempt.min(6)));
    asked.unwrap_or(exponential).min(MAX_BACKOFF)
}

/// `reqwest::Error` without the URL (query strings may carry keys).
trait WithoutUrlRef {
    fn without_url_ref(&self) -> String;
}

impl WithoutUrlRef for reqwest::Error {
    fn without_url_ref(&self) -> String {
        let mut text = self.to_string();
        if let Some(url) = self.url() {
            text = text.replace(url.as_str(), "<url>");
        }
        let mut source = std::error::Error::source(self);
        while let Some(cause) = source {
            text.push_str(": ");
            text.push_str(&cause.to_string());
            source = cause.source();
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_bounded() {
        assert_eq!(backoff(1, None), Duration::from_secs(1));
        assert_eq!(backoff(10, None), MAX_BACKOFF);
        assert_eq!(backoff(1, Some(Duration::from_secs(600))), MAX_BACKOFF);
    }

    #[test]
    fn private_addresses_are_denied_by_default() {
        let limits = Limits::default();
        let client = HttpClient::new(egress_policy(false, &limits)).unwrap();
        let http = SourceHttp::new(client, Auth::None, limits, "the site");
        for url in
            ["http://127.0.0.1/x", "http://169.254.169.254/latest/meta-data", "http://[::1]/", "file:///etc/passwd"]
        {
            let error = http.check_url(url).unwrap_err();
            assert_eq!(error.kind(), "denied", "{url}: {error}");
        }
        assert!(http.check_url("https://graphrt.com/").is_ok());
    }
}
