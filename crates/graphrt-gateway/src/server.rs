//! Public HTTP API of the gateway (HTTP/1.1, axum) and the admin API used by
//! `graphrt deploy`. Agents never call the gateway: it pulls their orphans.
//!
//! | Method | Path | |
//! | --- | --- | --- |
//! | POST | `/v1/deployments/{d}/runs` | start a run: JSON (sync), `"stream": true` (SSE) or `"async": true` (202) |
//! | GET | `/v1/deployments/{d}/runs/{id}` | run record |
//! | GET | `/v1/deployments/{d}/runs/{id}/events` | SSE replay + live tail (`Last-Event-ID` or `?from=`) |
//! | POST | `/v1/deployments/{d}/runs/{id}/resume` | resume a paused run (`inputs`, `secrets`) |
//! | POST | `/v1/deployments/{d}/runs/{id}/abort` | abort |
//! | POST | `/v1/deployments/{d}/hooks/{hook}` | webhook ingress |
//! | POST | `/v1/deployments/{d}/warm` | prewarm replicas ahead of traffic (`{"replicas": n}`) |
//! | POST | `/admin/v1/deployments/{d}/versions` | register a version (admin token) |
//! | GET | `/metrics`, `/healthz` | |
//!
//! Client headers are never forwarded to agents except the configured input
//! headers (which may not be `graphrt-*`), so a client cannot forge
//! `Graphrt-*` protocol headers.

// Handlers return early with a ready `Response` as the error: the idiomatic axum shape.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::StreamExt as _;
use graphrt_core::coordination::{CoordinationStore, deployment_key};
use graphrt_core::store::{RunRecord, RunStatus, RunStore, now_ms, stored_event_type};
use serde::Deserialize;
use serde_json::{Value as Json, json};

use crate::auth::{AuthError, Principal};
use crate::deployment::{Deployment, VersionError, VersionInfo};
use crate::limiter::{Decision, Gcra};
use crate::metrics::{DeploymentGauges, bump};
use crate::pool::ActivateError;
use crate::pool::{PrewarmOutcome, PrewarmRequest};
use crate::signature::unix_now;
use crate::sse::{Frame, is_after, resume_point};
use crate::supervisor::{Gateway, LiveRun, RunInput, Settled, StartError, StartOptions, Started};
use crate::warm::PrewarmSource;
use crate::webhook::{HookConfig, delivery_key, hook_run_id, verify};

/// Largest request body.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
const HEARTBEAT: Duration = Duration::from_secs(15);
const CLIENT_BUFFER_FRAMES: usize = 64;

/// A hook and its signing secret.
pub type HookEntry = (HookConfig, Option<Vec<u8>>);

/// Called when a version is registered (the local provider learns its command).
pub type VersionHook = Arc<dyn Fn(&Deployment, &VersionInfo) + Send + Sync>;

/// Shared state of the HTTP server.
pub struct App {
    /// The core.
    pub gateway: Arc<Gateway>,
    /// Per-client-IP limiter.
    pub ip_limiter: Option<Gcra<IpAddr>>,
    /// SHA-256 hex of the admin token.
    pub admin_token_hash: Option<String>,
    /// Hooks by (deployment, hook id) with their secrets.
    pub hooks: HashMap<(String, String), HookEntry>,
    /// Version registration hook.
    pub on_version: Option<VersionHook>,
    /// Gateway-wide wake gate (for `/metrics`).
    pub wake_gate: Option<Arc<crate::warm::WakeGate>>,
    started: tokio::time::Instant,
}

impl std::fmt::Debug for App {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("App").finish_non_exhaustive()
    }
}

impl App {
    /// Wraps a gateway.
    pub fn new(gateway: Arc<Gateway>) -> Self {
        Self {
            gateway,
            ip_limiter: None,
            admin_token_hash: None,
            hooks: HashMap::new(),
            on_version: None,
            wake_gate: None,
            started: tokio::time::Instant::now(),
        }
    }
}

