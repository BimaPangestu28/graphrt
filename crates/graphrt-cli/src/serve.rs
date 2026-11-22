//! `graphrt serve`: an HTTP API with SSE streaming.
//!
//! | Method | Path | Body | Response |
//! | --- | --- | --- | --- |
//! | GET | `/healthz` | – | `ok` (never requires auth) |
//! | GET | `/readyz` | – | `ready`, or `503 draining` during a shutdown drain (never requires auth) |
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
//! | GET | `/v1/runs/{run_id}/scores` | – | evaluation scores (feature `scores`, see [`crate::scores`]) |
//! | POST | `/v1/runs/{run_id}/feedback` | `{name, value, comment?, user?}` | `201` + the stored `human` score |
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
use crate::header_inputs::{HeaderBindings, HeaderInputSpec};
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
    /// Set by a shutdown drain: new and resumed runs are refused with `503`.
    draining: AtomicBool,
    store: Arc<dyn RunStore>,
    /// Evaluation scores, kept by the same backend as `store`.
    #[cfg(feature = "scores")]
    scores: Arc<dyn crate::scores::ScoreStore>,
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
    /// Header-mapped secret start inputs per workflow (`--input-header`); empty by default.
    header_inputs: HashMap<String, HeaderBindings>,
    /// Keep run inputs in run records (`--store-inputs`).
    store_inputs: bool,
    /// End-user JWT verification of run starts (`--verify-jwt`).
    #[cfg(feature = "jwt")]
    user_auth: Option<Arc<crate::user_auth::UserAuth>>,
    /// Host services of the served workflows, for judge workflows of online evaluators.
    #[cfg(feature = "online-eval")]
    services: Option<Arc<graphrt_core::Services>>,
    /// Online evaluation pool (`--evaluators`).
    #[cfg(feature = "online-eval")]
    online: Option<Arc<crate::online_eval::OnlineEval>>,
    /// Score uploads to Langfuse (`--langfuse-scores`).
    #[cfg(feature = "langfuse")]
    langfuse: Option<Arc<crate::langfuse::LangfuseUploader>>,
    /// The gateway this agent trusts (`--trust-gateway`): `/internal/v1`.
    gateway_agent: Option<Arc<crate::gateway_agent::GatewayAgent>>,
}

/// The verified end user of a request (`sub` of its JWT), set by `--verify-jwt`.
#[derive(Clone, Debug)]
pub struct AuthenticatedUser(pub String);

impl AppState {
    /// Create state from compiled engines.
    pub fn new(workflows: IndexMap<String, ServedWorkflow>, api_key: Option<String>) -> Self {
        let memory = Arc::new(MemoryStore::new());
        Self {
            workflows,
            runs: Mutex::new(HashMap::new()),
            max_active_runs: 0,
            draining: AtomicBool::new(false),
            store: memory.clone(),
            #[cfg(feature = "scores")]
            scores: memory,
            api_key,
            next_id: AtomicU64::new(1),
            extensions: Extensions::builtin(),
            telemetry: None,
            #[cfg(feature = "rag")]
            knowledge: None,
            webhooks: crate::webhooks::Webhooks::default(),
            header_inputs: HashMap::new(),
            store_inputs: false,
            #[cfg(feature = "jwt")]
            user_auth: None,
            #[cfg(feature = "online-eval")]
            services: None,
            #[cfg(feature = "online-eval")]
            online: None,
            #[cfg(feature = "langfuse")]
            langfuse: None,
            gateway_agent: None,
        }
    }

    /// Trust a gateway (`--trust-gateway`): serve the signed `/internal/v1` API.
    pub fn with_gateway_agent(mut self, agent: Option<Arc<crate::gateway_agent::GatewayAgent>>) -> Self {
        self.gateway_agent = agent;
        self
    }

    /// The gateway agent state, with `--trust-gateway`.
    pub(crate) fn gateway_agent(&self) -> Option<&Arc<crate::gateway_agent::GatewayAgent>> {
        self.gateway_agent.as_ref()
    }

    /// Served workflows by name.
    pub(crate) fn served_workflows(&self) -> impl Iterator<Item = (&String, &ServedWorkflow)> {
        self.workflows.iter()
    }

    /// The workflow served as `name`.
    pub(crate) fn served_workflow(&self, name: &str) -> Option<&ServedWorkflow> {
        self.workflows.get(name)
    }

    /// `--max-active-runs` (0 = unlimited).
    pub(crate) fn max_active_runs(&self) -> usize {
        self.max_active_runs
    }

    /// Plugins loaded at startup (their hosts stay up for the server's lifetime).
    pub(crate) fn warm_plugins(&self) -> Vec<String> {
        self.extensions.loaded().map(|plugin| plugin.name.clone()).collect()
    }

    /// The `429` to answer when `--max-active-runs` runs are executing.
    pub(crate) fn busy_response(&self) -> Option<Response> {
        self.refuse_if_busy()
    }

    /// The executing run `run_id`, if any.
    pub(crate) fn active_run(&self, run_id: &str) -> Option<Arc<ActiveRun>> {
        self.active(run_id)
    }

    /// Upload every stored score to Langfuse (`--langfuse-scores`). Call before
    /// [`AppState::with_evaluators`], whose pool reports to the sinks set then.
    #[cfg(feature = "langfuse")]
    pub fn with_langfuse(mut self, uploader: Option<crate::langfuse::LangfuseUploader>) -> Self {
        self.langfuse = uploader.map(Arc::new);
        self
    }

