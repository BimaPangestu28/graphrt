//! `graphrt serve`: an HTTP API with SSE streaming.
//!
//! | Method | Path | Body | Response |
//! | --- | --- | --- | --- |
//! | GET | `/healthz` | – | `ok` (never requires auth) |
//! | GET | `/v1/workflows` | – | workflow names, inputs and env requirements |
//! | POST | `/v1/workflows/{name}/runs` | `{inputs, sys, conversation, stream, async}` | SSE stream, JSON result, or `202` + run id when `async` |
//! | GET | `/v1/runs` | query `workflow`, `status`, `limit`, `cursor` | stored runs, newest first |
//! | GET | `/v1/runs/{run_id}` | – | run record (status, outputs, pause reasons and expected resume inputs) |
//! | GET | `/v1/runs/{run_id}/events` | query `from`, `stream=false` | SSE replay (+ live tail while running), or a JSON array |
//! | POST | `/v1/runs/{run_id}/abort` | – | `202` if the run was active |
//! | POST | `/v1/runs/{run_id}/resume` | `{inputs, secrets, stream, async}` (`snapshot` optional) | SSE stream or JSON result |
//! | DELETE | `/v1/runs/{run_id}` | – | `204`; `409` while the run executes |
//! | GET | `/v1/nodes` | – | node catalog (descriptors with config JSON Schemas) |
//! | POST | `/v1/validate` | workflow document, or `{document: "<yaml/json text>"}` | `{valid, errors, warnings}` |
//! | GET | `/v1/hooks` | – | webhooks configured with `--webhooks` |
//! | POST | `/v1/hooks/{hook}` | any JSON (signature-checked when configured) | `202` + run id; see [`crate::webhooks`] |
//!
//! SSE events use the engine event `type` as the event name and its JSON as
//! data, plus `response` events carrying the ordered answer text
//! (`{node_id, text, is_final}`).
//!
//! Runs are recorded in a run store (`--store <dir>` for a durable directory,
//! in-memory otherwise), so a paused run can be resumed by id. See `docs/serving.md`.
//!
//! Workflows are compiled once at startup. Environment variables declared by a
//! workflow are read from the server's environment (or `--env-file`); requests
//! cannot set them.

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{DefaultBodyLimit, Path as UrlPath, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures::{Stream, StreamExt};
use graphrt_core::engine::{Limits, ResumeFrom, RunInput};
use graphrt_core::node::TokioClock;
use graphrt_core::response::ResponseAssembler;
use graphrt_core::store::{
    RunFilter, RunRecord, RunStatus, RunStore, StoreError, StoredEvent, event_to_stored, now_ms, stored_event_type,
};
use graphrt_core::{Engine, Event, Graph, Selector, Snapshot, Value};
use indexmap::IndexMap;
use serde::Deserialize;

use crate::args::ServeArgs;
use crate::error::{CliError, EXIT_OK};
use crate::extensions::Extensions;
use crate::run::service_options;
use crate::setup::{self, ServiceOptions};
use crate::store::MemoryStore;
use crate::store::live::{self, ActiveRun};

/// One served workflow.
#[derive(Debug)]
pub struct ServedWorkflow {
    pub engine: Engine,
    /// Values for the workflow's declared env variables.
    pub env: IndexMap<String, Value>,
}

/// Shared server state.
#[derive(Debug)]
pub struct AppState {
    workflows: IndexMap<String, ServedWorkflow>,
    runs: Mutex<HashMap<String, Arc<ActiveRun>>>,
    /// Cap on `runs` (executing runs); `0` = unlimited.
    max_active_runs: usize,
    store: Arc<dyn RunStore>,
    api_key: Option<String>,
    next_id: AtomicU64,
    /// Node kinds (built-in and plugins) used for `/v1/nodes` and `/v1/validate`.
    extensions: Extensions,
    /// OpenTelemetry export, when enabled.
    telemetry: Option<Arc<crate::otel::Telemetry>>,
    /// The knowledge base behind `/v1/knowledge`, when served with `--knowledge`.
    #[cfg(feature = "rag")]
    knowledge: Option<crate::knowledge_api::KnowledgeService>,
    /// Webhook triggers under `/v1/hooks` (`--webhooks`).
    webhooks: crate::webhooks::Webhooks,
}

impl AppState {
    /// Create state from compiled engines.
    pub fn new(workflows: IndexMap<String, ServedWorkflow>, api_key: Option<String>) -> Self {
        Self {
            workflows,
            runs: Mutex::new(HashMap::new()),
            max_active_runs: 0,
            store: Arc::new(MemoryStore::new()),
            api_key,
            next_id: AtomicU64::new(1),
            extensions: Extensions::builtin(),
            telemetry: None,
            #[cfg(feature = "rag")]
            knowledge: None,
            webhooks: crate::webhooks::Webhooks::default(),
        }
    }

    /// Serve these webhooks under `/v1/hooks`.
    ///
    /// @throws CliError (usage) - When a hook names a workflow that is not served.
    pub fn with_webhooks(mut self, webhooks: crate::webhooks::Webhooks) -> Result<Self, CliError> {
        if let Some(hook) = webhooks.hooks().find(|hook| !self.workflows.contains_key(&hook.workflow)) {
            return Err(CliError::usage(format!(
                "webhook `{}` names workflow `{}`, which is not served",
                hook.id, hook.workflow
            )));
        }
        self.webhooks = webhooks;
        Ok(self)
    }

    pub(crate) fn webhooks(&self) -> &crate::webhooks::Webhooks {
        &self.webhooks
    }

    /// Whether `headers` carry the API key (always true when none is set).
    pub(crate) fn authorized(&self, headers: &axum::http::HeaderMap) -> bool {
        let Some(expected) = &self.api_key else { return true };
        let provided = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or_default();
        constant_time_eq(provided.as_bytes(), expected.as_bytes())
    }

    /// Serve this knowledge base under `/v1/knowledge`.
    #[cfg(feature = "rag")]
    /// `config` is the knowledge config path; datasets created over the API are saved next to it.
    pub fn with_knowledge(mut self, knowledge: Option<graphrt_rag::KnowledgeBase>, config: Option<PathBuf>) -> Self {
        self.knowledge = knowledge.map(|kb| crate::knowledge_api::KnowledgeService::new(kb, config));
        self
    }

    #[cfg(feature = "rag")]
    pub(crate) fn knowledge(&self) -> Option<&crate::knowledge_api::KnowledgeService> {
        self.knowledge.as_ref()
    }

    /// Trace every run with this telemetry.
    pub fn with_telemetry(mut self, telemetry: Option<Arc<crate::otel::Telemetry>>) -> Self {
        self.telemetry = telemetry;
        self
    }

    /// Use these node kinds (built-ins plus plugins) for the catalog and validation.
    pub fn with_extensions(mut self, extensions: Extensions) -> Self {
        self.extensions = extensions;
        self
    }

    /// Record runs in this store (default: in memory).
    /// Refuse new or resumed runs with `429` while `max` runs execute (`0` = unlimited).
    pub fn with_max_active_runs(mut self, max: usize) -> Self {
        self.max_active_runs = max;
        self
    }

    /// `429` when the server already executes `max_active_runs` runs. Checked before a
    /// run is created, so a refused request leaves no record behind. The check and the
    /// later insert are not atomic: concurrent requests can overshoot by a few runs.
    fn refuse_if_busy(&self) -> Option<Response> {
        if self.max_active_runs == 0 {
            return None;
        }
        let active = self.runs.lock().map(|runs| runs.len()).unwrap_or(usize::MAX);
        if active < self.max_active_runs {
            return None;
        }
        Some(error_response(
            StatusCode::TOO_MANY_REQUESTS,
            &format!("the server is already executing {} runs; retry later", self.max_active_runs),
        ))
    }

    pub fn with_store(mut self, store: Arc<dyn RunStore>) -> Self {
        self.store = store;
        self
    }

    /// The run store.
    pub fn store(&self) -> &Arc<dyn RunStore> {
        &self.store
    }

    /// After a restart: mark runs that were executing as failed. Returns how many.
    pub async fn recover(&self) -> Result<usize, CliError> {
        self.store
            .recover_interrupted(now_ms())
            .await
            .map_err(|e| CliError::failed(format!("run store recovery failed: {e}")))
    }

    fn new_run_id(&self) -> String {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        format!("run-{nanos:x}-{n:x}")
    }

    fn track(&self, run: Arc<ActiveRun>) {
        if let Ok(mut runs) = self.runs.lock() {
            runs.insert(run.run_id.clone(), run);
        }
    }

    fn untrack(&self, run_id: &str) {
        if let Ok(mut runs) = self.runs.lock() {
            runs.remove(run_id);
        }
    }

    fn active(&self, run_id: &str) -> Option<Arc<ActiveRun>> {
        self.runs.lock().ok().and_then(|runs| runs.get(run_id).cloned())
    }

    /// Number of runs currently executing.
    pub fn active_runs(&self) -> usize {
        self.runs.lock().map(|r| r.len()).unwrap_or(0)
    }
}

/// Discover workflow files: a single file, or every `.yaml` / `.yml` / `.json` file in a directory.
pub fn discover(path: &Path) -> Result<Vec<(String, PathBuf)>, CliError> {
    let name_of = |p: &Path| p.file_stem().and_then(|s| s.to_str()).unwrap_or("workflow").to_owned();
    if path.is_file() {
        return Ok(vec![(name_of(path), path.to_path_buf())]);
    }
    let entries =
        std::fs::read_dir(path).map_err(|e| CliError::usage(format!("cannot read `{}`: {e}", path.display())))?;
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let p = entry.path();
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or_default();
        if p.is_file()
            && matches!(ext, "yaml" | "yml" | "json" | "grt")
            && !p.to_string_lossy().ends_with(".snapshot.json")
        {
            found.push((name_of(&p), p));
        }
    }
    found.sort();
    if found.is_empty() {
        return Err(CliError::usage(format!("no workflow files in `{}`", path.display())));
    }
    Ok(found)
}

