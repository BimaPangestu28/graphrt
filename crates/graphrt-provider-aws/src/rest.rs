//! Signed REST client shared by the AWS providers.
//!
//! One `reqwest` client, SigV4 on every request, a token bucket per API, and
//! retries with jittered backoff (the shared `graphrt-provider-api` policy) on throttling / 5xx. A 401/403 that looks
//! like stale credentials invalidates the cache and retries once; a clock-skew
//! rejection learns the offset from the server's `Date` header and retries once.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::api::ProviderError;
use crate::api::http::{JitterRng, RetryPolicy, TokenBucket};
use crate::auth::sigv4::{self, SigningRequest};
use crate::auth::{AuthError, CredentialChain};

const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Retry and rate-limit settings of one AWS API (shared retry policy of
/// `graphrt-provider-api` plus a token bucket).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ApiLimits {
    pub retry: RetryPolicy,
    /// Sustained requests per second the client allows itself.
    pub rate_per_sec: f64,
    /// Bucket size (burst).
    pub burst: u32,
}

impl Default for ApiLimits {
    /// ECS and Lambda control-plane APIs throttle at roughly 20 req/s per account and region.
    fn default() -> Self {
        Self {
            retry: RetryPolicy {
                max_attempts: 4,
                base_delay: Duration::from_millis(200),
                max_delay: Duration::from_secs(5),
            },
            rate_per_sec: 20.0,
            burst: 40,
        }
    }
}

/// Endpoint and signing scope of one AWS API.
#[derive(Clone, Debug)]
pub struct ServiceEndpoint {
    /// `https://ecs.eu-west-1.amazonaws.com` or an operator override (VPC endpoint, fake server).
    pub base_url: String,
    pub region: String,
    /// SigV4 service name (`ecs`, `lambda`).
    pub service: String,
}

impl ServiceEndpoint {
    /// The public regional endpoint `https://<service>.<region>.amazonaws.com`.
    pub fn regional(service: &str, region: &str) -> Self {
        Self {
            base_url: format!("https://{service}.{region}.amazonaws.com"),
            region: region.to_owned(),
            service: service.to_owned(),
        }
    }

    fn host(&self) -> &str {
        let rest = self.base_url.split_once("://").map_or(self.base_url.as_str(), |(_, r)| r);
        rest.split('/').next().unwrap_or_default()
    }
}

/// One request before signing.
#[derive(Debug)]
pub struct AwsRequest<'a> {
    pub method: &'a str,
    /// Path as sent on the wire (each segment already percent-encoded).
    pub path: &'a str,
    pub query: &'a [(&'a str, &'a str)],
    /// Extra signed headers (`content-type`, `x-amz-target`, …).
    pub headers: &'a [(&'a str, &'a str)],
    pub body: &'a [u8],
    /// Whether a 5xx / timeout may be retried (false for Lambda invokes: the
    /// function may already have run).
    pub idempotent: bool,
}

/// An AWS error response, classified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AwsErrorBody {
    pub status: u16,
    /// Error code without the namespace (`ThrottlingException`).
    pub code: String,
    pub message: String,
}

impl AwsErrorBody {
    fn parse(status: u16, header_type: Option<&str>, body: &str) -> Self {
        let json: Option<serde_json::Value> = serde_json::from_str(body).ok();
        let field = |names: &[&str]| -> Option<String> {
            let json = json.as_ref()?;
            names.iter().find_map(|n| json.get(*n).and_then(|v| v.as_str()).map(str::to_owned))
        };
        let raw_code = header_type
            .map(str::to_owned)
            .or_else(|| field(&["__type", "code", "Code"]))
            .or_else(|| crate::auth::xml_text(body, "Code"))
            .unwrap_or_default();
        // `com.amazon.coral#ThrottlingException` / `ThrottlingException:http://…`.
        let code = raw_code.rsplit('#').next().unwrap_or_default().split(':').next().unwrap_or_default().to_owned();
        let message = field(&["message", "Message", "errorMessage"])
            .or_else(|| crate::auth::xml_text(body, "Message"))
            .unwrap_or_default();
        Self { status, code, message }
    }

    fn is_throttling(&self) -> bool {
        self.status == 429
            || matches!(
                self.code.as_str(),
                "ThrottlingException" | "Throttling" | "TooManyRequestsException" | "RequestLimitExceeded"
            )
    }

    fn is_clock_skew(&self) -> bool {
        matches!(self.code.as_str(), "RequestTimeTooSkewed" | "RequestExpired" | "SignatureDoesNotMatch")
            && self.message.to_ascii_lowercase().contains("expired")
            || self.code == "RequestTimeTooSkewed"
            || self.message.contains("Signature expired")
    }

    fn is_stale_credentials(&self) -> bool {
        matches!(self.status, 401 | 403)
            && matches!(
                self.code.as_str(),
                "ExpiredToken"
                    | "ExpiredTokenException"
                    | "UnrecognizedClientException"
                    | "InvalidClientTokenId"
                    | "InvalidSignatureException"
                    | "IncompleteSignature"
                    | ""
            )
    }