    /// Score a sample of finished runs with the evaluators of this config
    /// (`--evaluators`). Judge workflows compile now, on the served workflows'
    /// services. Call after [`AppState::with_stores`] and
    /// [`AppState::with_telemetry`]: the pool reports to the stores and sinks
    /// set at this point.
    ///
    /// @throws CliError (usage) - When the config or a judge workflow is invalid.
    #[cfg(feature = "online-eval")]
    pub fn with_evaluators(mut self, path: &Path, options: &ServiceOptions, limits: Limits) -> Result<Self, CliError> {
        use graphrt_eval::online::{OnlineConfig, OnlineEvaluators};
        let config = OnlineConfig::load(path).map_err(crate::eval::eval_error)?;
        let served: Vec<&str> = self.workflows.keys().map(String::as_str).collect();
        for pattern in config.unmatched_workflows(&served) {
            eprintln!("warning: evaluators: workflow pattern `{pattern}` matches no served workflow");
        }
        let services = match &self.services {
            Some(services) => Arc::clone(services),
            None => Arc::new(graphrt_core::Services::new()),
        };
        let judges = crate::eval::WorkflowJudges {
            registry: self.extensions.registry.clone(),
            services,
            model: None,
            limits,
            options: options.clone(),
        };
        let mut registry = graphrt_eval::AssertionRegistry::with_builtins();
        graphrt_eval::judge::register(&mut registry, config.base_dir(), judges);
        let evaluators = OnlineEvaluators::new(&config, registry).map_err(crate::eval::eval_error)?;
        eprintln!(
            "  online evaluation: {} evaluator(s) from {} (concurrency {}, queue {})",
            config.evaluators.len(),
            path.display(),
            config.concurrency,
            config.queue
        );
        let pool =
            crate::online_eval::OnlineEval::start(&config, evaluators, Arc::clone(&self.scores), self.score_sinks());
        self.online = Some(pool);
        Ok(self)
    }

    /// The online evaluation pool, when `--evaluators` is set.
    #[cfg(feature = "online-eval")]
    pub fn online_eval(&self) -> Option<&Arc<crate::online_eval::OnlineEval>> {
        self.online.as_ref()
    }

    /// Let a verified end-user JWT authorize run starts (`--verify-jwt`).
    #[cfg(feature = "jwt")]
    pub fn with_user_auth(mut self, user_auth: Option<crate::user_auth::UserAuth>) -> Self {
        self.user_auth = user_auth.map(Arc::new);
        self
    }

    /// Map request headers to secret start inputs of the run endpoints (`--input-header`).
    ///
    /// @throws CliError (usage) - When a served workflow declares a mapped input with a
    ///   type other than `secret`.
    pub fn with_header_inputs(mut self, specs: &[HeaderInputSpec]) -> Result<Self, CliError> {
        let mut header_inputs = HashMap::new();
        for (name, workflow) in &self.workflows {
            if let Some(bindings) =
                HeaderBindings::resolve(specs, name, workflow.engine.graph()).map_err(CliError::usage)?
            {
                header_inputs.insert(name.clone(), bindings);
            }
        }
        self.header_inputs = header_inputs;
        Ok(self)
    }

    /// Keep each run's inputs (secrets masked) in its run record.
    pub fn with_store_inputs(mut self, store_inputs: bool) -> Self {
        self.store_inputs = store_inputs;
        self
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

    /// Whether workflow `name` maps request header input `input` (`--input-header`).
    #[cfg(feature = "jwt")]
    pub(crate) fn takes_header_input(&self, name: &str, input: &str) -> bool {
        self.header_inputs.get(name).is_some_and(|bindings| bindings.maps(input))
    }

    pub(crate) fn webhooks(&self) -> &crate::webhooks::Webhooks {
        &self.webhooks
    }

    /// Authorize a request a user may make about their own runs (feedback): a
    /// valid end-user JWT (with `--verify-jwt`; returns its subject for the
    /// handler's ownership check), else the API key (returns `None`).
    ///
    /// @throws Response - `401` for an invalid token or key, `429` over the user's rate limit.
    #[cfg(feature = "scores")]
    pub(crate) async fn authorize_user_or_key(
        &self,
        headers: &axum::http::HeaderMap,
    ) -> Result<Option<String>, Response> {
        #[cfg(feature = "jwt")]
        if let Some(user_auth) = &self.user_auth
            && let crate::user_auth::Admission::User(user) = user_auth.admit(headers).await?
        {
            return Ok(Some(user.subject));
        }
        if self.authorized(headers) {
            Ok(None)
        } else {
            Err(error_response(StatusCode::UNAUTHORIZED, "missing or invalid bearer token"))
        }
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

    /// What a request receives of its run: everything, unless it came with a
    /// verified user token and `--user-events` is `answer` (the default).
    fn event_scope(&self, user_token: bool) -> EventScope {
        #[cfg(feature = "jwt")]
        if user_token && self.user_auth.as_ref().is_some_and(|auth| auth.events() == crate::args::UserEvents::Answer) {
            return EventScope::Answer;
        }
        let _ = user_token;
        EventScope::Full
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
    ///
    /// While the server drains for shutdown, every new or resumed run gets `503`
    /// with `Retry-After` instead (see [`crate::drain`]).
    fn refuse_if_busy(&self) -> Option<Response> {
        if self.is_draining() {
            let mut refused = error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "the server is shutting down and accepts no new runs; retry shortly",
            );
            refused.headers_mut().insert(header::RETRY_AFTER, axum::http::HeaderValue::from_static(DRAIN_RETRY_AFTER));
            return Some(refused);
        }
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

    /// Only the run store: scores stay in the previous store, so prefer
    /// [`AppState::with_stores`] when serving scores.
    pub fn with_store(mut self, store: Arc<dyn RunStore>) -> Self {
        self.store = store;
        self
    }

    /// Record runs, and their scores, in these stores.
    pub fn with_stores(mut self, stores: crate::store::Stores) -> Self {
        self.store = stores.runs;
        #[cfg(feature = "scores")]
        {
            self.scores = stores.scores;
        }
        self
    }

    /// The score store.
    #[cfg(feature = "scores")]
    pub fn score_store(&self) -> &Arc<dyn crate::scores::ScoreStore> {
        &self.scores
    }

    /// Where scores are reported besides the store (spans, metrics, uploads).
    #[cfg(feature = "scores")]
    pub fn score_sinks(&self) -> crate::scores::ScoreSinks {
        crate::scores::ScoreSinks {
            #[cfg(feature = "otel")]
            telemetry: self.telemetry.clone(),
            #[cfg(feature = "langfuse")]
            langfuse: self.langfuse.clone(),
        }
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

    /// The runs currently executing.
    pub fn active_list(&self) -> Vec<Arc<ActiveRun>> {
        self.runs.lock().map(|runs| runs.values().cloned().collect()).unwrap_or_default()
    }

    /// Stop accepting runs (`503` + `Retry-After`) and report not ready on `/readyz`.
    pub fn begin_drain(&self) {
        self.draining.store(true, Ordering::Release);
    }

    /// Whether [`AppState::begin_drain`] was called.
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Acquire)
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
    #[allow(unused_mut)]
    let mut state = AppState::new(workflows, api_key).with_extensions(extensions);
    #[cfg(feature = "online-eval")]
    {
        state.services = Some(Arc::clone(&services));
    }
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

/// `Retry-After` (seconds) of runs refused while draining: long enough for a
/// load balancer to route the retry to another replica, or for a restart.
const DRAIN_RETRY_AFTER: &str = "5";

/// The HTTP router.
pub fn router(state: Arc<AppState>, max_body_bytes: usize) -> Router {
    router_with_uploads(state, max_body_bytes, DEFAULT_MAX_UPLOAD_BYTES)
}

/// The HTTP router, with a separate body limit for knowledge base file uploads.
pub fn router_with_uploads(state: Arc<AppState>, max_body_bytes: usize, max_upload_bytes: usize) -> Router {
    let api = Router::new()
        .route("/v1/workflows", get(list_workflows))
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
    #[cfg(feature = "scores")]
    let api = api.route("/v1/runs/{run_id}/scores", get(crate::scores::get_scores));
    let api = api
        .route("/v1/hooks", get(crate::webhooks::list_hooks))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));
    // Run starts accept a verified end-user JWT instead of the key (`--verify-jwt`).
    let start = Router::new()
        .route("/v1/workflows/{name}/runs", post(start_run))
        .layer(middleware::from_fn_with_state(state.clone(), authorize_start));
    // Webhook deliveries authenticate themselves (signature, else the bearer key).
    let hooks = Router::new().route("/v1/hooks/{hook}", post(crate::webhooks::receive));
    let counted = api.merge(start).merge(hooks);
    // Feedback authorizes itself: a verified end-user JWT for the user's own runs, else the key.
    #[cfg(feature = "scores")]
    let counted = counted.route("/v1/runs/{run_id}/feedback", post(crate::scores::post_feedback));
    // Opt-in usage telemetry counts API requests (in memory); health checks are not counted.
    #[cfg(feature = "telemetry")]
    let counted = counted.layer(middleware::from_fn(crate::telemetry::count_requests));
    // Gateway-signed routes authenticate themselves (HMAC, `--trust-gateway`).
    let counted = if state.gateway_agent.is_some() {
        counted.merge(crate::gateway_agent::router(state.clone()))
    } else {
        counted
    };
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readiness))
        .merge(counted)
        .layer(DefaultBodyLimit::max(max_body_bytes))
        // Outermost: early rejections leave the body unread; drain it so keep-alive clients can reuse the connection.
        .layer(middleware::from_fn_with_state(
            crate::body_drain::DrainLimit(max_body_bytes),
            crate::body_drain::drain_unread_body,
        ))
        .with_state(state)
}

