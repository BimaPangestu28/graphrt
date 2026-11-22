//! `provider-lambda`: short synchronous runs on AWS Lambda.
//!
//! The gateway calls `InvokeWithResponseStream` directly (SigV4, no Function
//! URL) and holds the stream; Lambda Web Adapter inside the function turns the
//! invoke into an HTTP request to `graphrt serve --trust-gateway`.
//!
//! Limits that shape this provider (plan §5):
//! - a run may last at most [`MAX_RUN_SECS`] (14 min, under Lambda's 15 min cap);
//! - there is practically no grace period, so a run that outlives the cap is
//!   never preempted: it ends with an explicit [`InvokeError::PlatformTimeout`]
//!   (`failed{platform_timeout}`);
//! - the environment is frozen after the response, so async / webhook / cron runs
//!   are rejected at deploy time ([`validate_deployment`]);
//! - SSE only works with `AWS_LWA_INVOKE_MODE=response_stream`; a buffered reply
//!   is detected and reported as [`StreamMode::Buffered`] (conformance C6).

pub mod eventstream;
pub mod lwa;

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::Value;

pub use self::lwa::{AgentRequest, ResponseHead, StreamMode};

use crate::api::{
    Addressing, BoxFuture, Capabilities, DeployError, DeploymentRef, Endpoint, EndpointAddress, InvokeRequest,
    InvokeResponse, Provider, ProviderError, ReleaseMode, ReplicaPhase, ReplicaRef, ReplicaState, Scaler,
};
use crate::auth::CredentialChain;
use crate::auth::sigv4::uri_encode;
use crate::rest::{ApiLimits, AwsRequest, AwsRestClient, ServiceEndpoint};

/// Longest run the gateway sends to Lambda (Lambda's hard cap is 900 s).
pub const MAX_RUN_SECS: u64 = 840;
/// Lambda's maximum function timeout.
pub const LAMBDA_MAX_TIMEOUT_SECS: u64 = 900;

/// One Lambda function behind a deployment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LambdaFunction {
    /// Function name or ARN.
    pub function_name: String,
}

/// Configuration of the Lambda provider.
#[derive(Clone, Debug)]
pub struct LambdaConfig {
    pub endpoint: ServiceEndpoint,
    pub limits: ApiLimits,
    /// Deployment name → function, used when `DeploymentRef::target` is empty. The
    /// deployment version is the qualifier (published version or alias).
    pub functions: HashMap<String, LambdaFunction>,
    /// Gateway-side cap on one run (≤ [`MAX_RUN_SECS`]).
    pub max_run_secs: u64,
    /// A reply whose whole body arrives in one chunk after at least this long is
    /// reported as buffered (the agent's 15 s heartbeat never got through).
    pub buffering_threshold: Duration,
}

impl LambdaConfig {
    /// Defaults for `region`: public endpoint, 840 s cap, 20 s buffering threshold.
    pub fn new(region: &str) -> Self {
        Self {
            endpoint: ServiceEndpoint::regional("lambda", region),
            limits: ApiLimits::default(),
            functions: HashMap::new(),
            max_run_secs: MAX_RUN_SECS,
            buffering_threshold: Duration::from_secs(20),
        }
    }
}

/// How a run is triggered (deploy-time validation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunTrigger {
    /// A client waits for the (streamed) result.
    Sync,
    Async,
    Webhook,
    Cron,
}

/// What `graphrt deploy` knows about a Lambda deployment.
#[derive(Clone, Debug)]
pub struct DeploymentCheck<'a> {
    pub triggers: &'a [RunTrigger],
    pub max_run_secs: u64,
    pub function_timeout_secs: u64,
    /// Value of `AWS_LWA_INVOKE_MODE` in the function environment, if known.
    pub lwa_invoke_mode: Option<&'a str>,
}

