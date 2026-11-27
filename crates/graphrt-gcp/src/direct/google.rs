//! The one request path of a direct deploy to every Google API it calls
//! (Cloud Run, Cloud Storage, Secret Manager, IAM, Artifact Registry, Service
//! Usage): bearer token with one refresh on 401, retry of 429/5xx and network
//! errors, the quota project of a person's credentials, and errors that say
//! what to fix (an API that is off, billing, a missing permission, an
//! organization policy).

use std::sync::Arc;
use std::time::Duration;

use graphrt_provider_api::http::{TokenSource, parse_retry_after};
use serde_json::Value;

use super::{DirectError, ErrorCode};
use crate::http_client::{ClientOptions, build_client};

/// Timeout of one API call.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Timeout of one upload of the agent file into Cloud Storage.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(900);
/// Attempts of a call that keeps failing with 429, 5xx or a network error.
const MAX_ATTEMPTS: u32 = 4;
/// Longest `Retry-After` honoured.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);
/// Longest excerpt of Google's message kept in an error.
const MESSAGE_CHARS: usize = 600;

/// Base URLs of the Google APIs (operator configuration or tests, never input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    /// Cloud Run Admin API v2.
    pub run: String,
    /// Cloud Storage JSON API (`/storage/v1`, `/upload/storage/v1`).
    pub storage: String,
    /// Secret Manager v1.
    pub secret_manager: String,
    /// IAM v1 (service accounts).
    pub iam: String,
    /// Artifact Registry v1.
    pub artifact_registry: String,
    /// Service Usage v1 (turning APIs on).
    pub service_usage: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            run: "https://run.googleapis.com".to_owned(),
            storage: "https://storage.googleapis.com".to_owned(),
            secret_manager: "https://secretmanager.googleapis.com".to_owned(),
            iam: "https://iam.googleapis.com".to_owned(),
            artifact_registry: "https://artifactregistry.googleapis.com".to_owned(),
            service_usage: "https://serviceusage.googleapis.com".to_owned(),
        }
    }
}

impl Endpoints {
    /// Every API at one base URL (a fake Google in tests).
    pub fn all_at(base_url: &str) -> Self {
        let base = base_url.trim_end_matches('/').to_owned();
        Self {
            run: base.clone(),
            storage: base.clone(),
            secret_manager: base.clone(),
            iam: base.clone(),
            artifact_registry: base.clone(),
            service_usage: base,
        }
    }

    pub(super) fn all(&self) -> [&str; 6] {
        [&self.run, &self.storage, &self.secret_manager, &self.iam, &self.artifact_registry, &self.service_usage]
    }
}

/// HTTP method of a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Method {
    Get,
    Post,
    Patch,
    Put,
    Delete,
}

/// Body of a call.
#[derive(Debug, Clone, Copy)]
pub(super) enum Body<'a> {
    Empty,
    Json(&'a Value),
    /// An upload: content type and bytes.
    Bytes(&'a str, &'a [u8]),
}

/// A failed call, with what Google said about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ApiError {
    /// HTTP status, 0 when the API was unreachable.
    pub(super) status: u16,
    /// `error.status` (`PERMISSION_DENIED`, `FAILED_PRECONDITION`...).
    pub(super) google_status: String,
    /// `ErrorInfo.reason` (`SERVICE_DISABLED`, `BILLING_DISABLED`...).
    pub(super) reason: String,
    /// `error.message`.
    pub(super) message: String,
    /// `ErrorInfo.metadata.activationUrl` of an API that is off.
    pub(super) activation_url: Option<String>,
    /// `ErrorInfo.metadata.service` of an API that is off (`run.googleapis.com`).
    pub(super) service: Option<String>,
}

impl ApiError {
    fn unreachable(detail: &str) -> Self {
        Self {
            status: 0,
            google_status: String::new(),
            reason: String::new(),
            message: detail.to_owned(),
            activation_url: None,
            service: None,
        }
    }

    pub(super) fn is_not_found(&self) -> bool {
        self.status == 404
    }

    pub(super) fn is_conflict(&self) -> bool {
        self.status == 409
    }