/// The router.
pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .route("/v1/deployments/{deployment}/runs", post(create_run))
        .route("/v1/deployments/{deployment}/runs/{run_id}", get(get_run))
        .route("/v1/deployments/{deployment}/runs/{run_id}/events", get(run_events))
        .route("/v1/deployments/{deployment}/runs/{run_id}/resume", post(resume_run))
        .route("/v1/deployments/{deployment}/runs/{run_id}/abort", post(abort_run))
        .route("/v1/deployments/{deployment}/hooks/{hook}", post(webhook))
        .route("/v1/deployments/{deployment}/warm", post(warm))
        .route("/admin/v1/deployments/{deployment}/versions", post(add_version))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(app)
}

fn error(status: StatusCode, error_type: &str, message: &str) -> Response {
    (status, axum::Json(json!({ "error": message, "error_type": error_type }))).into_response()
}

fn retry_after(status: StatusCode, error_type: &str, message: &str, seconds: u64) -> Response {
    let mut response = error(status, error_type, message);
    if let Ok(value) = HeaderValue::from_str(&seconds.max(1).to_string()) {
        response.headers_mut().insert("retry-after", value);
    }
    response
}

fn start_error(app: &App, failure: StartError) -> Response {
    match failure {
        StartError::Version(VersionError::Retired(_)) => {
            error(StatusCode::GONE, "version_retired", &failure.to_string())
        }
        StartError::Version(VersionError::NothingDeployed) => {
            retry_after(StatusCode::SERVICE_UNAVAILABLE, "not_deployed", &failure.to_string(), 5)
        }
        StartError::Version(VersionError::Unknown(_)) | StartError::NotFound => {
            error(StatusCode::NOT_FOUND, "not_found", &failure.to_string())
        }
        StartError::Activate(activate) => {
            bump(&app.gateway.metrics.activation_failures);
            let status = if matches!(activate, ActivateError::Closed) {
                StatusCode::GONE
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            };
            retry_after(status, "no_capacity", &activate.to_string(), activate.retry_after_secs())
        }
        StartError::Conflict(message) => error(StatusCode::CONFLICT, "conflict", &message),
        StartError::Store(message) => retry_after(StatusCode::SERVICE_UNAVAILABLE, "store_unavailable", &message, 2),
    }
}

async fn healthz(State(app): State<Arc<App>>) -> Response {
    axum::Json(json!({ "status": "ok", "uptime_secs": app.started.elapsed().as_secs(), "live_runs": app.gateway.live_count() })).into_response()
}

async fn metrics(State(app): State<Arc<App>>) -> Response {
    let mut gauges = Vec::new();
    let mut budgets = Vec::new();
    let now = unix_now();
    for deployment in app.gateway.deployments() {
        if let Some(budget) = &deployment.pool_config.warm_budget {
            budgets.push((deployment.name.clone(), budget.used_secs(now), budget.limit_secs()));
        }
        for version in deployment.versions() {
            let stats = version.pool.stats();
            gauges.push(DeploymentGauges {
                deployment: deployment.name.clone(),
                version: version.info.version.clone(),
                replicas: stats.replicas,
                active_runs: stats.active_runs,
                queued: stats.queued,
                prewarm: stats.prewarm,
            });
        }
    }
    let mut text = app.gateway.metrics.render(&gauges, app.gateway.live_count());
    crate::metrics::render_warm(&mut text, &budgets, app.wake_gate.as_deref());
    ([("content-type", "text/plain; version=0.0.4")], text).into_response()
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    header(headers, "authorization")
        .and_then(|value| value.strip_prefix("Bearer ").or_else(|| value.strip_prefix("bearer ")))
}

