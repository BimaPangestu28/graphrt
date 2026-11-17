//! `graphrt serve`: an HTTP API with SSE streaming.
//!
//! | Method | Path | Body | Response |
//! | --- | --- | --- | --- |
//! | GET | `/healthz` | – | `ok` (never requires auth) |
//! | GET | `/v1/workflows` | – | workflow names, inputs and env requirements |
//! | POST | `/v1/workflows/{name}/runs` | `{inputs, sys, conversation, stream}` | SSE stream or JSON result |
//! | POST | `/v1/runs/{run_id}/abort` | – | `202` if the run was active |
//! | POST | `/v1/runs/{run_id}/resume` | `{snapshot, inputs, stream}` | SSE stream or JSON result |
//! | GET | `/v1/nodes` | – | node catalog (descriptors with config JSON Schemas) |
//! | POST | `/v1/validate` | workflow document, or `{document: "<yaml/json text>"}` | `{valid, errors, warnings}` |
//!
//! SSE events use the engine event `type` as the event name and its JSON as
//! data, plus `response` events carrying the ordered answer text
//! (`{node_id, text, is_final}`).
//!
//! Workflows are compiled once at startup. Environment variables declared by a
//! workflow are read from the server's environment (or `--env-file`); requests
//! cannot set them.

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{DefaultBodyLimit, Path as UrlPath, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{Stream, StreamExt};
use graphrt_core::engine::{Limits, ResumeFrom, RunHandle, RunInput, RunStream};
use graphrt_core::node::TokioClock;
use graphrt_core::response::ResponseAssembler;
use graphrt_core::{Engine, Event, Graph, Selector, Snapshot, Value};
use indexmap::IndexMap;
use serde::Deserialize;

use crate::args::ServeArgs;
use crate::error::{CliError, EXIT_OK};
use crate::extensions::Extensions;
use crate::run::service_options;
use crate::setup::{self, ServiceOptions};

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
    runs: Mutex<HashMap<String, RunHandle>>,
    api_key: Option<String>,
    next_id: AtomicU64,
    /// Node kinds (built-in and plugins) used for `/v1/nodes` and `/v1/validate`.
    extensions: Extensions,
}

impl AppState {
    /// Create state from compiled engines.
    pub fn new(workflows: IndexMap<String, ServedWorkflow>, api_key: Option<String>) -> Self {
        Self {
            workflows,
            runs: Mutex::new(HashMap::new()),
            api_key,
            next_id: AtomicU64::new(1),
            extensions: Extensions::builtin(),
        }
    }

    /// Use these node kinds (built-ins plus plugins) for the catalog and validation.
    pub fn with_extensions(mut self, extensions: Extensions) -> Self {
        self.extensions = extensions;
        self
    }

    fn new_run_id(&self) -> String {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        format!("run-{nanos:x}-{n:x}")
    }

    fn track(&self, run_id: &str, handle: RunHandle) {
        if let Ok(mut runs) = self.runs.lock() {
            runs.insert(run_id.to_owned(), handle);
        }
    }

    fn untrack(&self, run_id: &str) {
        if let Ok(mut runs) = self.runs.lock() {
            runs.remove(run_id);
        }
    }