    /// The error a user sees: what failed, why, and what to do about it.
    ///
    /// @param what - the action, e.g. "create the Cloud Storage bucket b" (&str)
    /// @param context - project and principal, for the fix (&Context)
    pub(super) fn to_direct(&self, what: &str, context: &Context) -> DirectError {
        let project = &context.project;
        let lower = self.message.to_ascii_lowercase();
        if self.reason == "SERVICE_DISABLED" || lower.contains("has not been used in project") {
            let service = self.service.clone().unwrap_or_else(|| "the API".to_owned());
            let url = self.activation_url.clone().unwrap_or_else(|| {
                format!("https://console.cloud.google.com/apis/library/{service}?project={project}")
            });
            return DirectError::new(
                ErrorCode::ApiDisabled,
                format!(
                    "could not {what}: {service} is turned off in project {project}. Turn it on with `gcloud \
                     services enable {service} --project {project}` or at {url}, wait a minute, then deploy again"
                ),
            );
        }
        if self.reason == "BILLING_DISABLED" || (lower.contains("billing") && !lower.contains("permission")) {
            return DirectError::new(
                ErrorCode::BillingDisabled,
                format!(
                    "could not {what}: project {project} has no active billing account ({}). Link one at \
                     https://console.cloud.google.com/billing/linkedaccount?project={project}, then deploy again",
                    excerpt(&self.message)
                ),
            );
        }
        if lower.contains("constraints/") || lower.contains("organization policy") || lower.contains("org policy") {
            return DirectError::new(
                ErrorCode::OrgPolicy,
                format!(
                    "could not {what}: an organization policy of project {project} forbids it ({}). Ask the \
                     administrator of your Google Cloud organization to allow it for this project, or use a \
                     project outside the organization",
                    excerpt(&self.message)
                ),
            );
        }
        match self.status {
            0 => DirectError::new(
                ErrorCode::GcpUnavailable,
                format!(
                    "could not {what}: Google Cloud is unreachable ({}). Check the network and retry",
                    self.message
                ),
            ),
            401 => DirectError::new(
                ErrorCode::Unauthorized,
                format!(
                    "could not {what}: Google rejected the credentials of {} (expired or revoked{}). Sign in again \
                     (gcloud auth application-default login, or Google sign in in the app), or pass a fresh token",
                    context.principal,
                    if self.message.is_empty() { String::new() } else { format!(": {}", excerpt(&self.message)) }
                ),
            ),
            403 if self.google_status == "RESOURCE_EXHAUSTED" => quota(what, &self.message),
            403 => DirectError::new(
                ErrorCode::PermissionDenied,
                format!(
                    "could not {what}: {} lacks a permission in project {project} ({}). {}",
                    context.principal,
                    excerpt(&self.message),
                    role_hint(&self.message)
                ),
            ),
            404 => DirectError::new(ErrorCode::NotFound, format!("could not {what}: {}", excerpt(&self.message))),
            429 => quota(what, &self.message),
            500..=599 => DirectError::new(
                ErrorCode::GcpUnavailable,
                format!(
                    "could not {what}: Google Cloud answered HTTP {} ({}). Retry in a minute",
                    self.status,
                    excerpt(&self.message)
                ),
            ),
            status => DirectError::new(
                ErrorCode::GcpApi,
                format!("could not {what}: Google Cloud answered HTTP {status}: {}", excerpt(&self.message)),
            ),
        }
    }
}

fn quota(what: &str, message: &str) -> DirectError {
    DirectError::new(
        ErrorCode::RateLimited,
        format!(
            "could not {what}: a Google Cloud quota is used up ({}). Retry later or ask for more quota",
            excerpt(message)
        ),
    )
}

fn excerpt(message: &str) -> String {
    let text: String = message.chars().take(MESSAGE_CHARS).collect();
    text.trim_end_matches('.').to_owned()
}