/// Compile workflows and build services.
pub async fn build_state(
    files: &[(String, PathBuf)],
    options: &ServiceOptions,
    limits: Limits,
    api_key: Option<String>,
) -> Result<AppState, CliError> {
    let mut extensions = Extensions::load(&options.plugins)?;
    let services = Arc::new(setup::services(options, extensions.tools.clone()).await?);
    let mut workflows = IndexMap::new();
    for (name, path) in files {
        if path.extension().and_then(|e| e.to_str()) == Some("json") && is_mcp_config(path) {
            continue;
        }
        let graph = if crate::bundle_cmd::is_bundle(path) {
            // Bundled WebAssembly plugins are registered; services come from the host flags.
            let mut bundle_options = options.clone();
            crate::bundle_cmd::bundle_graph(path, &mut bundle_options, &mut extensions.registry, &options.trust_keys)?
        } else {
            setup::load_graph_with(path, options.model_override.as_deref(), &extensions.registry)?
        };
        let env = setup::workflow_env(&graph, options);
        let engine = Engine::new(graph)
            .with_shared_services(services.clone())
            .with_clock(Arc::new(TokioClock))
            .with_limits(limits.clone());
        workflows.insert(name.clone(), ServedWorkflow { engine, env });
    }
    let state = AppState::new(workflows, api_key).with_extensions(extensions);
    // The same knowledge base the workflows use, so documents added over the API
    // are searchable by running workflows right away.
    #[cfg(feature = "rag")]
    let state = state.with_knowledge(services.get::<graphrt_rag::KnowledgeBase>().cloned(), options.knowledge.clone());
    Ok(state)
}