    fn summary(&self) -> String {
        format!("{} (HTTP {}): {}", self.code, self.status, self.message)
    }

    /// The `ProviderError` for a final (non-retried) failure.
    pub fn into_provider_error(self, retry_after_ms: u64) -> ProviderError {
        if self.is_throttling() {
            return ProviderError::rate_limited(self.summary())
                .with_retry_after(Some(Duration::from_millis(retry_after_ms)));
        }
        if matches!(self.status, 401 | 403) || self.code.contains("AccessDenied") {
            return ProviderError::auth(self.summary());
        }
        if self.status == 404 || self.code.contains("NotFound") {
            return ProviderError::not_found(self.summary());
        }
        if self.status >= 500 {
            return ProviderError::transient(self.summary());
        }
        ProviderError::fatal(self.summary())
    }
}

/// Signed client for one AWS API.
#[derive(Clone)]
pub struct AwsRestClient {
    http: reqwest::Client,
    chain: Arc<CredentialChain>,
    endpoint: ServiceEndpoint,
    limits: ApiLimits,
    bucket: Arc<TokenBucket>,
    jitter: Arc<JitterRng>,
}

impl fmt::Debug for AwsRestClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AwsRestClient").field("endpoint", &self.endpoint).field("limits", &self.limits).finish()
    }
}

fn auth_error(error: AuthError) -> ProviderError {
    ProviderError::auth(error.to_string())
}

fn retry_after_ms(response: &reqwest::Response) -> Option<u64> {
    let value = response.headers().get("retry-after")?.to_str().ok()?;
    value.trim().parse::<u64>().ok().map(|secs| secs.saturating_mul(1000))
}

/// Read at most `MAX_ERROR_BODY_BYTES` of an error body.
async fn read_error(response: reqwest::Response) -> AwsErrorBody {
    let status = response.status().as_u16();
    let header_type = response.headers().get("x-amzn-errortype").and_then(|v| v.to_str().ok()).map(str::to_owned);
    let body = response.bytes().await.unwrap_or_default();
    let body = String::from_utf8_lossy(body.get(..body.len().min(MAX_ERROR_BODY_BYTES)).unwrap_or_default());
    AwsErrorBody::parse(status, header_type.as_deref(), &body)
}