/// `GET /readyz`: `200 ready`, or `503 draining` once a shutdown drain began, so
/// a load balancer stops routing new runs here. `/healthz` stays `200` (liveness).
async fn readiness(State(state): State<Arc<AppState>>) -> Response {
    if state.is_draining() {
        (StatusCode::SERVICE_UNAVAILABLE, "draining").into_response()
    } else {
        "ready".into_response()
    }
}

/// Execute `graphrt serve`.
pub async fn serve(args: ServeArgs) -> Result<u8, CliError> {
    let mut options = service_options(&args.services)?;
    let files = discover(&args.path)?;
    let lookup_env = |name: &str| {
        std::env::var(name).ok().or_else(|| options.env_file.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()))
    };
    let api_key = lookup_env(&args.api_key_env).filter(|k| !k.is_empty());
    #[cfg(feature = "jwt")]
    crate::user_auth::require_api_key(args.user_auth.verify_jwt.is_some(), api_key.is_some(), &args.api_key_env)?;
    #[cfg(feature = "jwt")]
    let user_auth = crate::user_auth::UserAuth::from_args(&args.user_auth, &args.input_header, lookup_env).await?;
    let limits = setup::limits(args.services.timeout_ms, args.services.max_steps);
    let telemetry = crate::otel::Telemetry::from_args(&args.services, &options.all_vars())?;
    options.capture_content = crate::otel::captures_content(telemetry.as_ref());
    let stores = crate::store::open_stores(args.store.as_ref(), args.max_runs)?;
    let webhooks = match &args.webhooks {
        Some(path) => crate::webhooks::Webhooks::load(path, &options.all_vars(), options.allow_private_network)?,
        None => crate::webhooks::Webhooks::default(),
    };
    #[cfg(feature = "jwt")]
    let user_auth_line = user_auth.as_ref().map(crate::user_auth::UserAuth::describe);
    #[cfg(feature = "online-eval")]
    let judge_limits = limits.clone();
    let gateway = crate::gateway_agent::GatewayAgentConfig::from_args(&args.gateway, lookup_env, args.max_body_bytes)?;
    #[cfg(feature = "jwt")]
    if gateway.is_some() && user_auth_line.is_some() {
        return Err(CliError::usage(
            "--trust-gateway and --verify-jwt are exclusive: behind a gateway, the gateway verifies end-user tokens",
        ));
    }
    let state = build_state(&files, &options, limits, api_key.clone())
        .await?
        .with_stores(stores)
        .with_telemetry(telemetry.clone())
        .with_store_inputs(args.store_inputs)
        .with_max_active_runs(args.max_active_runs)
        .with_webhooks(webhooks)?
        .with_header_inputs(&args.input_header)?
        .with_gateway_agent(gateway.map(|config| Arc::new(crate::gateway_agent::GatewayAgent::new(config))));
    #[cfg(feature = "jwt")]
    let state = state.with_user_auth(user_auth);
    #[cfg(feature = "langfuse")]
    let state = if args.langfuse_scores {
        let uploader = crate::langfuse::LangfuseUploader::start(&options.all_vars(), options.allow_private_network)?;
        eprintln!("  scores uploaded to {}", uploader.url());
        if telemetry.is_none() {
            eprintln!(
                "warning: --langfuse-scores without OpenTelemetry: runs have no trace id, so no score is uploaded \
                 (add --otel-preset langfuse)"
            );
        }
        state.with_langfuse(Some(uploader))
    } else {
        state
    };
    #[cfg(feature = "online-eval")]
    let state = match &args.evaluators {
        Some(path) => state.with_evaluators(path, &options, judge_limits)?,
        None => state,
    };
    let state = Arc::new(state);
    let interrupted = state.recover().await?;
    if interrupted > 0 {
        eprintln!("marked {interrupted} run(s) interrupted by the previous shutdown as failed");
    }
    if let Some(days) = args.store_ttl_days.filter(|d| *d > 0) {
        spawn_retention(Arc::clone(state.store()), days);
    }
    let app = router_with_uploads(state.clone(), args.max_body_bytes, args.max_upload_bytes);

    // Before listening: from here on SIGTERM / SIGINT start a drain (as PID 1 of a
    // container, an unhandled SIGTERM would be ignored until the SIGKILL).
    let mut signals = crate::drain::ShutdownSignals::install()?;
    let listener = crate::listen::Listener::bind(&args.addr).await?;
    let loopback = listener.is_local();
    // Without an API key, a loopback server is reachable from any web page through DNS
    // rebinding (the page's own host name resolving to 127.0.0.1). Such requests carry
    // that foreign host name, so only local `Host` values are accepted.
    let app = if api_key.is_none() && loopback && listener.is_browser_reachable() {
        app.layer(axum::middleware::from_fn(require_local_host))
    } else {
        app
    };
    let shown_addr = if crate::listen::unix_path(&args.addr).is_some() {
        args.addr.clone()
    } else {
        format!("http://{}", args.addr)
    };
    eprintln!("graphrt serving {} workflow(s) on {shown_addr}", state.workflows.len());
    if let Some(agent) = state.gateway_agent() {
        eprintln!("  trusting the gateway of deployment `{}` (signed /internal/v1)", agent.deployment());
    }
    #[cfg(feature = "jwt")]
    if let Some(line) = &user_auth_line {
        eprintln!("  {line}");
    }
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
    let (close_tx, close_rx) = tokio::sync::oneshot::channel::<()>();
    let mut server = tokio::spawn(listener.serve(app, async {
        let _closed = close_rx.await;
    }));
    if !args.no_resume_drained {
        // After listening, so a store scan never delays the first health check.
        tokio::spawn(resume_drained_runs(Arc::clone(&state)));
    }
    let cause = match futures::future::select(&mut server, std::pin::pin!(signals.recv())).await {
        futures::future::Either::Left((ended, _)) => return server_ended(ended),
        futures::future::Either::Right((cause, _)) => cause,
    };
    let deadline = std::time::Instant::now() + args.drain_timeout;
    eprintln!(
        "received {cause}: draining (no new runs; active runs pause, deadline {:.1} s)",
        args.drain_timeout.as_secs_f64()
    );
    let report = crate::drain::drain(&state, args.drain_timeout, cause, Some(&mut signals)).await;
    eprintln!("{report}");
    if report.unfinished > 0 {
        eprintln!(
            "warning: {} run(s) did not stop in time; the next start marks them failed (raise --drain-timeout)",
            report.unfinished
        );
    }
    // Connections still open (idle keep-alives, finished streams) get the rest of
    // the deadline, at least a moment, to close.
    let _closing = close_tx.send(());
    let rest = deadline.saturating_duration_since(std::time::Instant::now()).max(std::time::Duration::from_millis(100));
    match tokio::time::timeout(rest, &mut server).await {
        Ok(ended) => {
            server_ended(ended)?;
        }
        Err(_) => {
            server.abort();
            eprintln!("warning: closing the remaining connections at the drain deadline");
        }
    }
    if let Some(telemetry) = &telemetry {
        telemetry.shutdown().await;
    }
    Ok(EXIT_OK)
}