/// The role that grants the permission a message names (`Permission 'run.services.create' denied`).
fn role_hint(message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    let role = [
        ("run.", "Cloud Run Admin (roles/run.admin)"),
        ("storage.", "Storage Admin (roles/storage.admin)"),
        ("secretmanager.", "Secret Manager Admin (roles/secretmanager.admin)"),
        ("iam.serviceaccounts.actas", "Service Account User (roles/iam.serviceAccountUser)"),
        ("iam.serviceaccounts.", "Service Account Admin (roles/iam.serviceAccountAdmin)"),
        ("artifactregistry.", "Artifact Registry Administrator (roles/artifactregistry.admin)"),
        ("serviceusage.", "Service Usage Admin (roles/serviceusage.serviceUsageAdmin)"),
    ]
    .into_iter()
    .find_map(|(prefix, role)| lower.contains(prefix).then_some(role));
    match role {
        Some(role) => format!(
            "Ask an owner of the project to give you the role {role}, or Owner (docs/deploy-cloud-run.md lists every role)"
        ),
        None => "Ask an owner of the project to give you the roles in docs/deploy-cloud-run.md, or Owner".to_owned(),
    }
}

/// Who calls and for which project, for error messages and the quota project.
#[derive(Debug, Clone)]
pub(super) struct Context {
    pub(super) project: String,
    pub(super) principal: String,
}

/// Authenticated client of every Google API of a direct deploy.
#[derive(Clone)]
pub(super) struct GoogleClient {
    http: reqwest::Client,
    uploads: reqwest::Client,
    tokens: Arc<dyn TokenSource>,
    /// Sent as `x-goog-user-project` with a person's credentials.
    user_project: Option<String>,
    pub(super) endpoints: Endpoints,
    pub(super) context: Context,
    /// Pause between retries (shortened in tests).
    retry_base: Duration,
}

impl std::fmt::Debug for GoogleClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GoogleClient")
            .field("endpoints", &self.endpoints)
            .field("context", &self.context)
            .finish_non_exhaustive()
    }
}

impl GoogleClient {
    pub(super) fn new(
        tokens: Arc<dyn TokenSource>,
        user_project: Option<String>,
        endpoints: Endpoints,
        context: Context,
        retry_base: Duration,
    ) -> Result<Self, DirectError> {
        let build = |timeout| {
            build_client(ClientOptions { request_timeout: Some(timeout), bypass_proxy: false })
                .map_err(|error| DirectError::new(ErrorCode::Internal, error.message))
        };
        Ok(Self {
            http: build(REQUEST_TIMEOUT)?,
            uploads: build(UPLOAD_TIMEOUT)?,
            tokens,
            user_project,
            endpoints,
            context,
            retry_base,
        })
    }