fn is_mcp_config(path: &Path) -> bool {
    std::fs::read_to_string(path).map(|t| t.contains("\"mcpServers\"")).unwrap_or(false)
}

/// Default upload limit for knowledge base files (`--max-upload-bytes`).
pub const DEFAULT_MAX_UPLOAD_BYTES: usize = 16 * 1024 * 1024;

/// Default cap on runs executing at once (`--max-active-runs`).
pub const DEFAULT_MAX_ACTIVE_RUNS: usize = 256;

/// The HTTP router.
pub fn router(state: Arc<AppState>, max_body_bytes: usize) -> Router {
    router_with_uploads(state, max_body_bytes, DEFAULT_MAX_UPLOAD_BYTES)
}

/// The HTTP router, with a separate body limit for knowledge base file uploads.
pub fn router_with_uploads(state: Arc<AppState>, max_body_bytes: usize, max_upload_bytes: usize) -> Router {
    let api = Router::new()
        .route("/v1/workflows", get(list_workflows))
        .route("/v1/workflows/{name}/runs", post(start_run))
        .route("/v1/runs", get(list_runs))
        .route("/v1/runs/{run_id}", get(get_run).merge(delete(delete_run)))
        .route("/v1/runs/{run_id}/events", get(run_events))
        .route("/v1/runs/{run_id}/abort", post(abort_run))
        .route("/v1/runs/{run_id}/resume", post(resume_run))
        .route("/v1/nodes", get(list_nodes))
        .route("/v1/validate", post(validate_workflow));
    #[cfg(feature = "rag")]
    let api = {
        use crate::knowledge_api as k;
        api.route("/v1/knowledge/datasets", get(k::list_datasets).post(k::create_dataset))
            .route("/v1/knowledge/datasets/{dataset}", delete(k::delete_dataset))
            .route("/v1/knowledge/resources", get(k::list_resources))
            .route("/v1/knowledge/datasets/{dataset}/documents", get(k::list_documents).post(k::put_text_document))
            .route(
                "/v1/knowledge/datasets/{dataset}/documents/{document}",
                get(k::get_document).merge(delete(k::delete_document)).merge(axum::routing::patch(k::patch_document)),
            )
            .route(
                "/v1/knowledge/datasets/{dataset}/documents/{document}/file",
                axum::routing::put(k::put_file_document).layer(DefaultBodyLimit::max(max_upload_bytes)),
            )
            .route("/v1/knowledge/retrieve", post(k::retrieve))
    };
    #[cfg(not(feature = "rag"))]
    let _ = max_upload_bytes;
    let api = api
        .route("/v1/hooks", get(crate::webhooks::list_hooks))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));
    // Webhook deliveries authenticate themselves (signature, else the bearer key).
    let hooks = Router::new().route("/v1/hooks/{hook}", post(crate::webhooks::receive));
    let counted = api.merge(hooks);
    // Opt-in usage telemetry counts API requests (in memory); health checks are not counted.
    #[cfg(feature = "telemetry")]
    let counted = counted.layer(middleware::from_fn(crate::telemetry::count_requests));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(counted)
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .with_state(state)
}

