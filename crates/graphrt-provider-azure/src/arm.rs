//! Minimal Azure Resource Manager REST client on the shared provider HTTP
//! layer (`ProviderHttp`: per-client token bucket, jittered retry of 429/5xx
//! honouring `Retry-After`, one token refresh on 401), plus ARM error-code
//! classification and long-running operation polling.

use std::sync::Arc;
use std::time::{Duration, Instant};

use graphrt_provider_api::http::{ProviderHttp, RetryPolicy, TokenBucket};
use graphrt_provider_api::{ProviderError, ProviderErrorKind};
use serde_json::Value;

use crate::auth::{ARM_SCOPE, ScopedToken, TokenProvider};
use crate::http::{build_client, json_at, read_body, retry_after};

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
            retry: RetryPolicy { max_attempts: 4, ..RetryPolicy::default() },
            request_timeout: Duration::from_secs(30),
            poll_interval: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(600),
        }
    }
}

/// One successful ARM response: status, JSON body and the operation-tracking headers.
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
    http: ProviderHttp,
    token: Arc<ScopedToken>,
    config: Arc<ArmConfig>,
}

impl ArmClient {
    /// @throws `ProviderError` (fatal) when the HTTP client cannot be built.
    pub fn new(config: ArmConfig, tokens: Arc<TokenProvider>) -> Result<Self, ProviderError> {
        let client = build_client(config.request_timeout).map_err(ProviderError::fatal)?;
        let bucket = Arc::new(TokenBucket::new(config.requests_per_sec, config.burst));
        let http = ProviderHttp::new(client, bucket).with_policy(config.retry).with_classifier(classify_arm_status);
        Ok(Self { http, token: Arc::new(ScopedToken::new(tokens, ARM_SCOPE)), config: Arc::new(config) })
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

    /// Send `path` (relative to the ARM endpoint, with `api-version`).
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
        let payload = body.map(Value::to_string);
        let response = self
            .http
            .send(
                |client| {
                    let request = client.request(method.clone(), url);
                    match &payload {
                        Some(payload) => {
                            request.header(reqwest::header::CONTENT_TYPE, "application/json").body(payload.clone())
                        }
                        None => request,
                    }
                },
                Some(self.token.as_ref()),
            )
            .await?;
        into_arm_response(response).await
    }

    /// Wait for the long-running operation a PUT/PATCH/POST started
    /// (`Azure-AsyncOperation`, else `Location`); returns at once for a
    /// synchronous response.
    ///
    /// @throws `ProviderError` (fatal) when the operation fails or the poll URL
    ///   points outside the ARM endpoint; transient on timeout.
    pub async fn wait(&self, started: &ArmResponse) -> Result<(), ProviderError> {
        let Some(poll_url) = started.async_operation.clone().or_else(|| started.location.clone()) else {
            return Ok(());
        };
        if !poll_url.starts_with(&format!("{}/", self.config.endpoint.trim_end_matches('/'))) {
            return Err(ProviderError::fatal("ARM operation URL is outside the configured ARM endpoint"));
        }
        let uses_status_body = started.async_operation.is_some();
        let deadline = Instant::now() + self.config.operation_timeout;
        let mut hint = started.retry_after;
        loop {
            if Instant::now() >= deadline {
                return Err(ProviderError::transient("ARM operation did not finish in time"));
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
                        return Err(ProviderError::fatal(format!(
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
    let bytes = read_body(response).await.map_err(ProviderError::transient)?;
    let body = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::Null) };
    Ok(ArmResponse { status, body, async_operation, location, retry_after: retry_after(&headers) })
}

/// ARM status + error body → error kind: quota / capacity codes are
/// `Capacity` whatever the status, the rest follows the shared status table.
pub fn classify_arm_status(status: u16, body: &str) -> ProviderErrorKind {
    let parsed: Value = serde_json::from_str(body).unwrap_or_default();
    let code = json_at(&parsed, "/error/code").as_str().unwrap_or("");
    if is_capacity_code(code) {
        return ProviderErrorKind::Capacity;
    }
    ProviderError::kind_for_status(status)
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

    #[test]
    fn errors_are_classified_for_the_gateway() {
        let quota = r#"{"error": {"code": "MaxNumberOfRegionalEnvironmentsInSubExceeded", "message": "m"}}"#;
        assert_eq!(classify_arm_status(400, quota), ProviderErrorKind::Capacity);
        assert_eq!(classify_arm_status(403, ""), ProviderErrorKind::Auth);
        assert_eq!(classify_arm_status(404, ""), ProviderErrorKind::NotFound);
        assert_eq!(classify_arm_status(429, ""), ProviderErrorKind::RateLimited);
        assert_eq!(classify_arm_status(503, "not json"), ProviderErrorKind::Transient);
        assert_eq!(classify_arm_status(400, ""), ProviderErrorKind::Fatal);
    }
}