/// Reject deployments Lambda cannot run correctly.
///
/// @throws `DeployError` (`Invalid`) naming the first violated rule.
pub fn validate_deployment(check: &DeploymentCheck<'_>) -> Result<(), DeployError> {
    if let Some(trigger) = check.triggers.iter().find(|t| **t != RunTrigger::Sync) {
        return Err(DeployError::invalid(format!(
            "Lambda runs synchronous requests only ({trigger:?} runs need a provider with background CPU, e.g. ECS Fargate)"
        )));
    }
    if check.max_run_secs > MAX_RUN_SECS {
        return Err(DeployError::invalid(format!(
            "max_run_secs {} exceeds the Lambda limit of {MAX_RUN_SECS} s",
            check.max_run_secs
        )));
    }
    if check.function_timeout_secs > LAMBDA_MAX_TIMEOUT_SECS || check.function_timeout_secs < check.max_run_secs + 30 {
        return Err(DeployError::invalid(format!(
            "function timeout must be between max_run_secs + 30 s ({}) and {LAMBDA_MAX_TIMEOUT_SECS} s (got {})",
            check.max_run_secs + 30,
            check.function_timeout_secs
        )));
    }
    if let Some(mode) = check.lwa_invoke_mode
        && !mode.eq_ignore_ascii_case("response_stream")
    {
        return Err(DeployError::invalid(format!(
            "AWS_LWA_INVOKE_MODE must be response_stream for streamed runs (got {mode})"
        )));
    }
    Ok(())
}

/// Why an invocation did not produce a complete reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InvokeError {
    /// The run hit Lambda's timeout or the gateway's `max_run_secs`: `failed{platform_timeout}`.
    PlatformTimeout { after_secs: u64, details: String },
    /// The function (or Lambda Web Adapter) failed.
    Function { code: String, details: String },
    /// The stream ended without `InvokeComplete`.
    Truncated,
    /// Bytes that are not a valid event stream / adapter reply.
    Malformed(String),
    /// The invoke API call itself failed.
    Provider(ProviderError),
}

impl InvokeError {
    /// The `error_type` a failed run records.
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::PlatformTimeout { .. } => "platform_timeout",
            Self::Function { .. } => "platform_error",
            Self::Truncated | Self::Malformed(_) | Self::Provider(_) => "owner_lost",
        }
    }
}

impl fmt::Display for InvokeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PlatformTimeout { after_secs, details } => {
                write!(f, "Lambda platform timeout after {after_secs} s: {details}")
            }
            Self::Function { code, details } => write!(f, "Lambda function error {code}: {details}"),
            Self::Truncated => write!(f, "Lambda response stream ended without InvokeComplete"),
            Self::Malformed(m) => write!(f, "malformed Lambda response: {m}"),
            Self::Provider(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for InvokeError {}

/// Observations about one reply, readable while and after the body streams.
#[derive(Clone, Debug, Default)]
pub struct StreamDiagnostics(Arc<StdMutex<DiagnosticsState>>);

#[derive(Debug, Clone)]
struct DiagnosticsState {
    mode: StreamMode,
    body_chunks: u32,
    body_bytes: u64,
    started: Option<Instant>,
    first_body_after: Option<Duration>,
}

impl Default for DiagnosticsState {
    fn default() -> Self {
        Self { mode: StreamMode::Unknown, body_chunks: 0, body_bytes: 0, started: None, first_body_after: None }
    }
}

impl StreamDiagnostics {
    fn with<R>(&self, f: impl FnOnce(&mut DiagnosticsState) -> R) -> R {
        let mut guard = match self.0.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&mut guard)
    }

    /// Streaming, buffered, or not yet known.
    pub fn mode(&self) -> StreamMode {
        self.with(|s| s.mode)
    }

    /// Body chunks received so far.
    pub fn body_chunks(&self) -> u32 {
        self.with(|s| s.body_chunks)
    }

    /// Time from the invoke to the first body byte.
    pub fn first_body_after(&self) -> Option<Duration> {
        self.with(|s| s.first_body_after)
    }
}

/// The agent's reply: head, then the body as it streams.
pub struct LambdaResponse {
    pub head: ResponseHead,
    pub body: BoxStream<'static, Result<Bytes, InvokeError>>,
    pub diagnostics: StreamDiagnostics,
}