/// Execute `graphrt serve`.
pub async fn serve(args: ServeArgs) -> Result<u8, CliError> {
    let options = service_options(&args.services)?;
    let files = discover(&args.path)?;
    let api_key = std::env::var(&args.api_key_env)
        .ok()
        .or_else(|| options.env_file.iter().find(|(k, _)| *k == args.api_key_env).map(|(_, v)| v.clone()))
        .filter(|k| !k.is_empty());
    let limits = setup::limits(args.services.timeout_ms, args.services.max_steps);
    let telemetry = crate::otel::Telemetry::from_args(&args.services, &options.all_vars())?;
    let store = crate::store::open(args.store.as_ref(), args.max_runs)?;
    let webhooks = match &args.webhooks {
        Some(path) => crate::webhooks::Webhooks::load(path, &options.all_vars(), options.allow_private_network)?,
        None => crate::webhooks::Webhooks::default(),
    };
    let state = Arc::new(
        build_state(&files, &options, limits, api_key.clone())
            .await?
            .with_store(store)
            .with_telemetry(telemetry.clone())
            .with_max_active_runs(args.max_active_runs)
            .with_webhooks(webhooks)?,
    );
    let interrupted = state.recover().await?;
    if interrupted > 0 {
        eprintln!("marked {interrupted} run(s) interrupted by the previous shutdown as failed");
    }
    if let Some(days) = args.store_ttl_days.filter(|d| *d > 0) {
        spawn_retention(Arc::clone(state.store()), days);
    }
    let app = router_with_uploads(state.clone(), args.max_body_bytes, args.max_upload_bytes);

    let listener = tokio::net::TcpListener::bind(&args.addr)
        .await
        .map_err(|e| CliError::usage(format!("cannot listen on {}: {e}", args.addr)))?;
    let loopback = listener.local_addr().map(|a| a.ip().is_loopback()).unwrap_or(false);
    // Without an API key, a loopback server is reachable from any web page through DNS
    // rebinding (the page's own host name resolving to 127.0.0.1). Such requests carry
    // that foreign host name, so only local `Host` values are accepted.
    let app =
        if api_key.is_none() && loopback { app.layer(axum::middleware::from_fn(require_local_host)) } else { app };
    eprintln!("graphrt serving {} workflow(s) on http://{}", state.workflows.len(), args.addr);
    for name in state.workflows.keys() {
        eprintln!("  POST /v1/workflows/{name}/runs");
    }
    for hook in state.webhooks().hooks() {
        eprintln!("  POST /v1/hooks/{} -> {}", hook.id, hook.workflow);
        if !hook.is_signed() && api_key.is_none() {
            eprintln!("warning: webhook `{}` has no `signature` and no API key is set; anyone can trigger it", hook.id);
        }
    }
    #[cfg(feature = "rag")]
    if state.knowledge().is_some() {
        eprintln!("  knowledge base API under /v1/knowledge (uploads up to {} bytes)", args.max_upload_bytes);
    }
    if api_key.is_none() {
        eprintln!("warning: no API key set (${}); the API is unauthenticated", args.api_key_env);
        if !loopback {
            eprintln!(
                "warning: listening on a non-loopback address without an API key: anyone who can reach {} \
                 can start runs with this server's credentials. Set ${} or bind 127.0.0.1",
                args.addr, args.api_key_env
            );
        }
    }
    // Without TCP_NODELAY, the second and later small writes of a streamed (SSE)
    // response wait for the client's delayed ACK: about 40 ms per run on Linux.
    let listener = axum::serve::ListenerExt::tap_io(listener, |tcp: &mut tokio::net::TcpStream| {
        if let Err(e) = tcp.set_nodelay(true) {
            eprintln!("warning: cannot set TCP_NODELAY: {e}");
        }
    });
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            eprintln!("shutting down");
        })
        .await
        .map_err(|e| CliError::failed(format!("server error: {e}")))?;
    if let Some(telemetry) = &telemetry {
        telemetry.shutdown().await;
    }
    Ok(EXIT_OK)
}

/// Reject requests whose `Host` is not a loopback name (DNS rebinding protection for
/// an unauthenticated loopback server).
async fn require_local_host(request: Request, next: Next) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or_else(|| request.uri().authority().map(|a| a.as_str().to_owned()));
    if host.as_deref().is_some_and(is_local_host) {
        next.run(request).await
    } else {
        error_response(
            StatusCode::FORBIDDEN,
            "requests to an unauthenticated server must use a loopback host name (localhost, 127.0.0.1, [::1]); \
             set an API key to serve other host names",
        )
    }
}

/// Whether a `Host` header value names this machine (`localhost`, `*.localhost`,
/// `127.x.y.z`, `[::1]`), with or without a port.
fn is_local_host(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        // `[::1]` or `[::1]:8080`
        match rest.split_once(']') {
            Some((inside, port)) if port.is_empty() || port.starts_with(':') => inside,
            _ => return false,
        }
    } else {
        host.rsplit_once(':').map_or(host, |(name, _port)| name)
    };
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    if name == "localhost" || name.ends_with(".localhost") {
        return true;
    }
    name.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

async fn require_auth(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    if state.authorized(request.headers()) {
        next.run(request).await
    } else {
        error_response(StatusCode::UNAUTHORIZED, "missing or invalid bearer token")
    }
}

/// Compare without early exit so response timing does not reveal the key prefix.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= usize::from(x ^ y);
    }
    diff == 0
}

pub(crate) fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

