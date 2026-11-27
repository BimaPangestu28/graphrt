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
use crate::store::timed::{TimedStore, WriteLatency};

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
    /// Stop requests of the gateway (`POST /internal/v1/stop`), handled like `SIGTERM`.
    pub(super) stop: Arc<tokio::sync::Notify>,
    /// The run store, wrapped in a [`TimedStore`] recording into `store_writes`.
    pub(super) store: Arc<dyn RunStore>,
    /// Recent write latency of `store` (the drain sizes its reserve with it).
    pub(super) store_writes: Arc<WriteLatency>,
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
    /// Whether users may resume their own paused runs (`--user-resume`).
    #[cfg(feature = "jwt")]
    pub(super) user_resume: bool,
    /// Conversations kept by the server (`--conversations`).
    #[cfg(feature = "conversations")]
    pub(super) conversations: Option<Arc<crate::conversations::Conversations>>,
    /// The chat page and widget (`--chat`).
    #[cfg(feature = "web-chat")]
    pub(super) chat: Option<Arc<super::chat::Chat>>,
    /// Paired monitoring devices (`--devices`).
    #[cfg(feature = "devices")]
    pub(super) devices: Option<Arc<crate::devices::Devices>>,
    /// The address clients reach this server at (`--public-url`), without a trailing slash.
    #[cfg(feature = "devices")]
    pub(super) public_url: Option<String>,
    /// The approvals inbox (`--approvals`).
    #[cfg(feature = "approvals")]
    pub(super) approvals: Option<crate::approvals::Approvals>,
    /// Who hears when a run fails or waits for a person.
    #[cfg(feature = "notify")]
    pub(super) notifications: Option<Arc<crate::notify::Notifications>>,
    /// Web Push (`--push`), one of the notifiers.
    #[cfg(feature = "web-push")]
    pub(super) push: Option<Arc<crate::notify::webpush::WebPush>>,
    /// The monitoring page (`--monitor`).
    #[cfg(feature = "web-monitor")]
    pub(super) monitor: Option<Arc<super::monitor::Monitor>>,
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
    /// The agents' workspace (`--workspace`), shared with the `files` tools.
    #[cfg(feature = "files")]
    pub(super) workspace: Option<crate::setup::ServedWorkspace>,
    /// The workspace routes under `/v1/files` (`--workspace-api`).
    #[cfg(feature = "files")]
    pub(super) file_api: Option<Arc<super::files_api::FileApi>>,
    /// The skill store behind `/v1/skills` (the default store, opened lazily).
    #[cfg(feature = "skills")]
    pub(super) skills: Option<Arc<graphrt_skill::SkillStore>>,
    /// The lesson store behind `/v1/learning` (the default store, opened lazily).
    #[cfg(feature = "learning")]
    pub(super) lessons: Option<Arc<graphrt_learn::LessonStore>>,
}

impl AppState {
    /// Create state from compiled engines.
    pub fn new(workflows: IndexMap<String, ServedWorkflow>, api_key: Option<String>) -> Self {
        let memory = Arc::new(MemoryStore::new());
        let store_writes = Arc::new(WriteLatency::default());
        Self {
            workflows,
            runs: Mutex::new(HashMap::new()),
            max_active_runs: 0,
            draining: AtomicBool::new(false),
            stop: Arc::new(tokio::sync::Notify::new()),
            store: Arc::new(TimedStore::new(memory.clone(), Arc::clone(&store_writes))),
            store_writes,
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
            #[cfg(feature = "jwt")]
            user_resume: false,
            #[cfg(feature = "conversations")]
            conversations: None,
            #[cfg(feature = "web-chat")]
            chat: None,
            #[cfg(feature = "devices")]
            devices: None,
            #[cfg(feature = "devices")]
            public_url: None,
            #[cfg(feature = "approvals")]
            approvals: None,
            #[cfg(feature = "notify")]
            notifications: None,
            #[cfg(feature = "web-push")]
            push: None,
            #[cfg(feature = "web-monitor")]
            monitor: None,
            #[cfg(feature = "online-eval")]
            services: None,
            #[cfg(feature = "online-eval")]
            online: None,
            #[cfg(feature = "langfuse")]
            langfuse: None,
            #[cfg(feature = "trust-gateway")]
            gateway_agent: None,
            #[cfg(feature = "files")]
            workspace: None,
            #[cfg(feature = "files")]
            file_api: None,
            #[cfg(feature = "skills")]
            skills: graphrt_skill::SkillStore::default_root()
                .map(|root| Arc::new(graphrt_skill::SkillStore::open(root))),
            #[cfg(feature = "learning")]
            lessons: graphrt_learn::LessonStore::default_root()
                .map(|root| Arc::new(graphrt_learn::LessonStore::open(root))),
        }
    }

    /// The workspace the served agents use, which [`AppState::with_workspace_api`] can serve.
    #[cfg(feature = "files")]
    pub fn with_workspace(mut self, workspace: Option<crate::setup::ServedWorkspace>) -> Self {
        self.workspace = workspace;
        self
    }