impl fmt::Debug for LambdaResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LambdaResponse").field("head", &self.head).field("diagnostics", &self.diagnostics).finish()
    }
}

fn is_timeout(code: &str, details: &str) -> bool {
    let code = code.to_ascii_lowercase();
    code.contains("timedout") || code.contains("timeout") || details.to_ascii_lowercase().contains("timed out")
}

/// Reads event-stream frames and adapter framing into body chunks.
struct InvocationReader {
    bytes: BoxStream<'static, reqwest::Result<Bytes>>,
    frames: eventstream::Decoder,
    http: lwa::HttpResponseDecoder,
    pending: VecDeque<Bytes>,
    head: Option<ResponseHead>,
    complete: bool,
    started: Instant,
    /// Fires at the gateway-side `max_run_secs` cap (one timer per invocation).
    deadline: Pin<Box<tokio::time::Sleep>>,
    buffering_threshold: Duration,
    diagnostics: StreamDiagnostics,
}

impl InvocationReader {
    fn timeout_error(&self, details: &str) -> InvokeError {
        InvokeError::PlatformTimeout { after_secs: self.started.elapsed().as_secs(), details: details.to_owned() }
    }

    fn record_body(&mut self, body: Vec<u8>) {
        if body.is_empty() {
            return;
        }
        let elapsed = self.started.elapsed();
        self.diagnostics.with(|s| {
            s.body_chunks += 1;
            s.body_bytes += body.len() as u64;
            s.first_body_after.get_or_insert(elapsed);
        });
        self.pending.push_back(Bytes::from(body));
    }

    fn apply(&mut self, decoded: lwa::Decoded) {
        if let Some(head) = decoded.head {
            self.head = Some(head);
        }
        self.record_body(decoded.body);
    }