/// Rate limit per IP, then authentication, then the per-principal limit.
async fn admit(app: &App, peer: IpAddr, deployment: &Deployment, headers: &HeaderMap) -> Result<Principal, Response> {
    let clock = app.started.elapsed().as_millis() as u64;
    if let Some(limiter) = &app.ip_limiter
        && let Decision::Deny(wait) = limiter.check(&peer, clock)
    {
        bump(&app.gateway.metrics.rate_limited);
        return Err(retry_after(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "too many requests from this address",
            wait.as_secs() + 1,
        ));
    }
    let principal = if deployment.open && !deployment.auth.is_configured() {
        Principal::ApiKey
    } else {
        match deployment.auth.authenticate(bearer(headers), header(headers, "x-api-key"), unix_now()).await {
            Ok(principal) => principal,
            Err(AuthError::KeysUnavailable) => {
                return Err(retry_after(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "auth_unavailable",
                    "token keys unavailable",
                    5,
                ));
            }
            Err(failure) => {
                bump(&app.gateway.metrics.auth_failures);
                return Err(error(StatusCode::UNAUTHORIZED, "unauthorized", &failure.to_string()));
            }
        }
    };
    let key = match &principal {
        Principal::ApiKey => format!("ip:{peer}"),
        Principal::User { subject, .. } => format!("sub:{subject}"),
    };
    if let Some(limiter) = &deployment.principal_limiter
        && let Decision::Deny(wait) = limiter.check(&key, clock)
    {
        bump(&app.gateway.metrics.rate_limited);
        return Err(retry_after(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "rate limit exceeded",
            wait.as_secs() + 1,
        ));
    }
    Ok(principal)
}

fn find_deployment(app: &App, name: &str) -> Result<Arc<Deployment>, Response> {
    app.gateway.deployment(name).ok_or_else(|| error(StatusCode::NOT_FOUND, "not_found", "unknown deployment"))
}

fn record_json(record: &RunRecord) -> Json {
    json!({
        "run_id": record.run_id, "deployment": record.deployment, "version": record.deployment_version,
        "workflow": record.workflow, "status": record.status, "outputs": record.outputs, "answer": record.answer,
        "error": record.error, "error_type": record.error_type, "pause_reasons": record.pause_reasons,
        "usage": record.usage, "event_count": record.event_count, "epoch": record.epoch,
        "created_ms": record.created_ms, "updated_ms": record.updated_ms,
    })
}

/// Builds the in-memory run input: body, input headers, verified `user_id`.
fn run_input(
    deployment: &Deployment,
    headers: &HeaderMap,
    mut body: serde_json::Map<String, Json>,
    principal: &Principal,
) -> RunInput {
    let mut expires_at = None;
    if let Principal::User { subject, expires_at: exp } = principal {
        body.insert("user_id".into(), Json::from(subject.clone()));
        expires_at = Some(*exp);
    }
    if !deployment.input_headers.is_empty() {
        let inputs = body.entry("inputs").or_insert_with(|| Json::Object(Default::default()));
        if let Some(inputs) = inputs.as_object_mut() {
            for mapping in &deployment.input_headers {
                if let Some(value) = header(headers, &mapping.header) {
                    let value =
                        if mapping.strip_bearer { value.strip_prefix("Bearer ").unwrap_or(value) } else { value };
                    inputs.insert(mapping.input.clone(), Json::from(value));
                }
            }
        }
    }
    RunInput { body, expires_at }
}

fn parse_object(bytes: &[u8]) -> Result<serde_json::Map<String, Json>, Response> {
    if bytes.is_empty() {
        return Ok(serde_json::Map::new());
    }
    match serde_json::from_slice::<Json>(bytes) {
        Ok(Json::Object(object)) => Ok(object),
        _ => Err(error(StatusCode::BAD_REQUEST, "bad_request", "body must be a JSON object")),
    }
}

