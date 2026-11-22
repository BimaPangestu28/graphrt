//! [`AppState`]: the served workflows, the run stores, the runs executing now
//! and the optional services (auth, telemetry, knowledge, evaluation) every
//! handler reads, with the builder methods `graphrt serve` and embedders use.

use std::collections::HashMap;
#[cfg(feature = "online-eval")]
use std::path::Path;
#[cfg(feature = "rag")]
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
#[cfg(feature = "online-eval")]
use graphrt_core::engine::Limits;
use graphrt_core::store::{RunStore, now_ms};
use graphrt_core::{Engine, Value};
use indexmap::IndexMap;

use super::delivery::EventScope;
use super::errors::error_response;
use crate::error::CliError;
use crate::extensions::Extensions;
use crate::header_inputs::{HeaderBindings, HeaderInputSpec};
#[cfg(feature = "online-eval")]
use crate::setup::ServiceOptions;
use crate::store::MemoryStore;
use crate::store::live::ActiveRun;

/// Default cap on runs executing at once (`--max-active-runs`).
pub const DEFAULT_MAX_ACTIVE_RUNS: usize = 256;

/// `Retry-After` (seconds) of runs refused while draining: long enough for a
/// load balancer to route the retry to another replica, or for a restart.
const DRAIN_RETRY_AFTER: &str = "5";

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
    pub(super) workflows: IndexMap<String, ServedWorkflow>,
    pub(super) runs: Mutex<HashMap<String, Arc<ActiveRun>>>,
    /// Cap on `runs` (executing runs); `0` = unlimited.
    pub(super) max_active_runs: usize,
    /// Set by a shutdown drain: new and resumed runs are refused with `503`.
    pub(super) draining: AtomicBool,
    pub(super) store: Arc<dyn RunStore>,
    /// Evaluation scores, kept by the same backend as `store`.
    #[cfg(feature = "scores")]
    pub(super) scores: Arc<dyn crate::scores::ScoreStore>,
    pub(super) api_key: Option<String>,
    pub(super) next_id: AtomicU64,
    /// Node kinds (built-in and plugins) used for `/v1/nodes` and `/v1/validate`.
    pub(super) extensions: Extensions,
    /// OpenTelemetry export, when enabled.
    pub(super) telemetry: Option<Arc<crate::otel::Telemetry>>,
    /// The knowledge base behind `/v1/knowledge`, when served with `--knowledge`.
    #[cfg(feature = "rag")]
    pub(super) knowledge: Option<crate::knowledge_api::KnowledgeService>,
    /// Webhook triggers under `/v1/hooks` (`--webhooks`).
    pub(super) webhooks: crate::webhooks::Webhooks,
    /// Header-mapped secret start inputs per workflow (`--input-header`); empty by default.
    pub(super) header_inputs: HashMap<String, HeaderBindings>,
    /// Keep run inputs in run records (`--store-inputs`).
    pub(super) store_inputs: bool,
    /// End-user JWT verification of run starts (`--verify-jwt`).
    #[cfg(feature = "jwt")]
    pub(super) user_auth: Option<Arc<crate::user_auth::UserAuth>>,
    /// Host services of the served workflows, for judge workflows of online evaluators.
    #[cfg(feature = "online-eval")]
    pub(super) services: Option<Arc<graphrt_core::Services>>,
    /// Online evaluation pool (`--evaluators`).
    #[cfg(feature = "online-eval")]
    pub(super) online: Option<Arc<crate::online_eval::OnlineEval>>,
    /// Score uploads to Langfuse (`--langfuse-scores`).
    #[cfg(feature = "langfuse")]
    pub(super) langfuse: Option<Arc<crate::langfuse::LangfuseUploader>>,
    /// The gateway this agent trusts (`--trust-gateway`): `/internal/v1`.
    #[cfg(feature = "trust-gateway")]
    pub(super) gateway_agent: Option<Arc<crate::gateway_agent::GatewayAgent>>,
}

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
            #[cfg(feature = "trust-gateway")]
            gateway_agent: None,
        }
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
        let config = graphrt_eval::online::OnlineConfig::load(path).map_err(crate::eval::eval_error)?;
        let served: Vec<&str> = self.workflows.keys().map(String::as_str).collect();
        for pattern in config.unmatched_workflows(&served) {
            eprintln!("warning: evaluators: workflow pattern `{pattern}` matches no served workflow");
        }
        let evaluators = self.online_evaluators(&config, options, limits)?;
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

    /// The evaluators of `config`, with judge workflows compiled on the served
    /// workflows' node kinds and services.
    ///
    /// @throws CliError (usage) - When an evaluator or a judge workflow is invalid.
    #[cfg(feature = "online-eval")]
    fn online_evaluators(
        &self,
        config: &graphrt_eval::online::OnlineConfig,
        options: &ServiceOptions,
        limits: Limits,
    ) -> Result<graphrt_eval::online::OnlineEvaluators, CliError> {
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
        graphrt_eval::online::OnlineEvaluators::new(config, registry).map_err(crate::eval::eval_error)
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
    pub(super) fn takes_header_input(&self, name: &str, input: &str) -> bool {
        self.header_inputs.get(name).is_some_and(|bindings| bindings.maps(input))
    }

    pub(crate) fn webhooks(&self) -> &crate::webhooks::Webhooks {
        &self.webhooks
    }

    /// What a request receives of its run: everything, unless it came with a
    /// verified user token and `--user-events` is `answer` (the default).
    pub(super) fn event_scope(&self, user_token: bool) -> EventScope {
        #[cfg(feature = "jwt")]
        if user_token && self.user_auth.as_ref().is_some_and(|auth| auth.events() == crate::args::UserEvents::Answer) {
            return EventScope::Answer;
        }
        let _ = user_token;
        EventScope::Full
    }

    /// Serve this knowledge base under `/v1/knowledge`. `config` is the
    /// knowledge config path; datasets created over the API are saved next to it.
    #[cfg(feature = "rag")]
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
    pub(super) fn refuse_if_busy(&self) -> Option<Response> {
        if self.is_draining() {
            return Some(draining_response());
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

    /// Record runs in this store (default: in memory).
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

    pub(super) fn new_run_id(&self) -> String {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        format!("run-{nanos:x}-{n:x}")
    }

    pub(super) fn track(&self, run: Arc<ActiveRun>) {
        if let Ok(mut runs) = self.runs.lock() {
            runs.insert(run.run_id.clone(), run);
        }
    }

    pub(super) fn untrack(&self, run_id: &str) {
        if let Ok(mut runs) = self.runs.lock() {
            runs.remove(run_id);
        }
    }

    pub(super) fn active(&self, run_id: &str) -> Option<Arc<ActiveRun>> {
        self.runs.lock().ok().and_then(|runs| runs.get(run_id).cloned())
    }

    /// The served workflow (and its name) whose compiled graph has `fingerprint`:
    /// the one a snapshot was taken from.
    pub(super) fn workflow_with_fingerprint(&self, fingerprint: &str) -> Option<(&String, &ServedWorkflow)> {
        self.workflows.iter().find(|(_, workflow)| workflow.engine.graph().fingerprint() == fingerprint)
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

/// The `503` of a run refused during a shutdown drain; `Retry-After` lets the
/// client (or a load balancer) retry on another replica or after the restart.
fn draining_response() -> Response {
    let mut refused = error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "the server is shutting down and accepts no new runs; retry shortly",
    );
    refused.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static(DRAIN_RETRY_AFTER));
    refused
}

/// What the gateway protocol (`--trust-gateway`, [`crate::gateway_agent`])
/// reads of the server state, outside the `serve` module.
#[cfg(feature = "trust-gateway")]
impl AppState {
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

    /// The `429` to answer when `--max-active-runs` runs are executing (or the
    /// `503` while draining).
    pub(crate) fn busy_response(&self) -> Option<Response> {
        self.refuse_if_busy()
    }

    /// The executing run `run_id`, if any.
    pub(crate) fn active_run(&self, run_id: &str) -> Option<Arc<ActiveRun>> {
        self.active(run_id)
    }
}