    /// One call; the parsed JSON body (`Null` when empty).
    pub(super) async fn call(&self, method: Method, url: &str, body: Body<'_>) -> Result<Value, Box<ApiError>> {
        let mut refreshed = false;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let token = self
                .tokens
                .token(refreshed)
                .await
                .map_err(|error| Box::new(ApiError { status: 401, ..ApiError::unreachable(&error.message) }))?;
            let client = if matches!(body, Body::Bytes(..)) { &self.uploads } else { &self.http };
            let mut request = match method {
                Method::Get => client.get(url),
                Method::Post => client.post(url),
                Method::Patch => client.patch(url),
                Method::Put => client.put(url),
                Method::Delete => client.delete(url),
            }
            .bearer_auth(token.expose());
            if let Some(project) = &self.user_project {
                request = request.header("x-goog-user-project", project);
            }
            request = match body {
                Body::Empty => request,
                Body::Json(value) => request.header("content-type", "application/json").body(value.to_string()),
                Body::Bytes(content_type, bytes) => request.header("content-type", content_type).body(bytes.to_vec()),
            };
            let response = match request.send().await {
                Ok(response) => response,
                Err(_) if attempt < MAX_ATTEMPTS => {
                    tokio::time::sleep(self.backoff(attempt, None)).await;
                    continue;
                }
                Err(error) => {
                    let kind = if error.is_timeout() { "timeout" } else { "connect error" };
                    return Err(Box::new(ApiError::unreachable(kind)));
                }
            };
            let status = response.status().as_u16();
            let retry_after =
                response.headers().get("retry-after").and_then(|value| value.to_str().ok()).and_then(parse_retry_after);
            let bytes = response.bytes().await.unwrap_or_default();
            if (200..300).contains(&status) {
                if bytes.is_empty() {
                    return Ok(Value::Null);
                }
                return serde_json::from_slice(&bytes)
                    .map_err(|_| Box::new(ApiError { status, ..ApiError::unreachable("the answer is not JSON") }));
            }
            if status == 401 && !refreshed {
                refreshed = true;
                continue;
            }
            if matches!(status, 429 | 500 | 502 | 503 | 504) && attempt < MAX_ATTEMPTS {
                tokio::time::sleep(self.backoff(attempt, retry_after)).await;
                continue;
            }
            return Err(Box::new(parse_error(status, &bytes)));
        }
    }

    fn backoff(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        let exponential = self.retry_base.saturating_mul(1 << attempt.min(5));
        retry_after.map_or(exponential, |wait| wait.min(MAX_RETRY_AFTER).max(exponential))
    }

    /// [`GoogleClient::call`], failing with what the user should do.
    pub(super) async fn send(
        &self,
        method: Method,
        url: &str,
        body: Body<'_>,
        what: &str,
    ) -> Result<Value, DirectError> {
        self.call(method, url, body).await.map_err(|error| error.to_direct(what, &self.context))
    }

    /// `GET url`, `Ok(None)` on 404.
    pub(super) async fn get_optional(&self, url: &str, what: &str) -> Result<Option<Value>, DirectError> {
        match self.call(Method::Get, url, Body::Empty).await {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.is_not_found() => Ok(None),
            Err(error) => Err(error.to_direct(what, &self.context)),
        }
    }

    /// Polls a long-running operation at `{base}/{version}/{name}` until it is done.
    ///
    /// @returns The operation's `response`
    /// @throws DirectError - the operation failed (its message), or did not finish in `timeout`
    pub(super) async fn wait_operation(
        &self,
        base: &str,
        api_version: &str,
        mut operation: Value,
        timeout: Duration,
        poll: Duration,
        what: &str,
    ) -> Result<Value, DirectError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if operation.get("done").and_then(Value::as_bool) == Some(true) {
                if let Some(error) = operation.get("error") {
                    let message = error.get("message").and_then(Value::as_str).unwrap_or("the operation failed");
                    return Err(ApiError {
                        status: error.get("code").and_then(Value::as_u64).map_or(400, grpc_to_http),
                        google_status: String::new(),
                        reason: String::new(),
                        message: message.to_owned(),
                        activation_url: None,
                        service: None,
                    }
                    .to_direct(what, &self.context));
                }
                return Ok(operation.get("response").cloned().unwrap_or(Value::Null));
            }
            let Some(name) = operation
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.contains("..") && !name.contains('?'))
                .map(str::to_owned)
            else {
                return Ok(operation.get("response").cloned().unwrap_or(Value::Null));
            };
            if tokio::time::Instant::now() + poll > deadline {
                return Err(DirectError::new(
                    ErrorCode::StartTimeout,
                    format!("could not {what}: still running after {} s", timeout.as_secs()),
                ));
            }
            tokio::time::sleep(poll).await;
            let url = format!("{base}/{api_version}/{name}");
            operation = self.send(Method::Get, &url, Body::Empty, what).await?;
        }
    }
}

/// gRPC code of a failed operation as an HTTP status (google.rpc.Code).
fn grpc_to_http(code: u64) -> u16 {
    match code {
        3 | 9 | 11 => 400,
        5 => 404,
        6 | 10 => 409,
        7 => 403,
        8 => 429,
        16 => 401,
        14 => 503,
        4 => 504,
        _ => 500,
    }
}