/// The result of the HTTP server task.
fn server_ended(ended: Result<std::io::Result<()>, tokio::task::JoinError>) -> Result<u8, CliError> {
    match ended {
        Ok(Ok(())) => Ok(EXIT_OK),
        Ok(Err(e)) => Err(CliError::failed(format!("server error: {e}"))),
        Err(e) => Err(CliError::failed(format!("server task failed: {e}"))),
    }
}

/// At startup: resume the async and webhook runs a previous shutdown drained
/// ([`crate::drain::is_auto_resumable`]) in the background. Runs whose workflow
/// changed, or that need secrets only their caller has, stay paused for a
/// resume over the API. Returns how many were resumed.
pub async fn resume_drained_runs(state: Arc<AppState>) -> usize {
    let mut resumed = 0;
    let mut cursor = None;
    loop {
        let filter = RunFilter { status: Some(RunStatus::Paused), limit: 100, cursor, ..RunFilter::default() };
        let page = match state.store.list(filter).await {
            Ok(page) => page,
            Err(e) => {
                eprintln!("warning: cannot list drained runs to resume: {e}");
                return resumed;
            }
        };
        for record in page.runs.iter().filter(|r| crate::drain::is_auto_resumable(r)) {
            if state.refuse_if_busy().is_some() {
                eprintln!("warning: not resuming further drained runs: the server is busy or draining");
                return resumed;
            }
            match resume_drained(&state, &record.run_id).await {
                Ok(()) => resumed += 1,
                Err(why) => eprintln!("run {}: drained run left paused: {why}", record.run_id),
            }
        }
        cursor = match page.next_cursor {
            Some(next) => Some(next),
            None => break,
        };
    }
    if resumed > 0 {
        eprintln!("resumed {resumed} run(s) drained by the previous shutdown");
    }
    resumed
}