    fn handle_message(&mut self, message: &eventstream::Message) -> Result<(), InvokeError> {
        if message.header(":message-type") == Some("exception") {
            let code = message.header(":exception-type").unwrap_or("Exception").to_owned();
            let details = String::from_utf8_lossy(&message.payload).into_owned();
            return Err(if is_timeout(&code, &details) {
                self.timeout_error(&details)
            } else {
                InvokeError::Function { code, details }
            });
        }
        match message.header(":event-type") {
            Some("PayloadChunk") => {
                let decoded = self.http.push(&message.payload).map_err(InvokeError::Malformed)?;
                self.apply(decoded);
                Ok(())
            }
            Some("InvokeComplete") => {
                let summary: Value = serde_json::from_slice(&message.payload).unwrap_or(Value::Null);
                let code = summary.get("ErrorCode").and_then(Value::as_str).unwrap_or_default().to_owned();
                let details = summary.get("ErrorDetails").and_then(Value::as_str).unwrap_or_default().to_owned();
                if !code.is_empty() {
                    return Err(if is_timeout(&code, &details) {
                        self.timeout_error(&details)
                    } else {
                        InvokeError::Function { code, details }
                    });
                }
                let decoded = self.http.finish().map_err(InvokeError::Malformed)?;
                self.apply(decoded);
                self.complete = true;
                self.finalize_mode();
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Decide streaming vs buffered once the reply is complete.
    fn finalize_mode(&self) {
        let elapsed = self.started.elapsed();
        let mode = self.http.mode();
        self.diagnostics.with(|s| {
            s.mode = match mode {
                StreamMode::Streaming if s.body_chunks <= 1 && elapsed >= self.buffering_threshold => {
                    StreamMode::Buffered
                }
                other => other,
            };
        });
        if self.diagnostics.mode() == StreamMode::Buffered {
            tracing::warn!(
                elapsed_ms = elapsed.as_millis() as u64,
                "Lambda reply was buffered: set AWS_LWA_INVOKE_MODE=response_stream and flush SSE events"
            );
        }
    }

    /// Read and handle one network chunk.
    async fn read_one(&mut self) -> Result<(), InvokeError> {
        let next = match futures::future::select(self.bytes.next(), self.deadline.as_mut()).await {
            futures::future::Either::Left((item, _)) => Ok(item),
            futures::future::Either::Right(((), _)) => Err(()),
        };
        let chunk = match next {
            Err(_) => return Err(self.timeout_error("gateway max_run_secs reached")),
            Ok(None) => return Err(InvokeError::Truncated),
            Ok(Some(Err(e))) => {
                return Err(InvokeError::Provider(ProviderError::transient(format!("lambda stream: {e}"))));
            }
            Ok(Some(Ok(chunk))) => chunk,
        };
        for message in self.frames.push(&chunk).map_err(|e| InvokeError::Malformed(e.to_string()))? {
            self.handle_message(&message)?;
        }
        if self.head.is_some() {
            let mode = self.http.mode();
            self.diagnostics.with(|s| {
                if s.mode == StreamMode::Unknown {
                    s.mode = mode;
                }
            });
        }
        Ok(())
    }

    async fn next_body(&mut self) -> Option<Result<Bytes, InvokeError>> {
        while self.pending.is_empty() && !self.complete {
            if let Err(e) = self.read_one().await {
                self.complete = true;
                return Some(Err(e));
            }
        }
        self.pending.pop_front().map(Ok)
    }
}

/// The Lambda provider.
pub struct LambdaProvider {
    client: AwsRestClient,
    config: LambdaConfig,
}

impl fmt::Debug for LambdaProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LambdaProvider").field("config", &self.config).finish_non_exhaustive()
    }
}

impl LambdaProvider {
    /// A provider signing with `chain`.
    ///
    /// @throws `ProviderError::Fatal` when `max_run_secs` exceeds 840 s or the client cannot be built.
    pub fn new(chain: Arc<CredentialChain>, config: LambdaConfig) -> Result<Self, ProviderError> {
        if config.max_run_secs > MAX_RUN_SECS {
            return Err(ProviderError::fatal(format!("Lambda max_run_secs is at most {MAX_RUN_SECS}")));
        }
        let client = AwsRestClient::new(chain, config.endpoint.clone(), config.limits)?;
        Ok(Self { client, config })
    }

    fn function(&self, deployment: &DeploymentRef) -> Result<LambdaFunction, ProviderError> {
        if !deployment.target.is_empty() {
            return Ok(LambdaFunction { function_name: deployment.target.clone() });
        }
        self.config
            .functions
            .get(&deployment.name)
            .cloned()
            .ok_or_else(|| ProviderError::fatal(format!("no Lambda function for deployment {}", deployment.name)))
    }

    /// `lambda://<function>[:<qualifier>]`: not an HTTP URL; the gateway invokes
    /// Lambda deployments through `Provider::invoke` ([`LambdaProvider::invoke_agent`]).
    fn invoke_address(function: &LambdaFunction, deployment: &DeploymentRef) -> EndpointAddress {
        if deployment.version.is_empty() {
            EndpointAddress::Http(format!("lambda://{}", function.function_name))
        } else {
            EndpointAddress::Http(format!("lambda://{}:{}", function.function_name, deployment.version))
        }
    }

    fn url_replica(deployment: &DeploymentRef) -> ReplicaRef {
        ReplicaRef { deployment: deployment.clone(), id: "url".to_owned() }
    }