async fn list_workflows(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let items: Vec<serde_json::Value> = state
        .workflows
        .iter()
        .map(|(name, wf)| {
            let doc = wf.engine.graph().workflow();
            let inputs: Vec<serde_json::Value> = doc
                .graph
                .nodes
                .iter()
                .filter(|n| n.kind == "start")
                .filter_map(|n| n.config.get("variables").cloned())
                .collect();
            serde_json::json!({
                "name": name,
                "title": doc.name,
                "description": doc.description,
                "inputs": inputs.into_iter().next().unwrap_or(serde_json::Value::Array(Vec::new())),
                "env": doc.env.iter().map(|e| serde_json::json!({
                    "name": e.name,
                    "configured": wf.env.contains_key(&e.name) || e.value.is_some(),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Json(serde_json::json!({ "workflows": items }))
}

async fn list_nodes(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "nodes": state.extensions.registry.descriptors() }))
}

/// Validate a workflow document against the node kinds of this build.
///
/// The body is either the document itself (JSON) or `{"document": "<YAML or JSON text>"}`.
async fn validate_workflow(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    Json(validation_report(body, &state.extensions.registry))
}

/// `{valid, errors, warnings}` for a workflow document.
pub fn validation_report(body: serde_json::Value, registry: &graphrt_core::Registry) -> serde_json::Value {
    let parsed = match body.get("document").and_then(serde_json::Value::as_str) {
        Some(text) => graphrt_core::Workflow::parse(text),
        None => graphrt_core::Workflow::from_value(body),
    };
    let report = |valid: bool, errors: Vec<String>, warnings: Vec<String>| serde_json::json!({ "valid": valid, "errors": errors, "warnings": warnings });
    let workflow = match parsed {
        Ok(w) => w,
        Err(e) => return report(false, vec![e.to_string()], Vec::new()),
    };
    match Graph::build(workflow, registry) {
        Ok(graph) => report(true, Vec::new(), graph.warnings().to_vec()),
        Err(e) => report(false, vec![crate::extensions::explain_unknown_kind(e.to_string())], Vec::new()),
    }
}

/// Body of `POST /v1/workflows/{name}/runs`.
#[derive(Debug, Default, Deserialize)]
pub struct RunRequest {
    #[serde(default)]
    pub inputs: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub sys: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub conversation: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub stream: bool,
    /// Return `202` with the run id immediately; follow the run via `GET /v1/runs/{id}`
    /// and `/events`.
    #[serde(default, rename = "async")]
    pub run_async: bool,
}

/// Body of `POST /v1/runs/{run_id}/resume`.
#[derive(Debug, Deserialize)]
pub struct ResumeRequest {
    /// Snapshot from a paused response. Optional: when omitted, the server uses the
    /// snapshot it stored when the run paused.
    #[serde(default)]
    pub snapshot: Option<Snapshot>,
    /// Input for paused nodes keyed by node id (or `<frame>/<node_id>`).
    #[serde(default)]
    pub inputs: serde_json::Map<String, serde_json::Value>,
    /// Secrets (other than `env.*`) to re-supply, keyed by `namespace.name`.
    /// Snapshots never contain secret values.
    #[serde(default)]
    pub secrets: IndexMap<String, String>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default, rename = "async")]
    pub run_async: bool,
}

/// How the caller wants the result of a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Delivery {
    /// Wait and return the final JSON result.
    Blocking,
    /// Server-sent events.
    Stream,
    /// `202` immediately.
    Detached,
}

impl Delivery {
    fn of(stream: bool, run_async: bool) -> Self {
        if run_async {
            Delivery::Detached
        } else if stream {
            Delivery::Stream
        } else {
            Delivery::Blocking
        }
    }
}

fn to_values(map: serde_json::Map<String, serde_json::Value>) -> IndexMap<String, Value> {
    map.into_iter().map(|(k, v)| (k, Value::from_json(v))).collect()
}

fn store_error(e: &StoreError) -> Response {
    let status = match e {
        StoreError::NotFound(_) => StatusCode::NOT_FOUND,
        StoreError::Conflict(_) => StatusCode::CONFLICT,
        StoreError::InvalidId(_) => StatusCode::BAD_REQUEST,
        _ => {
            // I/O and corruption errors name server paths: log them, answer generically.
            eprintln!("run store error: {e}");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "run store error (details in the server log)");
        }
    };
    error_response(status, &e.to_string())
}

async fn start_run(
    State(state): State<Arc<AppState>>,
    UrlPath(name): UrlPath<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<RunRequest>,
) -> Response {
    let Some(workflow) = state.workflows.get(&name) else {
        return error_response(StatusCode::NOT_FOUND, &format!("unknown workflow `{name}`"));
    };
    if let Some(busy) = state.refuse_if_busy() {
        return busy;
    }
    let run_id = state.new_run_id();
    let record = RunRecord::running(&run_id, &name, workflow.engine.graph().fingerprint(), now_ms());
    if let Err(e) = state.store.create(record.clone()).await {
        return store_error(&e);
    }
    let mut input = RunInput::new();
    input.run_id = Some(run_id);
    input.inputs = to_values(body.inputs);
    input.sys = to_values(body.sys);
    input.conversation = to_values(body.conversation);
    input.env = workflow.env.clone();
    execute(&state, &workflow.engine, record, input, Delivery::of(body.stream, body.run_async), traceparent(&headers))
        .await
}

/// W3C `traceparent` of the incoming request, continued by the run's trace.
fn traceparent(headers: &axum::http::HeaderMap) -> Option<String> {
    headers.get("traceparent").and_then(|v| v.to_str().ok()).map(str::to_owned)
}

async fn resume_run(
    State(state): State<Arc<AppState>>,
    UrlPath(run_id): UrlPath<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<ResumeRequest>,
) -> Response {
    if state.active(&run_id).is_some() {
        return error_response(StatusCode::CONFLICT, "run is already executing");
    }
    if let Some(busy) = state.refuse_if_busy() {
        return busy;
    }
    let (record, snapshot) = match body.snapshot {
        // Resume from the server's own records.
        None => {
            let record = match state.store.claim_resume(&run_id, now_ms()).await {
                Ok(r) => r,
                Err(e) => return store_error(&e),
            };
            match state.store.load_snapshot(&run_id).await {
                Ok(Some(snapshot)) => (record, snapshot),
                Ok(None) | Err(_) => {
                    // Undo the claim so the run stays visible as paused.
                    let mut back = record;
                    back.status = RunStatus::Paused;
                    if let Err(e) = state.store.put(back).await {
                        eprintln!("warning: run {run_id}: cannot restore paused status: {e}");
                    }
                    return error_response(
                        StatusCode::CONFLICT,
                        "no stored snapshot for this run; pass the `snapshot` from the paused response",
                    );
                }
            }
        }
        // Client-held snapshot (also works for runs this server never recorded).
        Some(snapshot) => {
            if snapshot.run_id != run_id {
                return error_response(StatusCode::BAD_REQUEST, "snapshot belongs to a different run");
            }
            let record = match state.store.claim_resume(&run_id, now_ms()).await {
                Ok(r) => r,
                Err(StoreError::NotFound(_)) => {
                    let Some((name, _)) = state
                        .workflows
                        .iter()
                        .find(|(_, w)| w.engine.graph().fingerprint() == snapshot.graph_fingerprint)
                    else {
                        return error_response(StatusCode::NOT_FOUND, "no served workflow matches this snapshot");
                    };
                    let record = RunRecord::running(&run_id, name, &snapshot.graph_fingerprint, now_ms());
                    if let Err(e) = state.store.create(record.clone()).await {
                        return store_error(&e);
                    }
                    record
                }
                Err(e) => return store_error(&e),
            };
            (record, snapshot)
        }
    };
    let workflow = state
        .workflows
        .get(&record.workflow)
        .or_else(|| state.workflows.values().find(|w| w.engine.graph().fingerprint() == snapshot.graph_fingerprint));
    let Some(workflow) = workflow else {
        return error_response(StatusCode::NOT_FOUND, &format!("workflow `{}` is not served", record.workflow));
    };
    let mut input = RunInput::new();
    input.run_id = Some(run_id);
    input.env = workflow.env.clone();
    let mut resume = ResumeFrom::new(snapshot, to_values(body.inputs));
    for (selector, secret) in body.secrets {
        resume = resume.secret(selector, secret);
    }
    input.resume = Some(resume);
    execute(&state, &workflow.engine, record, input, Delivery::of(body.stream, body.run_async), traceparent(&headers))
        .await
}