impl AwsRestClient {
    /// A client for `endpoint`, signing with credentials from `chain`.
    ///
    /// @throws `ProviderError::Fatal` when the HTTP client cannot be built.
    pub fn new(
        chain: Arc<CredentialChain>,
        endpoint: ServiceEndpoint,
        limits: ApiLimits,
    ) -> Result<Self, ProviderError> {
        let _already_installed = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| ProviderError::fatal(format!("cannot build HTTP client: {e}")))?;
        let bucket = Arc::new(TokenBucket::new(limits.rate_per_sec.max(0.001), limits.burst.max(1)));
        Ok(Self { http, chain, endpoint, limits, bucket, jitter: Arc::new(JitterRng::from_entropy()) })
    }

    pub fn endpoint(&self) -> &ServiceEndpoint {
        &self.endpoint
    }

    async fn sign_and_send(&self, request: &AwsRequest<'_>) -> Result<reqwest::Response, SendError> {
        let credentials = self.chain.credentials().await.map_err(SendError::Credentials)?;
        let date = sigv4::amz_date(self.chain.clock().now_unix());
        let payload_hash = sigv4::sha256_hex(request.body);
        let host = self.endpoint.host();
        let mut headers: Vec<(&str, &str)> = vec![("host", host), ("x-amz-date", &date)];
        if let Some(token) = &credentials.session_token {
            headers.push(("x-amz-security-token", token));
        }
        headers.extend_from_slice(request.headers);
        let authorization = sigv4::authorization(
            &credentials,
            &SigningRequest {
                method: request.method,
                path: request.path,
                query: request.query,
                headers: &headers,
                payload_hash: &payload_hash,
                region: &self.endpoint.region,
                service: &self.endpoint.service,
                amz_date: &date,
                double_encode_path: true,
            },
        );
        let query: Vec<String> = request
            .query
            .iter()
            .map(|(k, v)| format!("{}={}", sigv4::uri_encode(k, false), sigv4::uri_encode(v, false)))
            .collect();
        let mut url = format!("{}{}", self.endpoint.base_url.trim_end_matches('/'), request.path);
        if !query.is_empty() {
            url.push('?');
            url.push_str(&query.join("&"));
        }
        let method = reqwest::Method::from_bytes(request.method.as_bytes()).unwrap_or(reqwest::Method::POST);
        let mut builder = self.http.request(method, url).header("authorization", authorization);
        for (name, value) in headers.iter().filter(|(n, _)| *n != "host") {
            builder = builder.header(*name, *value);
        }
        builder.body(request.body.to_vec()).send().await.map_err(SendError::Http)
    }

    /// Send `request`; returns the 2xx response (body not yet read).
    ///
    /// @throws `ProviderError` classified per plan §6 after retries.
    pub async fn send(&self, request: &AwsRequest<'_>) -> Result<reqwest::Response, ProviderError> {
        let mut refreshed_credentials = false;
        let mut corrected_clock = false;
        let mut attempt = 0;
        loop {
            self.bucket.acquire().await;
            attempt += 1;
            let last_attempt = attempt >= self.limits.retry.max_attempts.max(1);
            let response = match self.sign_and_send(request).await {
                Ok(r) => r,
                Err(SendError::Credentials(error)) => return Err(auth_error(error)),
                Err(SendError::Http(error)) => {
                    let retryable = request.idempotent || error.is_connect();
                    if last_attempt || !retryable {
                        return Err(ProviderError::transient(format!(
                            "{} request failed: {error}",
                            self.endpoint.service
                        )));
                    }
                    tokio::time::sleep(self.limits.retry.delay(attempt - 1, None, &self.jitter)).await;
                    continue;
                }
            };
            if response.status().is_success() {
                return Ok(response);
            }
            let retry_after = retry_after_ms(&response);
            let server_date =
                response.headers().get("date").and_then(|v| v.to_str().ok()).and_then(crate::auth::parse_http_date);
            let error = read_error(response).await;
            if let Some(server) = server_date.filter(|_| error.is_clock_skew() && !corrected_clock) {
                tracing::warn!(
                    offset_secs = server as i64 - self.chain.clock().now_unix() as i64,
                    "aws clock skew corrected"
                );
                self.chain.clock().correct_to(server);
                corrected_clock = true;
                continue;
            }
            if error.is_stale_credentials() && !refreshed_credentials {
                self.chain.invalidate().await;
                refreshed_credentials = true;
                continue;
            }
            let retryable = error.is_throttling() || (request.idempotent && error.status >= 500);
            if retryable && !last_attempt {
                let backoff = self.limits.retry.delay(attempt - 1, None, &self.jitter);
                let wait = retry_after.map_or(backoff, |ms| Duration::from_millis(ms).max(backoff));
                tokio::time::sleep(wait).await;
                continue;
            }
            let retry_ms = retry_after.unwrap_or_else(|| {
                u64::try_from(self.limits.retry.delay(attempt - 1, None, &self.jitter).as_millis()).unwrap_or(u64::MAX)
            });
            return Err(error.into_provider_error(retry_ms.max(1)));
        }
    }

    /// Call an AWS JSON 1.1 API (`X-Amz-Target: <target>`), returning the parsed body.
    ///
    /// @throws `ProviderError` as `send`, or `Transient` when the body is not JSON.
    pub async fn json_11(&self, target: &str, body: &serde_json::Value) -> Result<serde_json::Value, ProviderError> {
        let payload =
            serde_json::to_vec(body).map_err(|e| ProviderError::fatal(format!("cannot encode request: {e}")))?;
        let headers = [("content-type", "application/x-amz-json-1.1"), ("x-amz-target", target)];
        let request =
            AwsRequest { method: "POST", path: "/", query: &[], headers: &headers, body: &payload, idempotent: true };
        let response = self.send(&request).await?;
        let bytes = response
            .bytes()
            .await
            .map_err(|e| ProviderError::transient(format!("{} response body: {e}", self.endpoint.service)))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| ProviderError::transient(format!("{target}: response is not JSON: {e}")))
    }
}

/// Why one attempt did not produce a response.
#[derive(Debug)]
enum SendError {
    Credentials(AuthError),
    Http(reqwest::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ProviderErrorKind;

    #[test]
    fn error_codes_are_normalised() {
        let e = AwsErrorBody::parse(
            400,
            None,
            r#"{"__type":"com.amazon.coral.availability#ThrottlingException","message":"Rate exceeded"}"#,
        );
        assert_eq!(e.code, "ThrottlingException");
        assert!(e.is_throttling());
        let error = e.clone().into_provider_error(5);
        assert_eq!(error.kind, ProviderErrorKind::RateLimited);
        assert_eq!(error.retry_after, Some(Duration::from_millis(5)));

        let e = AwsErrorBody::parse(403, Some("ExpiredTokenException:http://internal"), "{}");
        assert_eq!(e.code, "ExpiredTokenException");
        assert!(e.is_stale_credentials());

        let e = AwsErrorBody::parse(
            403,
            None,
            r#"{"__type":"InvalidSignatureException","message":"Signature expired: 20150830T123600Z is now earlier than ..."}"#,
        );
        assert!(e.is_clock_skew());

        let e =
            AwsErrorBody::parse(400, None, r#"{"__type":"ClusterNotFoundException","message":"Cluster not found."}"#);
        assert_eq!(e.into_provider_error(1).kind, ProviderErrorKind::NotFound);

        let e = AwsErrorBody::parse(400, None, r#"{"__type":"AccessDeniedException","Message":"no"}"#);
        assert_eq!(e.into_provider_error(1).kind, ProviderErrorKind::Auth);

        let e = AwsErrorBody::parse(500, None, "");
        assert_eq!(e.into_provider_error(1).kind, ProviderErrorKind::Transient);
    }
}