/// Claim and resume one drained run as a detached run.
async fn resume_drained(state: &Arc<AppState>, run_id: &str) -> Result<(), String> {
    let record = state.store.claim_resume(run_id, now_ms()).await.map_err(|e| e.to_string())?;
    let snapshot = match state.store.load_snapshot(run_id).await {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) | Err(_) => {
            release_claim(state, record).await;
            return Err("no stored snapshot".to_owned());
        }
    };
    let workflow =
        state.workflows.get(&record.workflow).filter(|w| w.engine.graph().fingerprint() == snapshot.graph_fingerprint);
    let Some(workflow) = workflow else {
        release_claim(state, record).await;
        return Err("its workflow is not served or changed since".to_owned());
    };
    if crate::drain::needs_caller_secrets(&snapshot) {
        release_claim(state, record).await;
        return Err("it needs secrets only its caller can supply; resume it over the API".to_owned());
    }
    let mut input = RunInput::new();
    input.run_id = Some(run_id.to_owned());
    input.env = workflow.env.clone();
    input.resume = Some(ResumeFrom::new(snapshot, IndexMap::new()));
    let active = launch(state, &workflow.engine, record, input, None);
    active.set_detached();
    Ok(())
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

/// Authorize `POST /v1/workflows/{name}/runs`: a valid end-user JWT (with
/// `--verify-jwt`), else the API key. A present but invalid token is refused.
async fn authorize_start(
    State(state): State<Arc<AppState>>,
    UrlPath(name): UrlPath<String>,
    request: Request,
    next: Next,
) -> Response {
    #[cfg(feature = "jwt")]
    if let Some(user_auth) = &state.user_auth {
        let mut request = request;
        match user_auth.admit(request.headers()).await {
            Err(refused) => return refused,
            // A user token starts only workflows built for end users: those that
            // take the verified token as an input.
            Ok(crate::user_auth::Admission::User(_)) if !state.takes_header_input(&name, user_auth.input()) => {
                return error_response(
                    StatusCode::FORBIDDEN,
                    &format!("workflow `{name}` does not accept user tokens (no `{}` input)", user_auth.input()),
                );
            }
            Ok(crate::user_auth::Admission::User(user)) => {
                request.extensions_mut().insert(AuthenticatedUser(user.subject));
                return next.run(request).await;
            }
            Ok(crate::user_auth::Admission::NoToken) => return require_auth(State(state), request, next).await,
        }
    }
    #[cfg(not(feature = "jwt"))]
    let _ = name;
    require_auth(State(state), request, next).await
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
    /// Caller labels, stored on the run record and stamped on its spans
    /// (`langfuse.trace.metadata.<key>`, `graphrt.run.metadata.<key>`).
    #[serde(default)]
    pub metadata: IndexMap<String, String>,
    /// Session / conversation id (`session.id`, `gen_ai.conversation.id`, `langfuse.session.id`).
    #[serde(default)]
    pub session_id: Option<String>,
    /// End-user id (`user.id`, `langfuse.user.id`).
    #[serde(default)]
    pub user_id: Option<String>,
    /// Tags (`langfuse.trace.tags`).
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Limits on the caller context of a run request.
const MAX_METADATA_ENTRIES: usize = 64;
const MAX_TAGS: usize = 32;
const MAX_LABEL_BYTES: usize = 256;
const MAX_METADATA_VALUE_BYTES: usize = 2048;

impl RunRequest {
    /// Validate and copy the caller context onto `record`.
    ///
    /// @throws String - A message for a `400` response when a field is too large.
    pub(crate) fn apply_context(&self, record: &mut RunRecord) -> Result<(), String> {
        if self.metadata.len() > MAX_METADATA_ENTRIES {
            return Err(format!("`metadata` has more than {MAX_METADATA_ENTRIES} entries"));
        }
        if self.tags.len() > MAX_TAGS {
            return Err(format!("`tags` has more than {MAX_TAGS} entries"));
        }
        let label_ok = |text: &str| !text.is_empty() && text.len() <= MAX_LABEL_BYTES;
        if let Some(key) = self.metadata.keys().find(|k| !label_ok(k)) {
            return Err(format!("`metadata` key `{key}` must be 1-{MAX_LABEL_BYTES} bytes"));
        }
        if self.metadata.values().any(|v| v.len() > MAX_METADATA_VALUE_BYTES) {
            return Err(format!("`metadata` values must be at most {MAX_METADATA_VALUE_BYTES} bytes"));
        }
        let ids = [("session_id", &self.session_id), ("user_id", &self.user_id)];
        if let Some((name, _)) = ids.iter().find(|(_, id)| id.as_deref().is_some_and(|id| !label_ok(id))) {
            return Err(format!("`{name}` must be 1-{MAX_LABEL_BYTES} bytes"));
        }
        if self.tags.iter().any(|t| !label_ok(t)) {
            return Err(format!("`tags` entries must be 1-{MAX_LABEL_BYTES} bytes"));
        }
        record.metadata.clone_from(&self.metadata);
        record.session_id.clone_from(&self.session_id);
        record.user_id.clone_from(&self.user_id);
        record.tags.clone_from(&self.tags);
        Ok(())
    }
}

/// Caller context stored on a run record, for its spans.
fn record_context(record: &RunRecord) -> crate::otel::RunContext {
    crate::otel::RunContext {
        session_id: record.session_id.clone(),
        user_id: record.user_id.clone(),
        tags: record.tags.clone(),
        metadata: record.metadata.clone(),
    }
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

/// How much of a run the caller receives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EventScope {
    /// Every engine event and the full result (API-key callers, `--user-events all`).
    Full,
    /// The answer stream, the final result and pause prompts (end users by default).
    #[cfg(feature = "jwt")]
    Answer,
}

/// How the caller receives a run: the delivery mode and the event scope.
#[derive(Clone, Copy, Debug)]
struct Reply {
    delivery: Delivery,
    scope: EventScope,
}

fn to_values(map: serde_json::Map<String, serde_json::Value>) -> IndexMap<String, Value> {
    map.into_iter().map(|(k, v)| (k, Value::from_json(v))).collect()
}

pub(crate) fn store_error(e: &StoreError) -> Response {
    let status = match e {
        StoreError::NotFound(_) => StatusCode::NOT_FOUND,
        StoreError::Conflict(_) | StoreError::Precondition(_) => StatusCode::CONFLICT,
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
    user: Option<axum::Extension<AuthenticatedUser>>,
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
    let mut record = RunRecord::running(&run_id, &name, workflow.engine.graph().fingerprint(), now_ms());
    // Validate the run context before the body's inputs are moved out of it.
    if let Err(message) = body.apply_context(&mut record) {
        return error_response(StatusCode::BAD_REQUEST, &message);
    }
    let mut inputs = to_values(body.inputs);
    if let Some(bindings) = state.header_inputs.get(&name)
        && let Err(refused) = bindings.apply_to_inputs(&headers, &mut inputs)
    {
        return refused.into_response();
    }
    let user_token = user.is_some();
    if let Some(axum::Extension(AuthenticatedUser(subject))) = &user {
        // A verified end user (`--verify-jwt`) is the run's user, whatever the body said.
        record.user_id = Some(subject.clone());
        record.user_verified = true;
    }
    if let Err(e) = state.store.create(record.clone()).await {
        return store_error(&e);
    }
    let mut input = RunInput::new();
    input.run_id = Some(run_id);
    input.inputs = inputs;
    input.sys = to_values(body.sys);
    if let Some(axum::Extension(AuthenticatedUser(subject))) = user {
        // The verified identity wins over anything the client put in `sys`.
        input.sys.insert("user_id".to_owned(), Value::String(subject.into()));
    }
    input.conversation = to_values(body.conversation);
    input.env = workflow.env.clone();
    let reply = Reply { delivery: Delivery::of(body.stream, body.run_async), scope: state.event_scope(user_token) };
    execute(&state, &workflow.engine, record, input, reply, traceparent(&headers)).await
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
                    release_claim(&state, record).await;
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
        .get_key_value(&record.workflow)
        .or_else(|| state.workflows.iter().find(|(_, w)| w.engine.graph().fingerprint() == snapshot.graph_fingerprint));
    let Some((name, workflow)) = workflow else {
        let message = format!("workflow `{}` is not served", record.workflow);
        // The claim above marked the run `running`; without a release it would stay so until a restart.
        release_claim(&state, record).await;
        return error_response(StatusCode::NOT_FOUND, &message);
    };
    let mut secrets = body.secrets;
    if let Some(bindings) = state.header_inputs.get(name)
        && let Err(refused) = bindings.apply_to_resume(&headers, &mut secrets)
    {
        release_claim(&state, record).await;
        return refused.into_response();
    }
    let mut input = RunInput::new();
    input.run_id = Some(run_id);
    input.env = workflow.env.clone();
    let mut resume = ResumeFrom::new(snapshot, to_values(body.inputs));
    for (selector, secret) in secrets {
        resume = resume.secret(selector, secret);
    }
    input.resume = Some(resume);
    // Resume takes the API key only, so its caller gets every event.
    let reply = Reply { delivery: Delivery::of(body.stream, body.run_async), scope: EventScope::Full };
    execute(&state, &workflow.engine, record, input, reply, traceparent(&headers)).await
}

/// Undo a resume claim so a refused resume leaves the run visible as paused. The
/// write is fenced by the claim's epoch, so it never undoes a later claim.
async fn release_claim(state: &AppState, mut record: RunRecord) {
    record.status = RunStatus::Paused;
    let run_id = record.run_id.clone();
    let epoch = record.epoch;
    if let Err(e) = state.store.put_if(record, epoch).await {
        eprintln!("warning: run {run_id}: cannot restore paused status: {e}");
    }
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
    headers: axum::http::HeaderMap,
) -> Response {
    // A reconnecting SSE client names the last event it saw.
    let from = query.from.max(crate::sse_ids::resume_index_of(&headers).unwrap_or(0));
    events_response(&state, &run_id, from, 0, query.stream == Some(false)).await
}

/// Stored events of `run_id` from index `from`, followed by the live tail while
/// the run executes, each with `id_base` + its index as SSE `id:` (or a JSON
/// array when `json`). Shared by `/v1/runs/{id}/events` and the gateway's reattach.
pub(crate) async fn events_response(state: &AppState, run_id: &str, from: u64, id_base: u64, json: bool) -> Response {
    let active = state.active(run_id);
    // Take the live subscription first: events the driver stores after this point
    // reach us through it, and the stored part is cut at `stored_before`.
    let subscription = active.as_ref().map(|run| (run.stored_before, run.subscribe()));
    let stored_until = subscription.as_ref().map(|(before, _)| *before);
    let mut stored = match state.store.events(run_id, from).await {
        Ok(events) => events,
        Err(e) => return store_error(&e),
    };
    if let Some(limit) = stored_until {
        let keep = usize::try_from(limit.saturating_sub(from)).unwrap_or(usize::MAX);
        stored.truncate(keep);
    }
    if json {
        let mut all = stored;
        if let Some((before, sub)) = subscription {
            let skip = usize::try_from(from.saturating_sub(before)).unwrap_or(usize::MAX);
            all.extend(sub.backlog.iter().skip(skip).map(|event| event_to_stored(event)));
        }
        return Json(all).into_response();
    }
    let replay = stored
        .into_iter()
        .zip(from..)
        .map(move |(json, index)| Ok::<_, Infallible>(stored_sse(&json, id_base + index)));
    let replay = futures::stream::iter(replay);
    match (subscription, active) {
        (Some((before, sub)), Some(run)) => {
            let live = numbered(live::follow(run, sub), before).filter_map(move |item| {
                futures::future::ready(match item {
                    Numbered::Event(event, index) => (index >= from).then(|| Ok(sse_event(&event, id_base + index))),
                    Numbered::Lagged(skipped) => Some(Ok(lagged_sse(skipped))),
                })
            });
            Sse::new(replay.chain(live)).keep_alive(KeepAlive::default()).into_response()
        }
        _ => Sse::new(replay.boxed()).into_response(),
    }
}

/// The `lagged` SSE event: `skipped` events are gone for this listener (it fell
/// further behind than the in-memory feed keeps). Fetch them with `?from=` later.
/// It carries no `id:`, so `Last-Event-ID` keeps naming the last real event.
fn lagged_sse(skipped: u64) -> SseEvent {
    let data = serde_json::json!({ "type": "lagged", "skipped": skipped });
    SseEvent::default().event("lagged").data(data.to_string())
}

/// A [`live::Followed`] item with the event's index in the run's log.
pub(crate) enum Numbered {
    Event(live::SharedEvent, u64),
    Lagged(u64),
}

/// Number the events of [`live::follow`], whose first event has index
/// `first_index`; events reported lagged keep their indices (a gap in the ids).
pub(crate) fn numbered(
    events: impl Stream<Item = live::Followed> + Send + 'static,
    first_index: u64,
) -> impl Stream<Item = Numbered> + Send + 'static {
    events.scan(first_index, |next, followed| {
        let item = match followed {
            live::Followed::Event(event) => {
                *next += 1;
                Numbered::Event(event, *next - 1)
            }
            live::Followed::Lagged(skipped) => {
                *next += skipped;
                Numbered::Lagged(skipped)
            }
        };
        futures::future::ready(Some(item))
    })
}

/// Aborts the run when a synchronous caller (blocking or streaming) goes away before
/// the run finished. Detached runs never get one.
struct AbortOnDrop(Arc<ActiveRun>);

impl StreamGuard for AbortOnDrop {}

/// Kept alive by a run's SSE response for as long as the client is connected.
pub(crate) trait StreamGuard: Send + 'static {
    /// The run's terminal event was handed to the response.
    fn terminal_sent(&self) {}
}

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
    reply: Reply,
    traceparent: Option<String>,
) -> Response {
    // The launched run's assembler records the answer in the store; a second one,
    // fed from the same events, produces `response` SSE events for a streaming caller.
    let stream_assembler = new_assembler(engine, &input);
    let run_id = record.run_id.clone();
    let active = launch(state, engine, record, input, traceparent.as_deref());
    match reply.delivery {
        Delivery::Detached => {
            active.set_detached();
            detached_response(&run_id)
        }
        Delivery::Stream => {
            // The subscription's backlog covers events emitted before subscribing.
            let subscription = active.subscribe();
            let events = live::follow(Arc::clone(&active), subscription);
            let first_index = active.stored_before;
            let guard = AbortOnDrop(active);
            let cursor = crate::sse_ids::Cursor::ALL;
            let events = sse_stream(events, stream_assembler, first_index, cursor, guard, reply.scope);
            Sse::new(events).keep_alive(KeepAlive::default()).into_response()
        }
        Delivery::Blocking => {
            let guard = AbortOnDrop(Arc::clone(&active));
            let outcome = active.wait().await;
            drop(guard);
            result_json(&run_id, outcome, reply.scope)
        }
    }
}