async fn abort_run(State(state): State<Arc<AppState>>, UrlPath(run_id): UrlPath<String>) -> Response {
    match state.active(&run_id) {
        Some(run) => {
            run.handle().abort("aborted via API");
            (StatusCode::ACCEPTED, Json(serde_json::json!({ "run_id": run_id, "status": "aborting" }))).into_response()
        }
        None => error_response(StatusCode::NOT_FOUND, "no active run with this id"),
    }
}

/// Query of `GET /v1/runs`.
#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    pub workflow: Option<String>,
    pub status: Option<String>,
    #[serde(default)]
    pub limit: usize,
    pub cursor: Option<String>,
}

async fn list_runs(State(state): State<Arc<AppState>>, Query(query): Query<ListQuery>) -> Response {
    let status = match query.status.as_deref().map(|s| (s, RunStatus::parse(s))) {
        Some((raw, None)) => return error_response(StatusCode::BAD_REQUEST, &format!("unknown status `{raw}`")),
        Some((_, parsed)) => parsed,
        None => None,
    };
    let filter = RunFilter { workflow: query.workflow, status, limit: query.limit, cursor: query.cursor };
    match state.store.list(filter).await {
        Ok(page) => {
            let runs: Vec<serde_json::Value> = page.runs.iter().map(|r| record_json(&state, r)).collect();
            Json(serde_json::json!({ "runs": runs, "next_cursor": page.next_cursor })).into_response()
        }
        Err(e) => store_error(&e),
    }
}

async fn get_run(State(state): State<Arc<AppState>>, UrlPath(run_id): UrlPath<String>) -> Response {
    match state.store.get(&run_id).await {
        Ok(Some(record)) => Json(record_json(&state, &record)).into_response(),
        Ok(None) => error_response(StatusCode::NOT_FOUND, "unknown run"),
        Err(e) => store_error(&e),
    }
}

async fn delete_run(State(state): State<Arc<AppState>>, UrlPath(run_id): UrlPath<String>) -> Response {
    if state.active(&run_id).is_some() {
        return error_response(StatusCode::CONFLICT, "run is executing; abort it first");
    }
    match state.store.delete(&run_id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error_response(StatusCode::NOT_FOUND, "unknown run"),
        Err(e) => store_error(&e),
    }
}

/// A run record as returned by the API, with the input each paused node expects.
fn record_json(state: &AppState, record: &RunRecord) -> serde_json::Value {
    let mut json = serde_json::to_value(record).unwrap_or(serde_json::Value::Null);
    if let Some(obj) = json.as_object_mut() {
        obj.insert("active".into(), serde_json::Value::Bool(state.active(&record.run_id).is_some()));
        if record.status == RunStatus::Paused {
            let expected: Vec<serde_json::Value> = record
                .pause_reasons
                .iter()
                .filter_map(|reason| {
                    let node = reason.node_id.as_deref()?;
                    // Resume inputs are keyed by node id, or `<frame>/<node_id>` inside containers.
                    let key =
                        if reason.frame.is_empty() { node.to_owned() } else { format!("{}/{node}", reason.frame) };
                    Some(serde_json::json!({ "key": key, "node_id": node, "kind": reason.kind, "data": reason.data }))
                })
                .collect();
            obj.insert("resume_inputs".into(), serde_json::Value::Array(expected));
        }
    }
    json
}

/// Query of `GET /v1/runs/{id}/events`.
#[derive(Debug, Default, Deserialize)]
pub struct EventsQuery {
    #[serde(default)]
    pub from: u64,
    /// `false` returns stored events as a JSON array instead of SSE.
    pub stream: Option<bool>,
}

async fn run_events(
    State(state): State<Arc<AppState>>,
    UrlPath(run_id): UrlPath<String>,
    Query(query): Query<EventsQuery>,
) -> Response {
    let active = state.active(&run_id);
    // Take the live subscription first: events the driver stores after this point
    // reach us through it, and the stored part is cut at `stored_before`.
    let subscription = active.as_ref().map(|run| (run.stored_before, run.subscribe()));
    let stored_until = subscription.as_ref().map(|(before, _)| *before);
    let mut stored = match state.store.events(&run_id, query.from).await {
        Ok(events) => events,
        Err(e) => return store_error(&e),
    };
    if let Some(limit) = stored_until {
        let keep = usize::try_from(limit.saturating_sub(query.from)).unwrap_or(usize::MAX);
        stored.truncate(keep);
    }
    if query.stream == Some(false) {
        let mut all = stored;
        if let Some((_, sub)) = subscription {
            all.extend(sub.backlog.iter().map(|event| event_to_stored(event)));
        }
        return Json(all).into_response();
    }
    let replay = futures::stream::iter(stored.into_iter().map(|json| Ok(stored_sse(&json))));
    match subscription {
        None => Sse::new(replay.boxed()).into_response(),
        Some((_, sub)) => {
            let backlog = futures::stream::iter(sub.backlog.into_iter().map(|e| Ok(sse_event(&e))));
            let live = if sub.done { futures::stream::empty().boxed() } else { live_events(sub.live).boxed() };
            Sse::new(replay.chain(backlog).chain(live)).keep_alive(KeepAlive::default()).into_response()
        }
    }
}