    /// Serve the workspace under `/v1/files` (`--workspace-api`), with uploads
    /// of at most `max_upload_bytes`. Call after [`AppState::with_user_auth`]:
    /// `users` needs verified user tokens.
    ///
    /// @throws CliError (usage) - Without a workspace, or `users` without `--workspace-per-user` and `--verify-jwt`.
    #[cfg(feature = "files")]
    pub fn with_workspace_api(
        mut self,
        who: Option<crate::args::WorkspaceApi>,
        max_upload_bytes: usize,
    ) -> Result<Self, CliError> {
        let Some(who) = who else { return Ok(self) };
        let served = self.workspace.clone().ok_or_else(|| {
            CliError::usage("`--workspace-api` needs `--workspace DIR`: there is no workspace to serve")
        })?;
        #[cfg(feature = "jwt")]
        let verifies_user_tokens = self.user_auth.is_some();
        #[cfg(not(feature = "jwt"))]
        let verifies_user_tokens = false;
        let api = super::files_api::FileApi::new(served, who, verifies_user_tokens, max_upload_bytes)?;
        self.file_api = Some(Arc::new(api));
        Ok(self)
    }

    /// The banner line of the workspace routes, when they are served.
    #[cfg(feature = "files")]
    pub(super) fn file_api_line(&self) -> Option<String> {
        self.file_api.as_deref().map(super::files_api::FileApi::describe)
    }

    /// Serve `/v1/skills` from `store` instead of the default skill store.
    #[cfg(feature = "skills")]
    pub fn with_skill_store(mut self, store: Option<Arc<graphrt_skill::SkillStore>>) -> Self {
        self.skills = store;
        self
    }

    /// The skill store behind `/v1/skills`.
    #[cfg(feature = "skills")]
    pub fn skill_store(&self) -> Option<&Arc<graphrt_skill::SkillStore>> {
        self.skills.as_ref()
    }

    /// Serve `/v1/learning` from `store` instead of the default lesson store.
    #[cfg(feature = "learning")]
    pub fn with_lesson_store(mut self, store: Option<Arc<graphrt_learn::LessonStore>>) -> Self {
        self.lessons = store;
        self
    }

    /// The lesson store behind `/v1/learning`.
    #[cfg(feature = "learning")]
    pub fn lesson_store(&self) -> Option<&Arc<graphrt_learn::LessonStore>> {
        self.lessons.as_ref()
    }

