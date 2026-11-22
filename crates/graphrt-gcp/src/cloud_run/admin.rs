//! The four Cloud Run Admin API v2 calls graphrt makes: `services.get`,
//! `services.patch` (create-or-update with `allowMissing`), `operations.get`,
//! and nothing else (no list, no delete: the gateway can only read).

use std::sync::Arc;
use std::time::Duration;

use graphrt_provider_api::http::{ProviderHttp, TokenBucket, TokenSource};
use graphrt_provider_api::{ProviderError, ProviderErrorKind};
use serde_json::Value;

use super::config::CloudRunConfig;
use crate::http_client::{ClientOptions, build_client};
use crate::rest::{Method, send_json};

/// Timeout of one Admin API request.
const ADMIN_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Admin API client of one project/region.
#[derive(Clone)]
pub(crate) struct AdminApi {
    http: ProviderHttp,
    tokens: Arc<dyn TokenSource>,
    endpoint: String,
}

impl std::fmt::Debug for AdminApi {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("AdminApi").field("endpoint", &self.endpoint).finish_non_exhaustive()
    }
}

/// `RESOURCE_EXHAUSTED` is a quota (rate limited); 409 `ABORTED` means a
/// concurrent update and is retried; the rest follows the shared table.
fn classify_admin_status(status: u16, body: &str) -> ProviderErrorKind {
    match status {
        403 if body.contains("RESOURCE_EXHAUSTED") => ProviderErrorKind::RateLimited,
        400 if body.contains("FAILED_PRECONDITION") && body.contains("in progress") => ProviderErrorKind::Transient,
        _ => ProviderError::kind_for_status(status),
    }
}

impl AdminApi {
    pub(crate) fn new(config: &CloudRunConfig, tokens: Arc<dyn TokenSource>) -> Result<Self, ProviderError> {
        config.validate()?;
        let client = build_client(ClientOptions { request_timeout: Some(ADMIN_REQUEST_TIMEOUT), bypass_proxy: false })?;
        let bucket = Arc::new(TokenBucket::new(config.admin_requests_per_second, config.admin_burst));
        let http = ProviderHttp::new(client, bucket).with_classifier(classify_admin_status);
        Ok(Self { http, tokens, endpoint: config.admin_endpoint.trim_end_matches('/').to_owned() })
    }

    /// `GET /v2/{name}`; `Ok(None)` when the service does not exist.
    pub(crate) async fn get_service(&self, name: &str) -> Result<Option<Value>, ProviderError> {
        let url = format!("{}/v2/{name}", self.endpoint);
        match send_json(&self.http, self.tokens.as_ref(), Method::Get, &url, None).await {
            Ok(service) => Ok(Some(service)),
            Err(error) if error.kind == ProviderErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    #[cfg(feature = "deploy-cloud-run")]
    /// `PATCH /v2/{name}?allowMissing=true` (creates the service when missing)
    /// and returns the long-running operation.
    pub(crate) async fn patch_service(
        &self,
        name: &str,
        body: &Value,
        validate_only: bool,
    ) -> Result<Value, ProviderError> {
        let url = format!("{}/v2/{name}?allowMissing=true&validateOnly={validate_only}", self.endpoint);
        let payload = body.to_string();
        send_json(&self.http, self.tokens.as_ref(), Method::Patch, &url, Some(&payload)).await
    }

    #[cfg(feature = "deploy-cloud-run")]
    /// Polls `operation` until `done`, then returns its `response` (the service).
    ///
    /// @throws `ProviderError` (fatal) when the operation finished with an error,
    ///   (transient) when it did not finish within `timeout`.
    pub(crate) async fn wait_operation(
        &self,
        mut operation: Value,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<Value, ProviderError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if operation.get("done").and_then(Value::as_bool) == Some(true) {
                return finished_operation(operation);
            }
            let name = operation
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| name.starts_with("projects/") && !name.contains(".."))
                .ok_or_else(|| ProviderError::fatal("Admin API returned an operation without a valid name"))?
                .to_owned();
            if tokio::time::Instant::now() + poll_interval > deadline {
                return Err(ProviderError::transient(format!("operation {name} still running after {timeout:?}")));
            }
            tokio::time::sleep(poll_interval).await;
            let url = format!("{}/v2/{name}", self.endpoint);
            operation = send_json(&self.http, self.tokens.as_ref(), Method::Get, &url, None).await?;
        }
    }
}

#[cfg(feature = "deploy-cloud-run")]
fn finished_operation(operation: Value) -> Result<Value, ProviderError> {
    if let Some(error) = operation.get("error") {
        let message = error.get("message").and_then(Value::as_str).unwrap_or("operation failed");
        let excerpt: String = message.chars().take(512).collect();
        return Err(ProviderError::fatal(format!("Cloud Run operation failed: {excerpt}")));
    }
    Ok(operation.get("response").cloned().unwrap_or(Value::Null))
}