/// Events from a broadcast receiver until the terminal event.
fn live_events(
    rx: tokio::sync::broadcast::Receiver<live::SharedEvent>,
) -> impl Stream<Item = Result<SseEvent, Infallible>> + Send + 'static {
    futures::stream::unfold((rx, false), |(mut rx, finished)| async move {
        if finished {
            return None;
        }
        match rx.recv().await {
            Ok(event) => {
                let terminal = event.is_terminal();
                Some((Ok(sse_event(&event)), (rx, terminal)))
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                let data = serde_json::json!({ "type": "lagged", "skipped": skipped });
                Some((Ok(SseEvent::default().event("lagged").data(data.to_string())), (rx, false)))
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => None,
        }
    })
}

/// Aborts the run when a synchronous caller (blocking or streaming) goes away before
/// the run finished. Detached runs never get one.
struct AbortOnDrop(Arc<ActiveRun>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if !self.0.is_done() {
            self.0.handle().abort("client disconnected");
        }
    }
}

async fn execute(
    state: &Arc<AppState>,
    engine: &Engine,
    record: RunRecord,
    input: RunInput,
    delivery: Delivery,
    traceparent: Option<String>,
) -> Response {
    // The launched run's assembler records the answer in the store; a second one,
    // fed from the same events, produces `response` SSE events for a streaming caller.
    let stream_assembler = new_assembler(engine, &input);
    let run_id = record.run_id.clone();
    let active = launch(state, engine, record, input, traceparent.as_deref());
    match delivery {
        Delivery::Detached => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "run_id": run_id,
                "status": "running",
                "run": format!("/v1/runs/{run_id}"),
                "events": format!("/v1/runs/{run_id}/events"),
            })),
        )
            .into_response(),
        Delivery::Stream => {
            // The subscription's backlog covers events emitted before subscribing.
            let subscription = active.subscribe();
            let guard = AbortOnDrop(active);
            Sse::new(sse_stream(subscription, stream_assembler, guard)).keep_alive(KeepAlive::default()).into_response()
        }
        Delivery::Blocking => {
            let guard = AbortOnDrop(Arc::clone(&active));
            let outcome = active.wait().await;
            drop(guard);
            result_json(&run_id, outcome)
        }
    }
}

fn new_assembler(engine: &Engine, input: &RunInput) -> ResponseAssembler {
    let mut assembler = ResponseAssembler::new(engine.graph());
    if let Some(resume) = &input.resume {
        // Nodes that finished before the pause will not re-run; seed their values.
        assembler = assembler.resume_from(&resume.snapshot.root);
    }
    for (k, v) in &input.sys {
        assembler = assembler.with_value(&Selector::of("sys", k), v.clone());
    }
    assembler
}

/// Start driving a run in the background and track it as active.
fn launch(
    state: &Arc<AppState>,
    engine: &Engine,
    record: RunRecord,
    input: RunInput,
    traceparent: Option<&str>,
) -> Arc<ActiveRun> {
    let tracer = crate::otel::tracer_for(state.telemetry.as_ref(), engine.graph(), traceparent);
    let assembler = new_assembler(engine, &input);
    let run = engine.run(input);
    let weak = Arc::downgrade(state);
    let on_finish: live::OnFinish = Box::new(move |id| {
        if let Some(state) = weak.upgrade() {
            state.untrack(id);
        }
    });
    let active = live::spawn(run, assembler, record, Arc::clone(&state.store), on_finish, tracer);
    state.track(Arc::clone(&active));
    #[cfg(feature = "telemetry")]
    crate::telemetry::count_run();
    active
}

/// Result of [`launch_detached`].
#[derive(Debug)]
pub(crate) enum Launch {
    /// A new run is executing.
    Started(Arc<ActiveRun>),
    /// A run with this id already exists (an idempotent redelivery).
    Duplicate { run_id: String, status: String },
}

/// Start a background run of workflow `name` with these inputs, for callers other
/// than the run API (webhooks). With `run_id`, an existing run of that id is
/// reported as [`Launch::Duplicate`] instead of starting another.
///
/// @throws Response - 404 for an unknown workflow, or the store's error response.
pub(crate) async fn launch_detached(
    state: &Arc<AppState>,
    name: &str,
    run_id: Option<String>,
    inputs: IndexMap<String, Value>,
    traceparent: Option<String>,
) -> Result<Launch, Response> {
    let Some(workflow) = state.workflows.get(name) else {
        return Err(error_response(StatusCode::NOT_FOUND, &format!("unknown workflow `{name}`")));
    };
    // Webhook runs count against `--max-active-runs` like API runs; senders retry on 429.
    if let Some(busy) = state.refuse_if_busy() {
        return Err(busy);
    }
    let run_id = run_id.unwrap_or_else(|| state.new_run_id());
    let record = RunRecord::running(&run_id, name, workflow.engine.graph().fingerprint(), now_ms());
    match state.store.create(record.clone()).await {
        Ok(()) => {}
        // `create` is atomic, so concurrent redeliveries start one run between them.
        Err(StoreError::Conflict(_)) => {
            let status = match state.store.get(&run_id).await {
                Ok(Some(existing)) => crate::store::status_name(existing.status),
                Ok(None) | Err(_) => "unknown".to_owned(),
            };
            return Ok(Launch::Duplicate { run_id, status });
        }
        Err(e) => return Err(store_error(&e)),
    }
    let mut input = RunInput::new();
    input.run_id = Some(run_id);
    input.inputs = inputs;
    input.env = workflow.env.clone();
    Ok(Launch::Started(launch(state, &workflow.engine, record, input, traceparent.as_deref())))
}