    fn handle(&self, run_id: &str) -> Option<RunHandle> {
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
        if p.is_file() && matches!(ext, "yaml" | "yml" | "json") && !p.to_string_lossy().ends_with(".snapshot.json") {
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
    let extensions = Extensions::load(&options.plugins)?;
    let services = Arc::new(setup::services(options, extensions.tools.clone()).await?);
    let mut workflows = IndexMap::new();
    for (name, path) in files {
        if path.extension().and_then(|e| e.to_str()) == Some("json") && is_mcp_config(path) {
            continue;
        }
        let graph = setup::load_graph_with(path, options.model_override.as_deref(), &extensions.registry)?;
        let env = setup::workflow_env(&graph, options);
        let engine = Engine::new(graph)
            .with_shared_services(services.clone())
            .with_clock(Arc::new(TokioClock))
            .with_limits(limits.clone());
        workflows.insert(name.clone(), ServedWorkflow { engine, env });
    }
    Ok(AppState::new(workflows, api_key).with_extensions(extensions))
}

fn is_mcp_config(path: &Path) -> bool {
    std::fs::read_to_string(path).map(|t| t.contains("\"mcpServers\"")).unwrap_or(false)
}

/// The HTTP router.
pub fn router(state: Arc<AppState>, max_body_bytes: usize) -> Router {
    let api = Router::new()
        .route("/v1/workflows", get(list_workflows))
        .route("/v1/workflows/{name}/runs", post(start_run))
        .route("/v1/runs/{run_id}/abort", post(abort_run))
        .route("/v1/runs/{run_id}/resume", post(resume_run))
        .route("/v1/nodes", get(list_nodes))
        .route("/v1/validate", post(validate_workflow))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(api)
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
    let state = Arc::new(build_state(&files, &options, limits, api_key.clone()).await?);
    let app = router(state.clone(), args.max_body_bytes);

    let listener = tokio::net::TcpListener::bind(&args.addr)
        .await
        .map_err(|e| CliError::usage(format!("cannot listen on {}: {e}", args.addr)))?;
    eprintln!("graphrt serving {} workflow(s) on http://{}", state.workflows.len(), args.addr);
    for name in state.workflows.keys() {
        eprintln!("  POST /v1/workflows/{name}/runs");
    }
    if api_key.is_none() {
        eprintln!("warning: no API key set (${}); the API is unauthenticated", args.api_key_env);
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            eprintln!("shutting down");
        })
        .await
        .map_err(|e| CliError::failed(format!("server error: {e}")))?;
    Ok(EXIT_OK)
}

async fn require_auth(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    let Some(expected) = &state.api_key else {
        return next.run(request).await;
    };
    let provided = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    if constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        next.run(request).await
    } else {
        error_response(StatusCode::UNAUTHORIZED, "missing or invalid bearer token")
    }
}

/// Compare without early exit so response timing does not reveal the key prefix.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= usize::from(x ^ y);
    }
    diff == 0
}

fn error_response(status: StatusCode, message: &str) -> Response {
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
}

/// Body of `POST /v1/runs/{run_id}/resume`.
#[derive(Debug, Deserialize)]
pub struct ResumeRequest {
    pub snapshot: Snapshot,
    /// Input for paused nodes keyed by node id.
    #[serde(default)]
    pub inputs: serde_json::Map<String, serde_json::Value>,
    /// Secrets (other than `env.*`) to re-supply, keyed by `namespace.name`.
    /// Snapshots never contain secret values.
    #[serde(default)]
    pub secrets: IndexMap<String, String>,
    #[serde(default)]
    pub stream: bool,
}

fn to_values(map: serde_json::Map<String, serde_json::Value>) -> IndexMap<String, Value> {
    map.into_iter().map(|(k, v)| (k, Value::from_json(v))).collect()
}

async fn start_run(
    State(state): State<Arc<AppState>>,
    UrlPath(name): UrlPath<String>,
    Json(body): Json<RunRequest>,
) -> Response {
    let Some(workflow) = state.workflows.get(&name) else {
        return error_response(StatusCode::NOT_FOUND, &format!("unknown workflow `{name}`"));
    };
    let run_id = state.new_run_id();
    let mut input = RunInput::new();
    input.run_id = Some(run_id.clone());
    input.inputs = to_values(body.inputs);
    input.sys = to_values(body.sys);
    input.conversation = to_values(body.conversation);
    input.env = workflow.env.clone();
    execute(state.clone(), &workflow.engine, run_id, input, body.stream).await
}