async fn create_run(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let deployment = match find_deployment(&app, &name) {
        Ok(deployment) => deployment,
        Err(response) => return response,
    };
    let principal = match admit(&app, peer.ip(), &deployment, &headers).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    deployment.record_arrival(unix_now());
    let object = match parse_object(&body) {
        Ok(object) => object,
        Err(response) => return response,
    };
    let stream = object.get("stream").and_then(Json::as_bool).unwrap_or(false);
    let run_async = object.get("async").and_then(Json::as_bool).unwrap_or(false);
    let run_id = header(&headers, "idempotency-key").filter(|key| !key.is_empty() && key.len() <= 256).map(|key| {
        let digest = crate::hex::sha256_hex(format!("idem|{name}|{key}").as_bytes());
        format!("idem-{}", digest.get(..32).unwrap_or_default())
    });
    let input = run_input(&deployment, &headers, object, &principal);
    let options = StartOptions { run_id, interactive: !run_async, callback: None, metadata: Vec::new() };
    let run = match app.gateway.start_run(&deployment, input, options).await {
        Ok(Started::New(run)) => run,
        Ok(Started::Duplicate(record)) => {
            let mut body = record_json(&record);
            if let Some(object) = body.as_object_mut() {
                object.insert("duplicate".into(), Json::Bool(true));
            }
            return (StatusCode::OK, axum::Json(body)).into_response();
        }
        Err(failure) => return start_error(&app, failure),
    };
    if run_async {
        return (StatusCode::ACCEPTED, axum::Json(json!({ "run_id": run.run_id, "status": "queued" }))).into_response();
    }
    if stream {
        return client_stream(app.gateway.clone(), run.run_id.clone(), Some(run), 0, None);
    }
    let mut settled = run.settled();
    let record = loop {
        if let Settled::Done(record) = &*settled.borrow_and_update() {
            break record.clone();
        }
        if settled.changed().await.is_err() {
            return error(StatusCode::INTERNAL_SERVER_ERROR, "internal", "run supervisor stopped");
        }
    };
    (StatusCode::OK, axum::Json(record_json(&record))).into_response()
}

async fn load_run(app: &App, deployment: &Deployment, run_id: &str) -> Result<RunRecord, Response> {
    match app.gateway.store.get(run_id).await {
        Ok(Some(record)) if record.deployment.as_deref() == Some(deployment.name.as_str()) => Ok(record),
        Ok(_) | Err(graphrt_core::store::StoreError::InvalidId(_)) => {
            Err(error(StatusCode::NOT_FOUND, "not_found", "run not found"))
        }
        Err(failure) => Err(retry_after(StatusCode::SERVICE_UNAVAILABLE, "store_unavailable", &failure.to_string(), 2)),
    }
}