/// `202 Accepted` for an async run.
fn detached_response(run_id: &str) -> Response {
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "run_id": run_id,
            "status": "running",
            "run": format!("/v1/runs/{run_id}"),
            "events": format!("/v1/runs/{run_id}/events"),
        })),
    )
        .into_response()
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
    // The context lives on the record; building it is skipped when nothing traces.
    let tracer = match &state.telemetry {
        Some(telemetry) => telemetry.tracer(engine.graph(), traceparent, &record_context(&record)),
        None => crate::otel::RunTracer::default(),
    };
    let capture = live::Capture {
        start_node: state.store_inputs.then(|| root_start_node(engine.graph())).flatten(),
        #[cfg(feature = "online-eval")]
        online: state.online.as_ref().filter(|online| online.covers(&record.workflow)).cloned(),
    };
    let assembler = new_assembler(engine, &input);
    let run = engine.run(input);
    let weak = Arc::downgrade(state);
    let on_finish: live::OnFinish = Box::new(move |id| {
        if let Some(state) = weak.upgrade() {
            state.untrack(id);
        }
    });
    let active = live::spawn(run, assembler, record, Arc::clone(&state.store), on_finish, tracer, capture);
    state.track(Arc::clone(&active));
    #[cfg(feature = "telemetry")]
    crate::telemetry::count_run();
    active
}

