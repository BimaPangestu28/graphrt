//! Minimal Azure Resource Manager REST client: bearer auth with one refresh on
//! 401, a token bucket per client, jittered retries on 429/5xx, error
//! classification into [`ProviderError`], and long-running operation polling.

use crate::http::json_at;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::api::ProviderError;
use crate::auth::{ARM_SCOPE, TokenProvider};
use crate::http::{RetryPolicy, TokenBucket, build_client, is_retryable_status, read_body, retry_after};

/// ARM settings from the gateway operator's configuration.
#[derive(Clone, Debug)]
pub struct ArmConfig {
    /// `https://management.azure.com` (sovereign clouds differ). Tokens are
    /// only ever sent to this origin, including when polling operations.
    pub endpoint: String,
    /// Steady request rate of this client; ARM throttles per principal and subscription.
    pub requests_per_sec: f64,
    pub burst: u32,
    pub retry: RetryPolicy,
    pub request_timeout: Duration,
    /// Poll interval of long-running operations when ARM sends no `Retry-After`.
    pub poll_interval: Duration,
    pub operation_timeout: Duration,
}

impl Default for ArmConfig {
    fn default() -> Self {
        Self {
            endpoint: "https://management.azure.com".into(),
            requests_per_sec: 10.0,
            burst: 40,
            retry: RetryPolicy::default(),
            request_timeout: Duration::from_secs(30),
            poll_interval: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(600),
        }
    }
}

/// One ARM response: status, JSON body and the operation-tracking headers.
#[derive(Clone, Debug)]
pub struct ArmResponse {
    pub status: u16,
    pub body: Value,
    pub async_operation: Option<String>,
    pub location: Option<String>,
    pub retry_after: Option<Duration>,
}

/// ARM client shared by the provider and the deploy driver.
#[derive(Debug, Clone)]
pub struct ArmClient {
    client: reqwest::Client,
    tokens: Arc<TokenProvider>,
    bucket: Arc<TokenBucket>,
    config: Arc<ArmConfig>,
}

impl ArmClient {
    /// @throws `ProviderError::Fatal` when the HTTP client cannot be built.
    pub fn new(config: ArmConfig, tokens: Arc<TokenProvider>) -> Result<Self, ProviderError> {
        let client = build_client(config.request_timeout).map_err(ProviderError::Fatal)?;
        let bucket = Arc::new(TokenBucket::new(config.requests_per_sec, config.burst));
        Ok(Self { client, tokens, bucket, config: Arc::new(config) })
    }

    pub fn config(&self) -> &ArmConfig {
        &self.config
    }

    pub async fn get(&self, path: &str) -> Result<ArmResponse, ProviderError> {
        self.send(reqwest::Method::GET, path, None).await
    }

    pub async fn put(&self, path: &str, body: &Value) -> Result<ArmResponse, ProviderError> {
        self.send(reqwest::Method::PUT, path, Some(body)).await
    }

    pub async fn patch(&self, path: &str, body: &Value) -> Result<ArmResponse, ProviderError> {
        self.send(reqwest::Method::PATCH, path, Some(body)).await
    }

    pub async fn post(&self, path: &str) -> Result<ArmResponse, ProviderError> {
        self.send(reqwest::Method::POST, path, None).await
    }

    /// Send `path` (relative to the ARM endpoint, with `api-version`), retrying
    /// throttling and server errors and refreshing the token once on 401.
    ///
    /// @throws `ProviderError` classified from the final status and ARM error code.
    pub async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<ArmResponse, ProviderError> {
        let url = format!("{}{path}", self.config.endpoint.trim_end_matches('/'));
        self.send_url(method, &url, body).await
    }

    async fn send_url(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<&Value>,
    ) -> Result<ArmResponse, ProviderError> {
        let mut attempt = 1;
        let mut refreshed_after_401 = false;
        loop {
            self.bucket.acquire().await;
            let token = self.tokens.token(ARM_SCOPE).await?;
            let mut request = self.client.request(method.clone(), url).bearer_auth(token.expose());
            if let Some(body) = body {
                request = request.header(reqwest::header::CONTENT_TYPE, "application/json").body(body.to_string());
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(_transient) if attempt < self.config.retry.max_attempts => {
                    tokio::time::sleep(self.config.retry.delay(attempt, None)).await;
                    attempt += 1;
                    continue;
                }
                Err(error) => {
                    return Err(ProviderError::Transient(format!("ARM unreachable: {}", error.without_url())));
                }
            };
            let response = into_arm_response(response).await?;
            if response.status == 401 && !refreshed_after_401 {
                self.tokens.invalidate(ARM_SCOPE, &token);
                refreshed_after_401 = true;
                continue;
            }
            if is_retryable_status(response.status) && attempt < self.config.retry.max_attempts {
                tokio::time::sleep(self.config.retry.delay(attempt, response.retry_after)).await;
                attempt += 1;
                continue;
            }
            return classify(response);
        }
    }