fn sse_stream(
    subscription: live::Subscription,
    mut assembler: ResponseAssembler,
    guard: AbortOnDrop,
) -> impl Stream<Item = Result<SseEvent, Infallible>> + Send + 'static {
    let backlog = futures::stream::iter(subscription.backlog);
    let live = futures::stream::unfold(subscription.live, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(event) => return Some((event, rx)),
                // A streaming caller that cannot keep up skips events, like any lagging listener.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    let events = if subscription.done { backlog.boxed() } else { backlog.chain(live).boxed() };
    // End right after the terminal event: the broadcast channel itself stays open
    // while the run is tracked, so waiting for it to close would hang.
    let events = futures::stream::unfold((events, false), |(mut events, finished)| async move {
        if finished {
            return None;
        }
        let event = events.next().await?;
        let terminal = event.is_terminal();
        Some((event, (events, terminal)))
    });
    let mut chunks = Vec::new();
    events.flat_map(move |event| {
        let _keep_alive = &guard;
        let mut out = Vec::new();
        assembler.push_into(&event, &mut chunks);
        for chunk in chunks.drain(..) {
            let data = serde_json::json!({ "node_id": chunk.node_id, "text": chunk.text, "is_final": chunk.is_final });
            out.push(Ok(SseEvent::default().event("response").data(data.to_string())));
        }
        out.push(Ok(sse_event(&event)));
        futures::stream::iter(out)
    })
}

/// A live event; unlike stored ones, `run_paused` keeps its snapshot here.
fn sse_event(event: &Event) -> SseEvent {
    let text = serde_json::to_string(event).unwrap_or_else(|_| "null".to_owned());
    match serde_json::value::RawValue::from_string(text) {
        Ok(json) => stored_sse(&json),
        Err(_) => SseEvent::default().event("event").data("null"),
    }
}

/// One SSE message per event, named after the event's `type`.
fn stored_sse(event: &StoredEvent) -> SseEvent {
    let name = stored_event_type(event).unwrap_or("event");
    SseEvent::default().event(name).data(event.get())
}

/// The JSON body returned to blocking callers.
fn result_json(run_id: &str, outcome: live::Outcome) -> Response {
    let answer = outcome.answer;
    let body = match outcome.terminal {
        Some(Event::RunSucceeded { outputs, usage, handled_errors, elapsed_ms }) => serde_json::json!({
            "run_id": run_id, "status": "succeeded", "outputs": outputs, "answer": answer,
            "usage": usage, "handled_errors": handled_errors, "elapsed_ms": elapsed_ms,
        }),
        Some(Event::RunFailed { error, error_type, usage, elapsed_ms }) => serde_json::json!({
            "run_id": run_id, "status": "failed", "error": error, "error_type": error_type,
            "usage": usage, "elapsed_ms": elapsed_ms,
        }),
        Some(Event::RunAborted { reason, elapsed_ms }) => serde_json::json!({
            "run_id": run_id, "status": "aborted", "reason": reason, "elapsed_ms": elapsed_ms,
        }),
        Some(Event::RunPaused { reasons, snapshot }) => serde_json::json!({
            "run_id": run_id, "status": "paused", "reasons": reasons, "snapshot": snapshot, "answer": answer,
        }),
        _ => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "run ended without a terminal event"),
    };
    Json(body).into_response()
}

/// Delete runs older than `days` (any status except running), hourly.
fn spawn_retention(store: Arc<dyn RunStore>, days: u64) {
    tokio::spawn(async move {
        let ttl_ms = days.saturating_mul(24 * 60 * 60 * 1000);
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60 * 60));
        loop {
            tick.tick().await;
            match store.purge_before(now_ms().saturating_sub(ttl_ms)).await {
                Ok(0) => {}
                Ok(n) => eprintln!("run store: removed {n} run(s) older than {days} day(s)"),
                Err(e) => eprintln!("warning: run store cleanup failed: {e}"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{constant_time_eq, is_local_host};

    #[test]
    fn local_host_names() {
        for host in ["localhost", "localhost:8080", "LOCALHOST.", "app.localhost:3000", "127.0.0.1", "127.0.0.1:8080"] {
            assert!(is_local_host(host), "{host}");
        }
        for host in ["[::1]", "[::1]:8080", "127.1.2.3:80"] {
            assert!(is_local_host(host), "{host}");
        }
        for host in [
            "evil.example",
            "evil.example:8080",
            "localhost.evil.example",
            "127.0.0.1.nip.io",
            "[::1]x",
            "10.0.0.1:8080",
            "",
        ] {
            assert!(!is_local_host(host), "{host}");
        }
    }

    #[test]
    fn constant_time_comparison() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret2"));
        assert!(!constant_time_eq(b"", b"x"));
    }
}