async fn resume_run(
    State(state): State<Arc<AppState>>,
    UrlPath(run_id): UrlPath<String>,
    Json(body): Json<ResumeRequest>,
) -> Response {
    if body.snapshot.run_id != run_id {
        return error_response(StatusCode::BAD_REQUEST, "snapshot belongs to a different run");
    }
    let fingerprint = body.snapshot.graph_fingerprint.clone();
    let Some(workflow) = state.workflows.values().find(|w| w.engine.graph().fingerprint() == fingerprint) else {
        return error_response(StatusCode::NOT_FOUND, "no served workflow matches this snapshot");
    };
    if state.handle(&run_id).is_some() {
        return error_response(StatusCode::CONFLICT, "run is already executing");
    }
    let mut input = RunInput::new();
    input.run_id = Some(run_id.clone());
    input.env = workflow.env.clone();
    let mut resume = ResumeFrom::new(body.snapshot, to_values(body.inputs));
    for (selector, secret) in body.secrets {
        resume = resume.secret(selector, secret);
    }
    input.resume = Some(resume);
    execute(state.clone(), &workflow.engine, run_id, input, body.stream).await
}

async fn abort_run(State(state): State<Arc<AppState>>, UrlPath(run_id): UrlPath<String>) -> Response {
    match state.handle(&run_id) {
        Some(handle) => {
            handle.abort("aborted via API");
            (StatusCode::ACCEPTED, Json(serde_json::json!({ "run_id": run_id, "status": "aborting" }))).into_response()
        }
        None => error_response(StatusCode::NOT_FOUND, "no active run with this id"),
    }
}

/// Removes a run from the active set when the response (or SSE stream) is dropped,
/// including when the client disconnects mid-stream.
struct RunGuard {
    state: Arc<AppState>,
    run_id: String,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.state.untrack(&self.run_id);
    }
}

async fn execute(state: Arc<AppState>, engine: &Engine, run_id: String, input: RunInput, stream: bool) -> Response {
    let mut assembler = ResponseAssembler::new(engine.graph());
    if let Some(resume) = &input.resume {
        // Nodes that finished before the pause will not re-run; seed their values.
        assembler = assembler.resume_from(&resume.snapshot.root);
    }
    for (k, v) in &input.sys {
        assembler = assembler.with_value(&Selector::of("sys", k), v.clone());
    }
    let run = engine.run(input);
    state.track(&run_id, run.handle());
    let guard = RunGuard { state, run_id: run_id.clone() };
    if stream {
        Sse::new(sse_stream(run, assembler, guard)).keep_alive(KeepAlive::default()).into_response()
    } else {
        collect(run, assembler, guard, run_id, engine.graph()).await
    }
}

fn sse_stream(
    run: RunStream,
    mut assembler: ResponseAssembler,
    guard: RunGuard,
) -> impl Stream<Item = Result<SseEvent, Infallible>> + Send + 'static {
    run.flat_map(move |event| {
        let _keep_alive = &guard;
        let mut out = Vec::new();
        for chunk in assembler.push(&event) {
            let data = serde_json::json!({ "node_id": chunk.node_id, "text": chunk.text, "is_final": chunk.is_final });
            out.push(Ok(SseEvent::default().event("response").data(data.to_string())));
        }
        out.push(Ok(sse_event(&event)));
        futures::stream::iter(out)
    })
}

fn sse_event(event: &Event) -> SseEvent {
    let json = serde_json::to_value(event).unwrap_or(serde_json::Value::Null);
    let name = json.get("type").and_then(|t| t.as_str()).unwrap_or("event").to_owned();
    SseEvent::default().event(name).data(json.to_string())
}

async fn collect(
    mut run: RunStream,
    mut assembler: ResponseAssembler,
    guard: RunGuard,
    run_id: String,
    _graph: &Arc<Graph>,
) -> Response {
    let mut terminal = None;
    while let Some(event) = run.next().await {
        assembler.push(&event);
        if event.is_terminal() {
            terminal = Some(event);
        }
    }
    drop(guard);
    let answer = assembler.text_so_far().to_owned();
    let body = match terminal {
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

#[cfg(test)]
mod tests {
    use super::constant_time_eq;

    #[test]
    fn constant_time_comparison() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret2"));
        assert!(!constant_time_eq(b"", b"x"));
    }
}