async fn get_run(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path((name, run_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let deployment = match find_deployment(&app, &name) {
        Ok(deployment) => deployment,
        Err(response) => return response,
    };
    if let Err(response) = admit(&app, peer.ip(), &deployment, &headers).await {
        return response;
    }
    match load_run(&app, &deployment, &run_id).await {
        Ok(record) => axum::Json(record_json(&record)).into_response(),
        Err(response) => response,
    }
}

/// `from=N` of the events query (parsed by hand: one field does not need a
/// query deserializer compiled for it). `400` when it is not a number.
fn events_from(raw: Option<&str>) -> Result<Option<u64>, Response> {
    let Some(value) = raw.unwrap_or_default().split('&').find_map(|pair| pair.strip_prefix("from=")) else {
        return Ok(None);
    };
    value.parse().map(Some).map_err(|failure: std::num::ParseIntError| {
        error(StatusCode::BAD_REQUEST, "bad_request", &format!("from: {failure}"))
    })
}

async fn run_events(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path((name, run_id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let from = match events_from(query.as_deref()) {
        Ok(from) => from,
        Err(response) => return response,
    };
    let deployment = match find_deployment(&app, &name) {
        Ok(deployment) => deployment,
        Err(response) => return response,
    };
    if let Err(response) = admit(&app, peer.ip(), &deployment, &headers).await {
        return response;
    }
    if let Err(response) = load_run(&app, &deployment, &run_id).await {
        return response;
    }
    let (start, chunks) = header(&headers, "last-event-id").and_then(resume_point).unwrap_or((from.unwrap_or(0), None));
    let live = app.gateway.live_run(&run_id);
    client_stream(app.gateway.clone(), run_id, live, start, chunks)
}

/// A client SSE stream: stored events from `start` (when the in-memory ring
/// does not reach back far enough), the ring, then the live tail, without gaps
/// or duplicates (I5). The per-client buffer is bounded; a client too slow for
/// the broadcast is disconnected and reconnects with `Last-Event-ID` (D10).
fn client_stream(
    gateway: Arc<Gateway>,
    run_id: String,
    live: Option<Arc<LiveRun>>,
    start: u64,
    chunks: Option<u64>,
) -> Response {
    let (mut sender, receiver) = futures::channel::mpsc::channel::<Bytes>(CLIENT_BUFFER_FRAMES);
    graphrt_core::task::spawn(async move {
        use futures::SinkExt as _;
        bump(&gateway.metrics.client_streams);
        let mut cursor = (start, chunks);
        let subscription = live.as_ref().map(|run| (run.subscribe(), run.settled()));
        // A run this request just created streams from its ring alone: one
        // store read (and its wait for the connection) fewer before the first byte.
        let needs_store = live.as_ref().is_none_or(|run| run.needs_store_from(cursor.0));
        let send = |frame: &Frame, cursor: &mut (u64, Option<u64>)| -> Option<Bytes> {
            let id = frame.id.as_deref()?;
            if !is_after(id, cursor.0, cursor.1) {
                return None;
            }
            *cursor = match id.split_once('.') {
                Some((index, chunk)) => (index.parse().unwrap_or(cursor.0), chunk.parse().ok()),
                None => (id.parse::<u64>().map_or(cursor.0, |index| index + 1), None),
            };
            Some(Bytes::from(frame.encode()))
        };
        if needs_store && let Ok(events) = gateway.store.events(&run_id, cursor.0).await {
            let base = cursor.0;
            for (offset, event) in events.iter().enumerate() {
                let index = base + offset as u64;
                let frame = Frame {
                    event: stored_event_type(event).unwrap_or("event").to_owned(),
                    id: Some(index.to_string()),
                    data: event.get().to_owned(),
                };
                if let Some(bytes) = send(&frame, &mut cursor.clone()) {
                    cursor = (index + 1, None);
                    if sender.send(bytes).await.is_err() {
                        return finish_stream(&gateway);
                    }
                }
            }
        }
        let Some(((buffered, mut frames), mut settled)) = subscription else { return finish_stream(&gateway) };
        for frame in &buffered {
            if let Some(bytes) = send(frame, &mut cursor)
                && sender.send(bytes).await.is_err()
            {
                return finish_stream(&gateway);
            }
        }
        // First beat one period from now. `interval` + an initial `tick().await`
        // went through the timer driver (1 ms resolution) and held back the
        // first frame of every stream by up to a millisecond.
        let mut heartbeat = tokio::time::interval_at(tokio::time::Instant::now() + HEARTBEAT, HEARTBEAT);
        loop {
            if !matches!(*settled.borrow(), Settled::Running) {
                while let Ok(frame) = frames.try_recv() {
                    if let Some(bytes) = send(&frame, &mut cursor) {
                        let _client_gone = sender.send(bytes).await;
                    }
                }
                return finish_stream(&gateway);
            }
            tokio::select! {
                frame = frames.recv() => match frame {
                    Ok(frame) => if let Some(bytes) = send(&frame, &mut cursor) && sender.send(bytes).await.is_err() { return finish_stream(&gateway) },
                    // Lagged or closed: end the stream; the client resumes with Last-Event-ID.
                    Err(_) => return finish_stream(&gateway),
                },
                changed = settled.changed() => if changed.is_err() { return finish_stream(&gateway) },
                _ = heartbeat.tick() => if sender.send(Bytes::from_static(b": heartbeat\n\n")).await.is_err() { return finish_stream(&gateway) },
            }
        }
    });
    let body = Body::from_stream(receiver.map(Ok::<_, Infallible>));
    ([("content-type", "text/event-stream"), ("cache-control", "no-cache"), ("x-accel-buffering", "no")], body)
        .into_response()
}

fn finish_stream(gateway: &Gateway) {
    gateway.metrics.client_streams.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
}

#[derive(Debug, Default, Deserialize)]
struct ResumeBody {
    #[serde(default)]
    inputs: Json,
    #[serde(default)]
    secrets: serde_json::Map<String, Json>,
    #[serde(default)]
    stream: bool,
}

async fn resume_run(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path((name, run_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let deployment = match find_deployment(&app, &name) {
        Ok(deployment) => deployment,
        Err(response) => return response,
    };
    let principal = match admit(&app, peer.ip(), &deployment, &headers).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let request: ResumeBody = if body.is_empty() {
        ResumeBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(request) => request,
            Err(failure) => return error(StatusCode::BAD_REQUEST, "bad_request", &failure.to_string()),
        }
    };
    deployment.record_arrival(unix_now());
    let fresh = match &principal {
        Principal::User { .. } => Some(run_input(&deployment, &headers, serde_json::Map::new(), &principal)),
        Principal::ApiKey => None,
    };
    let inputs = if request.inputs.is_null() { json!({}) } else { request.inputs };
    match app.gateway.resume_run(&deployment, &run_id, inputs, request.secrets, fresh).await {
        Ok(run) if request.stream => {
            let start = app.gateway.store.get(&run_id).await.ok().flatten().map_or(0, |record| record.event_count);
            client_stream(app.gateway.clone(), run_id, Some(run), start, None)
        }
        Ok(run) => {
            (StatusCode::ACCEPTED, axum::Json(json!({ "run_id": run.run_id, "status": "running" }))).into_response()
        }
        Err(failure) => start_error(&app, failure),
    }
}

async fn abort_run(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path((name, run_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let deployment = match find_deployment(&app, &name) {
        Ok(deployment) => deployment,
        Err(response) => return response,
    };
    if let Err(response) = admit(&app, peer.ip(), &deployment, &headers).await {
        return response;
    }
    match app.gateway.abort_run(&deployment, &run_id).await {
        Ok(record) => {
            (StatusCode::ACCEPTED, axum::Json(json!({ "run_id": record.run_id, "status": "aborting" }))).into_response()
        }
        Err(failure) => start_error(&app, failure),
    }
}

async fn webhook(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path((name, hook_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let deployment = match find_deployment(&app, &name) {
        Ok(deployment) => deployment,
        Err(response) => return response,
    };
    let Some((hook, secret)) = app.hooks.get(&(name.clone(), hook_id.clone())) else {
        return error(StatusCode::NOT_FOUND, "not_found", "unknown hook");
    };
    if let Some(limiter) = &app.ip_limiter
        && let Decision::Deny(wait) = limiter.check(&peer.ip(), app.started.elapsed().as_millis() as u64)
    {
        bump(&app.gateway.metrics.rate_limited);
        return retry_after(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "too many requests", wait.as_secs() + 1);
    }
    if let Err(failure) = verify(hook, secret.as_deref(), &headers, &body, unix_now()) {
        bump(&app.gateway.metrics.auth_failures);
        return error(StatusCode::UNAUTHORIZED, "unauthorized", &failure.to_string());
    }
    deployment.record_arrival(unix_now());
    let payload: Json =
        serde_json::from_slice(&body).unwrap_or_else(|_| Json::from(String::from_utf8_lossy(&body).into_owned()));
    let key = delivery_key(hook, &headers, &body);
    let run_id = hook_run_id(&name, &hook.id, &key);
    let mut inputs = serde_json::Map::new();
    inputs.insert(hook.input.clone().unwrap_or_else(|| "payload".into()), payload);
    let mut request = serde_json::Map::new();
    request.insert("inputs".into(), Json::Object(inputs));
    let options = StartOptions {
        run_id: Some(run_id),
        interactive: false,
        callback: hook.callback_url.clone(),
        metadata: vec![("webhook".into(), hook.id.clone())],
    };
    match app.gateway.start_run(&deployment, RunInput { body: request, expires_at: None }, options).await {
        Ok(Started::New(run)) => {
            bump(&app.gateway.metrics.webhooks_accepted);
            (StatusCode::ACCEPTED, axum::Json(json!({ "run_id": run.run_id, "duplicate": false }))).into_response()
        }
        Ok(Started::Duplicate(record)) => {
            bump(&app.gateway.metrics.webhooks_duplicate);
            (StatusCode::OK, axum::Json(json!({ "run_id": record.run_id, "duplicate": true, "status": record.status })))
                .into_response()
        }
        Err(failure) => start_error(&app, failure),
    }
}

/// `POST /v1/deployments/{d}/warm`: wakes replicas of the current version
/// ahead of traffic (a chat UI calls it when a user opens the page). It is
/// authenticated and rate-limited like a run, counts against
/// `max_wakes_per_hour` and the keep-warm budget, is not an arrival for the
/// predictive model, and answers without waiting for the wake: `200 warm`,
/// `202 warming`, `429 warm_budget_exhausted` / `wake_quota_exhausted`.
async fn warm(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let deployment = match find_deployment(&app, &name) {
        Ok(deployment) => deployment,
        Err(response) => return response,
    };
    if let Err(response) = admit(&app, peer.ip(), &deployment, &headers).await {
        return response;
    }
    let object = match parse_object(&body) {
        Ok(object) => object,
        Err(response) => return response,
    };
    // `replicas`: wanted warm (default 1, capped at `max_replicas`).
    let wanted = match object.get("replicas") {
        None => 1,
        Some(value) => match value.as_u64() {
            Some(count) => u32::try_from(count).unwrap_or(u32::MAX),
            None => return error(StatusCode::BAD_REQUEST, "bad_request", "replicas must be a positive integer"),
        },
    };
    let current = match deployment.current() {
        Ok(current) => current,
        Err(failure) => return start_error(&app, StartError::Version(failure)),
    };
    let replicas = wanted.clamp(1, deployment.pool_config.quotas.max_replicas.max(1));
    let pool = current.pool;
    match pool.prewarm_check(replicas) {
        PrewarmOutcome::AlreadyWarm => {
            (StatusCode::OK, axum::Json(json!({ "status": "warm", "replicas": pool.stats().replicas }))).into_response()
        }
        PrewarmOutcome::BudgetExhausted => retry_after(
            StatusCode::TOO_MANY_REQUESTS,
            "warm_budget_exhausted",
            "the keep-warm budget of today is spent",
            crate::warm::WarmBudget::refill_in_secs(unix_now()),
        ),
        PrewarmOutcome::Closed => start_error(&app, StartError::Activate(ActivateError::Closed)),
        PrewarmOutcome::QuotaExhausted => retry_after(
            StatusCode::TOO_MANY_REQUESTS,
            "wake_quota_exhausted",
            "wake quota exhausted",
            ActivateError::WakeQuota.retry_after_secs(),
        ),
        PrewarmOutcome::InFlight | PrewarmOutcome::Started(_) => {
            pool.spawn_prewarm(PrewarmRequest { replicas, ..PrewarmRequest::one(PrewarmSource::Api, Duration::MAX) });
            (StatusCode::ACCEPTED, axum::Json(json!({ "status": "warming", "replicas": replicas }))).into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
struct NewVersion {
    version: String,
    workflow: String,
    #[serde(default)]
    fingerprint: String,
    #[serde(default)]
    target: String,
    #[serde(default)]
    command: Vec<String>,
    #[serde(default)]
    secret_inputs: Option<Vec<String>>,
}

async fn add_version(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let authorized = match (&app.admin_token_hash, bearer(&headers)) {
        (Some(hash), Some(token)) => {
            crate::hex::constant_time_eq(hash.as_bytes(), crate::hex::sha256_hex(token.as_bytes()).as_bytes())
        }
        _ => false,
    };
    if !authorized {
        bump(&app.gateway.metrics.auth_failures);
        return error(StatusCode::UNAUTHORIZED, "unauthorized", "admin token required");
    }
    let Some(deployment) = app.gateway.deployment(&name) else {
        return error(StatusCode::NOT_FOUND, "not_found", "unknown deployment (add it to the gateway config first)");
    };
    let request: NewVersion = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(failure) => return error(StatusCode::BAD_REQUEST, "bad_request", &failure.to_string()),
    };
    if graphrt_core::coordination::validate_key(&deployment_key(&name, "versions", &request.version)).is_err() {
        return error(StatusCode::BAD_REQUEST, "bad_request", "invalid version id");
    }
    let info = VersionInfo {
        version: request.version,
        workflow: request.workflow,
        fingerprint: request.fingerprint,
        target: request.target,
        command: request.command,
        secret_inputs: request.secret_inputs,
        activated_ms: now_ms(),
        superseded_ms: None,
    };
    let existing_versions = deployment.versions();
    if existing_versions.iter().any(|entry| entry.info.version == info.version) {
        return (StatusCode::OK, axum::Json(json!({ "deployment": name, "version": info.version, "created": false })))
            .into_response();
    }
    if let Some(hook) = &app.on_version {
        hook(&deployment, &info);
    }
    let key = deployment_key(&name, "versions", &info.version);
    let stored = serde_json::to_vec(&info).unwrap_or_default();
    if let Err(failure) = app.gateway.store.create_if_absent(&key, stored).await
        && !matches!(failure, graphrt_core::store::StoreError::Conflict(_))
    {
        return retry_after(StatusCode::SERVICE_UNAVAILABLE, "store_unavailable", &failure.to_string(), 2);
    }
    // Persist the superseded mark of the previous current version.
    if let Ok(previous) = deployment.current() {
        let mut superseded = previous.info.clone();
        superseded.superseded_ms = Some(info.activated_ms);
        let previous_key = deployment_key(&name, "versions", &superseded.version);
        if let Ok(Some(object)) = app.gateway.store.get_object(&previous_key).await {
            let _logged = app
                .gateway
                .store
                .compare_and_swap(&previous_key, object.version, serde_json::to_vec(&superseded).unwrap_or_default())
                .await
                .map_err(|failure| eprintln!("gateway: {failure}"));
        }
    }
    let entry = deployment.add_version(info, now_ms());
    if deployment.pool_config.min_replicas > 0 {
        let replicas = deployment.pool_config.min_replicas;
        let request = PrewarmRequest { replicas, ..PrewarmRequest::one(PrewarmSource::MinReplicas, Duration::ZERO) };
        entry.pool.spawn_prewarm(request);
    }
    eprintln!("{{\"audit\":\"version_added\",\"deployment\":\"{name}\",\"version\":\"{}\"}}", entry.info.version);
    (StatusCode::CREATED, axum::Json(json!({ "deployment": name, "version": entry.info.version, "created": true })))
        .into_response()
}

/// Loads persisted versions of every deployment at start-up.
///
/// # Errors
/// Store errors.
pub async fn load_versions(
    gateway: &Gateway,
    on_version: Option<&VersionHook>,
) -> Result<usize, graphrt_core::store::StoreError> {
    let mut loaded = 0;
    for deployment in gateway.deployments() {
        let prefix = format!("d/{}/versions/", deployment.name);
        let mut infos: Vec<VersionInfo> = gateway
            .store
            .list_objects(&prefix)
            .await?
            .into_iter()
            .filter_map(|(_, object)| serde_json::from_slice(&object.value).ok())
            .collect();
        infos.sort_by_key(|info| info.activated_ms);
        for info in infos {
            if let Some(hook) = on_version {
                hook(&deployment, &info);
            }
            deployment.add_version(info, now_ms());
            loaded += 1;
        }
    }
    Ok(loaded)
}

/// Status of a record as text, for logs.
pub fn status_text(status: RunStatus) -> String {
    serde_json::to_value(status).ok().and_then(|value| value.as_str().map(str::to_owned)).unwrap_or_default()
}