impl AppState {
    /// Start driving a run in the background and track it as active (for the
    /// gateway protocol). `record` must already exist in the store.
    pub(crate) fn launch_run(
        self: &Arc<Self>,
        engine: &Engine,
        record: RunRecord,
        input: RunInput,
        traceparent: Option<&str>,
    ) -> Arc<ActiveRun> {
        launch(self, engine, record, input, traceparent)
    }
}

/// Id of the root graph's start node, whose outputs are the run's validated inputs.
fn root_start_node(graph: &Graph) -> Option<Arc<str>> {
    graph.nodes().iter().find(|n| n.parent.is_none() && n.kind.as_ref() == "start").map(|n| Arc::clone(&n.id))
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
    let active = launch(state, &workflow.engine, record, input, traceparent.as_deref());
    active.set_detached();
    Ok(Launch::Started(active))
}

/// The SSE response of a streaming run start or resume: response text and
/// engine events (from [`live::follow`], whose first event has index
/// `first_index`) with their `id:`s until the terminal one, skipping what
/// `cursor` excludes. The assembler sees every event, so the text after a
/// cursor is exactly what an uninterrupted stream would have sent. `guard`
/// lives as long as the stream.
pub(crate) fn sse_stream<G: StreamGuard>(
    events: impl Stream<Item = live::Followed> + Send + 'static,
    mut assembler: ResponseAssembler,
    first_index: u64,
    cursor: crate::sse_ids::Cursor,
    guard: G,
    scope: EventScope,
) -> impl Stream<Item = Result<SseEvent, Infallible>> + Send + 'static {
    // `follow` ends right after the terminal event and refills a lagging caller
    // from the run's feed, so the answer text stays complete.
    let mut chunks = Vec::new();
    numbered(events, first_index).flat_map(move |item| {
        let (event, index) = match item {
            Numbered::Event(event, index) => (event, index),
            Numbered::Lagged(skipped) => return futures::stream::iter(vec![Ok(lagged_sse(skipped))]),
        };
        let mut out = Vec::new();
        assembler.push_into(&event, &mut chunks);
        for (k, chunk) in chunks.drain(..).enumerate() {
            if !cursor.admits(index, Some(k)) {
                continue;
            }
            let data = serde_json::json!({ "node_id": chunk.node_id, "text": chunk.text, "is_final": chunk.is_final });
            let response = SseEvent::default().event("response").id(crate::sse_ids::chunk_id(index, k));
            out.push(Ok(response.data(data.to_string())));
        }
        if cursor.admits(index, None) {
            if let Some(message) = scoped_sse_event(&event, index, scope) {
                out.push(Ok(message));
            }
            if event.is_terminal() {
                guard.terminal_sent();
            }
        }
        futures::stream::iter(out)
    })
}