    /// Wait for the long-running operation a PUT/PATCH/POST started
    /// (`Azure-AsyncOperation`, else `Location`); returns at once for a
    /// synchronous response.
    ///
    /// @throws `ProviderError::Fatal` when the operation fails, or the poll URL
    ///   points outside the ARM endpoint; `Transient` on timeout.
    pub async fn wait(&self, started: &ArmResponse) -> Result<(), ProviderError> {
        let Some(poll_url) = started.async_operation.clone().or_else(|| started.location.clone()) else {
            return Ok(());
        };
        if !poll_url.starts_with(&format!("{}/", self.config.endpoint.trim_end_matches('/'))) {
            return Err(ProviderError::Fatal("ARM operation URL is outside the configured ARM endpoint".into()));
        }
        let uses_status_body = started.async_operation.is_some();
        let deadline = Instant::now() + self.config.operation_timeout;
        let mut hint = started.retry_after;
        loop {
            if Instant::now() >= deadline {
                return Err(ProviderError::Transient("ARM operation did not finish in time".into()));
            }
            tokio::time::sleep(hint.unwrap_or(self.config.poll_interval).min(Duration::from_secs(30))).await;
            let polled = self.send_url(reqwest::Method::GET, &poll_url, None).await?;
            if !uses_status_body {
                // Location polling: 202 = still running, 200/204 = done.
                if polled.status != 202 {
                    return Ok(());
                }
            } else {
                match json_at(&polled.body, "/status").as_str().unwrap_or("InProgress") {
                    "Succeeded" => return Ok(()),
                    "Failed" | "Canceled" | "Cancelled" => {
                        return Err(ProviderError::Fatal(format!(
                            "ARM operation failed: {}",
                            error_summary(json_at(&polled.body, "/error"))
                        )));
                    }
                    _ => {}
                }
            }
            hint = polled.retry_after;
        }
    }
}

async fn into_arm_response(response: reqwest::Response) -> Result<ArmResponse, ProviderError> {
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok()).map(str::to_string);
    let async_operation = header("azure-asyncoperation");
    let location = header("location");
    let bytes = read_body(response).await.map_err(ProviderError::Transient)?;
    let body = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::Null) };
    Ok(ArmResponse { status, body, async_operation, location, retry_after: retry_after(&headers) })
}

/// Map a final ARM response to success or a [`ProviderError`] class.
fn classify(response: ArmResponse) -> Result<ArmResponse, ProviderError> {
    let status = response.status;
    if (200..300).contains(&status) {
        return Ok(response);
    }
    let error = json_at(&response.body, "/error");
    let code = json_at(error, "/code").as_str().unwrap_or("");
    let summary = format!("HTTP {status}: {}", error_summary(error));
    if is_capacity_code(code) {
        return Err(ProviderError::Capacity(summary));
    }
    Err(match status {
        401 | 403 => ProviderError::Auth(summary),
        404 => ProviderError::NotFound(summary),
        429 => ProviderError::RateLimited { retry_after: response.retry_after },
        408 | 409 | 500..=599 => ProviderError::Transient(summary),
        _ => ProviderError::Fatal(summary),
    })
}

/// ARM / Container Apps error codes that mean "no room", not "bad request".
fn is_capacity_code(code: &str) -> bool {
    let lowered = code.to_ascii_lowercase();
    ["quota", "capacity", "maxnumberof", "skunotavailable", "allocationfailed"]
        .iter()
        .any(|marker| lowered.contains(marker))
}

/// `code: message`, truncated, from an ARM `error` object.
fn error_summary(error: &Value) -> String {
    let code = json_at(error, "/code").as_str().unwrap_or("unknown");
    let message: String = json_at(error, "/message").as_str().unwrap_or("").chars().take(300).collect();
    format!("{code}: {message}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(status: u16, body: Value) -> ArmResponse {
        ArmResponse { status, body, async_operation: None, location: None, retry_after: None }
    }

    #[test]
    fn errors_are_classified_for_the_gateway() {
        let quota =
            serde_json::json!({"error": {"code": "MaxNumberOfRegionalEnvironmentsInSubExceeded", "message": "m"}});
        assert!(matches!(classify(response(400, quota)), Err(ProviderError::Capacity(_))));
        assert!(matches!(classify(response(403, Value::Null)), Err(ProviderError::Auth(_))));
        assert!(matches!(classify(response(404, Value::Null)), Err(ProviderError::NotFound(_))));
        assert!(matches!(classify(response(429, Value::Null)), Err(ProviderError::RateLimited { .. })));
        assert!(matches!(classify(response(503, Value::Null)), Err(ProviderError::Transient(_))));
        assert!(matches!(classify(response(400, Value::Null)), Err(ProviderError::Fatal(_))));
        assert!(classify(response(201, Value::Null)).is_ok());
    }
}