    /// Invoke the agent behind `deployment` and stream its reply.
    ///
    /// Returns once the reply head arrived; the body streams afterwards and ends
    /// with an `InvokeError` when the platform cuts the run.
    ///
    /// @throws `InvokeError::Provider` when the invoke is rejected (throttled, not found, auth).
    pub async fn invoke_agent(
        &self,
        deployment: &DeploymentRef,
        request: &AgentRequest,
    ) -> Result<LambdaResponse, InvokeError> {
        let function = self.function(deployment).map_err(InvokeError::Provider)?;
        let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let payload = serde_json::to_vec(&lwa::function_url_event(request, now_ms))
            .map_err(|e| InvokeError::Malformed(format!("cannot encode event: {e}")))?;
        let path = format!(
            "/2021-11-15/functions/{}/response-streaming-invocations",
            uri_encode(&function.function_name, false)
        );
        let query: Vec<(&str, &str)> =
            if deployment.version.is_empty() { Vec::new() } else { vec![("Qualifier", deployment.version.as_str())] };
        let headers = [
            ("content-type", "application/json"),
            ("x-amz-invocation-type", "RequestResponse"),
            ("x-amz-log-type", "None"),
        ];
        let started = Instant::now();
        let deadline = started + Duration::from_secs(self.config.max_run_secs);
        let aws_request = AwsRequest {
            method: "POST",
            path: &path,
            query: &query,
            headers: &headers,
            body: &payload,
            idempotent: false,
        };
        let response = self.client.send(&aws_request).await.map_err(InvokeError::Provider)?;
        let diagnostics = StreamDiagnostics::default();
        diagnostics.with(|s| s.started = Some(started));
        let mut reader = InvocationReader {
            bytes: response.bytes_stream().boxed(),
            frames: eventstream::Decoder::default(),
            http: lwa::HttpResponseDecoder::default(),
            pending: VecDeque::new(),
            head: None,
            complete: false,
            started,
            deadline: Box::pin(tokio::time::sleep_until(deadline.into())),
            buffering_threshold: self.config.buffering_threshold,
            diagnostics: diagnostics.clone(),
        };
        // A buffered reply has no prelude: its head only arrives with InvokeComplete.
        while reader.head.is_none() && !reader.complete {
            reader.read_one().await?;
        }
        let head = reader.head.clone().ok_or(InvokeError::Truncated)?;
        let body =
            futures::stream::unfold(
                reader,
                |mut reader| async move { reader.next_body().await.map(|item| (item, reader)) },
            )
            .boxed();
        Ok(LambdaResponse { head, body, diagnostics })
    }
}

impl Provider for LambdaProvider {
    /// `max_request_secs` is the gateway-side cap (840 s); `max_grace_secs = 0`
    /// tells the gateway there is no preemption window: a run past the cap ends
    /// with `failed{platform_timeout}` ([`InvokeError::PlatformTimeout`]).
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            addressing: Addressing::Url,
            scaler: Scaler::Platform,
            max_request_secs: Some(self.config.max_run_secs),
            idle_timeout_secs: None,
            max_grace_secs: 0,
            background_cpu: false,
            supports_suspend: false,
            wake_p95_target_ms: 1_000,
        }
    }

    /// No-op: Lambda wakes on invoke. Returns the single invoke target.
    fn wake<'a>(
        &'a self,
        deployment: &'a DeploymentRef,
        replicas: u32,
    ) -> BoxFuture<'a, Result<Vec<Endpoint>, ProviderError>> {
        Box::pin(async move {
            let function = self.function(deployment)?;
            if replicas == 0 {
                return Ok(Vec::new());
            }
            let address = Self::invoke_address(&function, deployment);
            Ok(vec![Endpoint { replica: Self::url_replica(deployment), address }])
        })
    }

    /// No-op: the platform scales in.
    fn release<'a>(&'a self, _replica: &'a ReplicaRef, _mode: ReleaseMode) -> BoxFuture<'a, Result<(), ProviderError>> {
        Box::pin(async { Ok(()) })
    }

    /// Observability only: one pseudo-replica for the function version.
    fn list<'a>(&'a self, deployment: &'a DeploymentRef) -> BoxFuture<'a, Result<Vec<ReplicaState>, ProviderError>> {
        Box::pin(async move {
            let function = self.function(deployment)?;
            Ok(vec![ReplicaState {
                replica: Self::url_replica(deployment),
                phase: ReplicaPhase::Ready,
                address: Some(Self::invoke_address(&function, deployment)),
                started_at_ms: None,
            }])
        })
    }

    /// Carries the gateway's (already signed) protocol request to the agent through
    /// `InvokeWithResponseStream`. `None` for anything that is not a `lambda://` endpoint.
    fn invoke<'a>(
        &'a self,
        endpoint: &'a Endpoint,
        request: InvokeRequest,
    ) -> Option<BoxFuture<'a, Result<InvokeResponse, ProviderError>>> {
        match &endpoint.address {
            EndpointAddress::Http(address) if address.starts_with("lambda://") => {}
            _ => return None,
        }
        Some(Box::pin(async move {
            let agent_request = agent_request(request);
            let response = self
                .invoke_agent(&endpoint.replica.deployment, &agent_request)
                .await
                .map_err(InvokeError::into_provider_error)?;
            let body =
                response.body.map(|item| item.map(|chunk| chunk.to_vec()).map_err(InvokeError::into_provider_error));
            Ok(InvokeResponse { status: response.head.status, body: Box::pin(body) })
        }))
    }
}