    /// The agents whose lessons the nodes of workflow `name` read
    /// (`learning.agent` of its `agent` nodes), without duplicates.
    #[cfg(feature = "learning")]
    pub(crate) fn learning_agents(&self, name: &str) -> Vec<String> {
        let Some(served) = self.workflows.get(name) else { return Vec::new() };
        let mut agents: Vec<String> = Vec::new();
        for node in &served.engine.graph().workflow().graph.nodes {
            let agent = node.config.get("learning").and_then(|learning| learning.get("agent"));
            if let Some(agent) = agent.and_then(serde_json::Value::as_str)
                && node.kind == "agent"
                && !agents.iter().any(|known| known == agent)
            {
                agents.push(agent.to_owned());
            }
        }
        agents
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

    /// Let users resume their own paused runs (`--user-resume`).
    #[cfg(feature = "jwt")]
    pub fn with_user_resume(mut self, user_resume: bool) -> Self {
        self.user_resume = user_resume;
        self
    }

    /// Keep conversations on the server (`--conversations`).
    #[cfg(feature = "conversations")]
    pub fn with_conversations(mut self, conversations: Option<crate::conversations::Conversations>) -> Self {
        self.conversations = conversations.map(Arc::new);
        self
    }

    /// The conversations kept by the server, when it keeps any.
    #[cfg(feature = "conversations")]
    pub fn conversations(&self) -> Option<&Arc<crate::conversations::Conversations>> {
        self.conversations.as_ref()
    }

    /// Whether `/v1/conversations` is served.
    #[cfg(feature = "web-chat")]
    pub(super) fn keeps_conversations(&self) -> bool {
        #[cfg(feature = "conversations")]
        return self.conversations.is_some();
        #[cfg(not(feature = "conversations"))]
        false
    }

    /// Serve the chat page and widget (`--chat`).
    #[cfg(feature = "web-chat")]
    pub fn with_chat(mut self, chat: Option<super::chat::Chat>) -> Self {
        self.chat = chat.map(Arc::new);
        self
    }

    /// Pair monitoring devices (`--devices`). `public_url` is the address they
    /// reach this server at (`--public-url`), for pairing links.
    #[cfg(feature = "devices")]
    pub fn with_devices(mut self, devices: Option<crate::devices::Devices>, public_url: Option<String>) -> Self {
        self.devices = devices.map(Arc::new);
        self.public_url = public_url;
        self
    }

    /// The paired devices, when the server pairs any.
    #[cfg(feature = "devices")]
    pub fn devices(&self) -> Option<&Arc<crate::devices::Devices>> {
        self.devices.as_ref()
    }

    /// The address clients reach this server at, when the operator named it.
    #[cfg(feature = "devices")]
    pub(crate) fn public_url(&self) -> Option<&str> {
        self.public_url.as_deref()
    }

    /// Whether the monitoring page is served, so a pairing can link to it.
    #[cfg(feature = "devices")]
    pub(crate) fn serves_monitor_page(&self) -> bool {
        #[cfg(feature = "web-monitor")]
        return self.monitor.is_some();
        #[cfg(not(feature = "web-monitor"))]
        false
    }

    /// Drop what a revoked device left behind: its push subscriptions.
    #[cfg(feature = "devices")]
    pub(crate) fn forget_device(&self, id: &str) {
        #[cfg(feature = "web-push")]
        if let Some(push) = &self.push {
            push.forget_device(id);
        }
        #[cfg(not(feature = "web-push"))]
        let _ = id;
    }

    /// Tell `notifications` when a run fails or waits for a person.
    #[cfg(feature = "notify")]
    pub fn with_notifications(mut self, notifications: Option<crate::notify::Notifications>) -> Self {
        self.notifications = notifications.filter(|notifications| !notifications.is_empty()).map(Arc::new);
        self
    }

    /// Serve the Web Push API (`--push`). The caller also adds it to the notifiers.
    #[cfg(feature = "web-push")]
    pub fn with_push(mut self, push: Option<Arc<crate::notify::webpush::WebPush>>) -> Self {
        self.push = push;
        self
    }

    /// Web Push, when the server sends push notifications.
    #[cfg(feature = "web-push")]
    pub fn push(&self) -> Option<&Arc<crate::notify::webpush::WebPush>> {
        self.push.as_ref()
    }

    /// Serve the monitoring page (`--monitor`).
    #[cfg(feature = "web-monitor")]
    pub fn with_monitor(mut self, monitor: Option<super::monitor::Monitor>) -> Self {
        self.monitor = monitor.map(Arc::new);
        self
    }

    /// Serve the approvals inbox (`--approvals`).
    #[cfg(feature = "approvals")]
    pub fn with_approvals(mut self, approvals: Option<crate::approvals::Approvals>) -> Self {
        self.approvals = approvals;
        self
    }

    /// The approvals inbox, when the server has one.
    #[cfg(feature = "approvals")]
    pub fn approvals(&self) -> Option<crate::approvals::Approvals> {
        self.approvals
    }

    /// The declared start inputs of the served workflow `name` (`None` when it is not served).
    #[cfg(any(feature = "web-chat", feature = "conversations"))]
    pub(crate) fn start_inputs(&self, name: &str) -> Option<Vec<String>> {
        let workflow = self.workflows.get(name)?;
        let declared = workflow
            .engine
            .graph()
            .workflow()
            .graph
            .nodes
            .iter()
            .filter(|node| node.kind == "start")
            .find_map(|node| node.config.get("variables").and_then(serde_json::Value::as_array).cloned())
            .unwrap_or_default();
        Some(declared.iter().filter_map(|variable| variable["variable"].as_str().map(str::to_owned)).collect())
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
        // A chat guest is a user without `--verify-jwt`: the least trusted caller sees the answer only.
        if user_token && self.user_auth.as_ref().is_none_or(|auth| auth.events() == crate::args::UserEvents::Answer) {
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
        self.store = Arc::new(TimedStore::new(store, Arc::clone(&self.store_writes)));
        self
    }

    /// Record runs, and their scores, in these stores.
    pub fn with_stores(mut self, stores: crate::store::Stores) -> Self {
        self.store = Arc::new(TimedStore::new(stores.runs, Arc::clone(&self.store_writes)));
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

    /// Recent duration of one store write (a decaying maximum, see
    /// [`WriteLatency`]).
    pub fn store_write_estimate(&self) -> std::time::Duration {
        self.store_writes.estimate()
    }

    /// Stop accepting runs (`503` + `Retry-After`) and report not ready on `/readyz`.
    pub fn begin_drain(&self) {
        self.draining.store(true, Ordering::Release);
    }

    /// Ask the server to drain and exit, as `SIGTERM` does (the gateway's
    /// `POST /internal/v1/stop`; [`crate::drain::ShutdownSignals`] delivers it).
    pub fn request_stop(&self) {
        self.stop.notify_one();
    }

    /// Where [`AppState::request_stop`] is delivered.
    pub fn stop_requests(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.stop)
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
impl AppState {
    /// Starts the loaded plugins' processes now (see [`Extensions::prewarm_plugins`]).
    pub(crate) fn prewarm_plugins(&self) {
        self.extensions.prewarm_plugins();
    }
}

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

    /// Loaded plugins that would serve a node without starting a process first.
    pub(crate) fn warm_plugins(&self) -> Vec<String> {
        self.extensions.warm_plugins()
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