/// The SSE message for `event` (at `index`) within `scope`; `None` when the
/// caller does not see it.
fn scoped_sse_event(event: &Event, index: u64, scope: EventScope) -> Option<SseEvent> {
    match scope {
        EventScope::Full => Some(sse_event(event, index)),
        #[cfg(feature = "jwt")]
        EventScope::Answer => crate::user_auth::events::sse_event(event).map(|m| m.id(crate::sse_ids::event_id(index))),
    }
}

/// A live event with its index as `id:`; unlike stored ones, `run_paused` keeps
/// its snapshot here.
pub(crate) fn sse_event(event: &Event, index: u64) -> SseEvent {
    let text = serde_json::to_string(event).unwrap_or_else(|_| "null".to_owned());
    match serde_json::value::RawValue::from_string(text) {
        Ok(json) => stored_sse(&json, index),
        Err(_) => SseEvent::default().event("event").id(crate::sse_ids::event_id(index)).data("null"),
    }
}

/// One SSE message per event, named after the event's `type`, with its index as `id:`.
fn stored_sse(event: &StoredEvent, index: u64) -> SseEvent {
    let name = stored_event_type(event).unwrap_or("event");
    SseEvent::default().event(name).id(crate::sse_ids::event_id(index)).data(event.get())
}

/// The JSON body returned to blocking callers.
fn result_json(run_id: &str, outcome: live::Outcome, scope: EventScope) -> Response {
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
    match scope {
        EventScope::Full => Json(body).into_response(),
        #[cfg(feature = "jwt")]
        EventScope::Answer => Json(crate::user_auth::events::result(body)).into_response(),
    }
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
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use std::sync::Arc;

    use axum::response::IntoResponse;
    use axum::response::sse::Sse;
    use graphrt_core::engine::{Limits, RunInput};
    use graphrt_core::store::RunRecord;

    use super::{constant_time_eq, is_local_host};
    use crate::store::live;

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

    /// Property: a stream started at any cursor (after any engine event, or in the
    /// middle of an event's response text) sends exactly the rest of what an
    /// uninterrupted stream sends: same ids, same names, same response text.
    #[tokio::test]
    async fn a_stream_from_any_cursor_is_the_exact_rest_of_the_full_stream() {
        use http_body_util::BodyExt;
        let dir = std::env::temp_dir().join(format!("graphrt-cursor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("chat.yaml"),
            "version: \"1\"\nname: chat\ngraph:\n  nodes:\n    - { id: start, type: start, variables: [] }\n\
             \x20   - { id: a, type: answer, answer: \"one \" }\n    - { id: b, type: answer, answer: \"two\" }\n\
             \x20 edges:\n    - { source: start, target: a }\n    - { source: a, target: b }\n",
        )
        .unwrap();
        let files = super::discover(&dir).unwrap();
        let options = crate::setup::ServiceOptions {
            plugins: crate::extensions::PluginOptions { dirs: Vec::new(), no_default: true, trusted: Vec::new() },
            ..crate::setup::ServiceOptions::default()
        };
        let state = std::sync::Arc::new(super::build_state(&files, &options, Limits::default(), None).await.unwrap());
        // (id, event name, data unless it holds timings).
        let stream = |cursor: crate::sse_ids::Cursor| {
            let state = std::sync::Arc::clone(&state);
            async move {
                let workflow = state.served_workflow("chat").unwrap();
                let record = RunRecord::running(state.new_run_id(), "chat", workflow.engine.graph().fingerprint(), 1);
                state.store.create(record.clone()).await.unwrap();
                let input = RunInput::new();
                let assembler = super::new_assembler(&workflow.engine, &input);
                let active = super::launch(&state, &workflow.engine, record, input, None);
                let events = live::follow(Arc::clone(&active), active.subscribe());
                let sse = Sse::new(super::sse_stream(
                    events,
                    assembler,
                    7,
                    cursor,
                    super::AbortOnDrop(active),
                    super::EventScope::Full,
                ));
                let body = sse.into_response().into_body().collect().await.unwrap().to_bytes();
                let text = String::from_utf8(body.to_vec()).unwrap();
                text.split("\n\n")
                    .filter(|block| block.contains("event:"))
                    .map(|block| {
                        let field = |name: &str| {
                            block
                                .lines()
                                .find_map(|l| l.strip_prefix(name))
                                .map(|v| v.trim().to_owned())
                                .unwrap_or_default()
                        };
                        let data = if field("event:") == "response" { field("data:") } else { String::new() };
                        (field("id:"), field("event:"), data)
                    })
                    .collect::<Vec<_>>()
            }
        };
        let full = stream(crate::sse_ids::Cursor::ALL).await;
        assert!(full.iter().any(|(id, ..)| id.contains('.')), "the stream has response text: {full:?}");
        assert_eq!(full.first().map(|(id, ..)| id.as_str()), Some("7"), "ids start at the first index");
        for (at, (last, ..)) in full.iter().enumerate() {
            let rest = stream(crate::sse_ids::Cursor::after(last).unwrap()).await;
            assert_eq!(rest, full[at + 1..], "after {last}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