/// The run id travels in the `graphrt-run-id` header; it becomes the Lambda request id.
fn agent_request(request: InvokeRequest) -> AgentRequest {
    let request_id = request
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("graphrt-run-id"))
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    AgentRequest {
        method: request.method,
        path_and_query: request.path,
        headers: request.headers,
        body: request.body,
        request_id,
    }
}

impl InvokeError {
    /// The `ProviderError` the gateway sees through `Provider::invoke`.
    ///
    /// A platform timeout is `Fatal` with a message starting with
    /// [`PLATFORM_TIMEOUT_PREFIX`], so the gateway records
    /// `failed{error_type: platform_timeout}` instead of re-dispatching.
    pub fn into_provider_error(self) -> ProviderError {
        match self {
            Self::Provider(error) => error,
            Self::PlatformTimeout { .. } => ProviderError::fatal(format!("{PLATFORM_TIMEOUT_PREFIX}: {self}")),
            Self::Function { .. } => ProviderError::fatal(format!("platform_error: {self}")),
            Self::Truncated | Self::Malformed(_) => ProviderError::transient(self.to_string()),
        }
    }
}

/// Message prefix of the `ProviderError` for a run cut by the Lambda / gateway time cap.
pub const PLATFORM_TIMEOUT_PREFIX: &str = "platform_timeout";

#[cfg(test)]
mod tests {
    use super::*;

    fn check(
        triggers: &[RunTrigger],
        max_run: u64,
        timeout: u64,
        mode: Option<&'static str>,
    ) -> Result<(), DeployError> {
        validate_deployment(&DeploymentCheck {
            triggers,
            max_run_secs: max_run,
            function_timeout_secs: timeout,
            lwa_invoke_mode: mode,
        })
    }

    #[test]
    fn deployment_rules() {
        assert!(check(&[RunTrigger::Sync], 840, 900, Some("response_stream")).is_ok());
        assert!(check(&[RunTrigger::Sync], 60, 120, None).is_ok());
        assert!(check(&[RunTrigger::Sync, RunTrigger::Cron], 60, 120, None).is_err());
        assert!(check(&[RunTrigger::Async], 60, 120, None).is_err());
        assert!(check(&[RunTrigger::Webhook], 60, 120, None).is_err());
        assert!(check(&[RunTrigger::Sync], 841, 900, None).is_err());
        assert!(check(&[RunTrigger::Sync], 840, 860, None).is_err());
        assert!(check(&[RunTrigger::Sync], 60, 901, None).is_err());
        assert!(check(&[RunTrigger::Sync], 60, 120, Some("buffered")).is_err());
    }

    #[test]
    fn timeout_detection() {
        assert!(is_timeout("Sandbox.Timedout", ""));
        assert!(is_timeout("Unhandled", "Task timed out after 900.00 seconds"));
        assert!(!is_timeout("Runtime.ExitError", "exit status 1"));
        assert_eq!(
            InvokeError::PlatformTimeout { after_secs: 1, details: String::new() }.error_type(),
            "platform_timeout"
        );
    }
}