/// Google's error body: `{"error": {"code", "message", "status", "details": [{"reason", "metadata"}]}}`,
/// or Cloud Storage's `{"error": {"code", "message", "errors": [{"reason"}]}}`.
fn parse_error(status: u16, bytes: &[u8]) -> ApiError {
    let body: Value = serde_json::from_slice(bytes).unwrap_or(Value::Null);
    let error = body.get("error").unwrap_or(&Value::Null);
    let text = |value: Option<&Value>| value.and_then(Value::as_str).unwrap_or_default().to_owned();
    let details = error.get("details").and_then(Value::as_array).cloned().unwrap_or_default();
    let info = details.iter().find(|detail| detail.get("reason").is_some());
    let metadata = info.and_then(|info| info.get("metadata"));
    let message = match error.get("message").and_then(Value::as_str) {
        Some(message) => message.to_owned(),
        None if error.is_string() => text(Some(error)),
        None => String::from_utf8_lossy(bytes).chars().take(MESSAGE_CHARS).collect(),
    };
    let reason = info
        .map(|info| text(info.get("reason")))
        .or_else(|| {
            error
                .get("errors")
                .and_then(Value::as_array)
                .and_then(|errors| errors.first())
                .map(|e| text(e.get("reason")))
        })
        .unwrap_or_default();
    ApiError {
        status,
        google_status: text(error.get("status")),
        reason,
        message,
        activation_url: metadata
            .and_then(|metadata| metadata.get("activationUrl"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        service: metadata.and_then(|metadata| metadata.get("service")).and_then(Value::as_str).map(str::to_owned),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn context() -> Context {
        Context { project: "my-project".to_owned(), principal: "user someone@example.com".to_owned() }
    }

    fn error_of(status: u16, body: &Value) -> DirectError {
        parse_error(status, body.to_string().as_bytes()).to_direct("create the service", &context())
    }

    #[test]
    fn an_api_that_is_off_says_how_to_turn_it_on() {
        let body = json!({"error": {"code": 403, "status": "PERMISSION_DENIED",
            "message": "Cloud Run Admin API has not been used in project 123 before or it is disabled.",
            "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "SERVICE_DISABLED",
                "metadata": {"service": "run.googleapis.com", "activationUrl": "https://console.developers.google.com/apis/api/run.googleapis.com/overview?project=123"}}]}});
        let error = error_of(403, &body);
        assert_eq!(error.code, ErrorCode::ApiDisabled);
        assert!(
            error.message.contains("gcloud services enable run.googleapis.com --project my-project"),
            "{}",
            error.message
        );
        assert!(error.message.contains("https://console.developers.google.com/apis/api/run.googleapis.com"));
    }

    #[test]
    fn billing_permission_and_policy_errors_name_the_fix() {
        let billing = json!({"error": {"code": 403, "status": "PERMISSION_DENIED",
            "message": "Billing account for project '123' is not found. Billing must be enabled for activation of service(s)",
            "details": [{"reason": "BILLING_DISABLED"}]}});
        let error = error_of(403, &billing);
        assert_eq!(error.code, ErrorCode::BillingDisabled);
        assert!(error.message.contains("billing/linkedaccount?project=my-project"));
        let permission = json!({"error": {"code": 403, "status": "PERMISSION_DENIED",
            "message": "Permission 'run.services.create' denied on resource 'projects/my-project/locations/r/services/s'"}});
        let error = error_of(403, &permission);
        assert_eq!(error.code, ErrorCode::PermissionDenied);
        assert!(error.message.contains("roles/run.admin") && error.message.contains("someone@example.com"));
        let policy = json!({"error": {"code": 400, "status": "FAILED_PRECONDITION",
            "message": "Operation denied by org policy on resource: [\"constraints/run.managed.requireInvokerIam\"]"}});
        assert_eq!(error_of(400, &policy).code, ErrorCode::OrgPolicy);
        let storage = json!({"error": {"code": 409, "message": "The requested bucket name is not available.", "errors": [{"reason": "conflict"}]}});
        let parsed = parse_error(409, storage.to_string().as_bytes());
        assert!(parsed.is_conflict() && parsed.reason == "conflict");
        assert_eq!(error_of(401, &json!({})).code, ErrorCode::Unauthorized);
        assert_eq!(error_of(503, &json!({})).code, ErrorCode::GcpUnavailable);
    }

    #[test]
    fn operation_codes_map_to_statuses() {
        assert_eq!(grpc_to_http(7), 403);
        assert_eq!(grpc_to_http(9), 400);
        assert_eq!(grpc_to_http(5), 404);
    }
}
