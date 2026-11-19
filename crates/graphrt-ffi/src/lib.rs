//! # graphrt-ffi
//!
//! Embedding API for apps written in Kotlin (Android), Swift (iOS / macOS),
//! Python and other languages supported by UniFFI.
//!
//! The surface is deliberately small and JSON-string based so it stays stable
//! across languages and versions:
//!
//! ```text
//! Runtime::new(config_json)                       // providers, env, limits
//!   .register_wasm_plugin(bytes)                  // optional, sandboxed plugins
//!   .load_workflow(yaml_or_json) -> WorkflowHandle
//! WorkflowHandle.run(inputs_json, sys_json, listener) -> RunHandle
//! WorkflowHandle.resume(snapshot_json, inputs_json, secrets_json, listener) -> RunHandle
//! RunHandle.abort(reason) / pause(reason) / wait_json(timeout_ms)
//! Runtime.set_credential_provider(provider, callback)  // keys from Keychain / Keystore
//! ```
//!
//! Runs execute on a Tokio runtime owned by [`Runtime`]; listener callbacks are
//! invoked from its worker threads, never from the caller's thread. Nothing in
//! this crate spawns processes, so every feature is usable on iOS and Android.
//!
//! ## Config JSON
//!
//! ```json
//! {
//!   "providers": { "OPENAI_API_KEY": "sk-...", "OLLAMA_HOST": "http://10.0.2.2:11434" },
//!   "env": { "CRM_TOKEN": "..." },
//!   "limits": { "max_steps": 10000, "max_duration_ms": 60000, "default_node_timeout_ms": 30000 },
//!   "allow_private_network": false,
//!   "knowledge": { "...": "knowledge-retrieval datasets (see graphrt-rag)" },
//!   "mcp_servers": { "mcpServers": { "docs": { "url": "https://..." } } },
//!   "wasm": { "fuel": 1000000000, "memory_bytes": 67108864 },
//!   "credentials": {
//!     "openai": { "source": "external" },
//!     "proxy": { "source": "external", "kind": "openai", "base_url": "https://llm-proxy.example.com/v1" },
//!     "anthropic": { "source": "token_broker", "url": "https://api.example.com/llm-token", "app_auth": "external" }
//!   }
//! }
//! ```
//!
//! `credentials` declares providers whose keys never appear in the config: an
//! `external` provider asks the [`CredentialProvider`] registered with
//! [`Runtime::set_credential_provider`]; a `token_broker` provider fetches
//! short-lived tokens from your backend. See `docs/credentials.md`.
//!
//! `providers` uses the same variable names as the CLI's environment
//! (`OPENAI_API_KEY`, `OPENAI_BASE_URL`, `ANTHROPIC_API_KEY`, `OLLAMA_HOST`,
//! `GRAPHRT_OPENAI_COMPAT_<NAME>_BASE_URL`...). `env` values are exposed to
//! workflows only for the `env` variables a workflow declares.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use futures::StreamExt;
use graphrt_core::engine::{Limits, ResumeFrom};
use graphrt_core::response::ResponseAssembler;
use graphrt_core::snapshot::Snapshot;
use graphrt_core::{Engine, Event, Graph, Registry, RunInput, Selector, Services, Value, Workflow};
use indexmap::IndexMap;
use serde::Deserialize;

mod credentials;

pub use credentials::{CredentialProvider, CredentialRecord};

uniffi::setup_scaffolding!();

/// Errors surfaced to foreign code as exceptions.
#[derive(Debug, thiserror::Error, uniffi::Error)]
#[non_exhaustive]
pub enum GraphrtError {
    /// The runtime configuration JSON is invalid.
    #[error("invalid config: {message}")]
    InvalidConfig { message: String },
    /// The workflow document could not be parsed or compiled.
    #[error("invalid workflow: {message}")]
    InvalidWorkflow { message: String },
    /// Run inputs, snapshot or selector JSON is invalid.
    #[error("invalid input: {message}")]
    InvalidInput { message: String },
    /// A WebAssembly plugin could not be loaded.
    #[error("plugin error: {message}")]
    Plugin { message: String },
    /// Internal failure (runtime could not start, lock poisoned...).
    #[error("runtime error: {message}")]
    Runtime { message: String },
}

impl From<uniffi::UnexpectedUniFFICallbackError> for GraphrtError {
    fn from(e: uniffi::UnexpectedUniFFICallbackError) -> Self {
        Self::Runtime { message: format!("callback failed: {}", e.reason) }
    }
}

impl GraphrtError {
    fn config(message: impl Into<String>) -> Self {
        Self::InvalidConfig { message: message.into() }
    }
    fn input(message: impl Into<String>) -> Self {
        Self::InvalidInput { message: message.into() }
    }
}

/// Receives the events of one run. Implemented in the host language.
///
/// Callbacks run on a background thread; marshal to your UI thread yourself.
/// Exceptions thrown by a callback are swallowed so they cannot break the run.
#[uniffi::export(with_foreign)]
pub trait EventListener: Send + Sync {
    /// Every raw engine event, as JSON (`{"type": "node_started", ...}`).
    fn on_event(&self, event_json: String);
    /// Ordered user-facing answer text (answer nodes), streamed as it is produced.
    fn on_answer_chunk(&self, text: String);
    /// Called exactly once with the final result (see [`RunHandle::wait_json`]).
    fn on_finish(&self, result_json: String);
}

/// Version of the embedded runtime.
#[uniffi::export]
pub fn graphrt_version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeConfig {
    #[serde(default)]
    providers: IndexMap<String, String>,
    #[serde(default)]
    env: IndexMap<String, String>,
    #[serde(default)]
    limits: LimitsConfig,
    #[serde(default)]
    allow_private_network: bool,
    #[serde(default)]
    knowledge: Option<serde_json::Value>,
    #[serde(default)]
    mcp_servers: Option<serde_json::Value>,
    #[serde(default)]
    wasm: WasmConfig,
    #[serde(default)]
    credentials: IndexMap<String, graphrt_llm::credential_config::ProviderCredentialConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitsConfig {
    max_steps: Option<u64>,
    max_duration_ms: Option<u64>,
    max_depth: Option<u32>,
    default_node_timeout_ms: Option<u64>,
    max_concurrency: Option<usize>,
}

impl LimitsConfig {
    fn to_limits(&self) -> Limits {
        let mut limits = Limits::default();
        if let Some(v) = self.max_steps {
            limits.max_steps = v;
        }
        if self.max_duration_ms.is_some() {
            limits.max_duration_ms = self.max_duration_ms;
        }
        if let Some(v) = self.max_depth {
            limits.max_depth = v;
        }
        if self.default_node_timeout_ms.is_some() {
            limits.default_node_timeout_ms = self.default_node_timeout_ms;
        }
        if let Some(v) = self.max_concurrency {
            limits.max_concurrency = v.max(1);
        }
        limits
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct WasmConfig {
    fuel: Option<u64>,
    memory_bytes: Option<usize>,
    module_bytes: Option<usize>,
}

impl WasmConfig {
    fn to_limits(&self) -> graphrt_wasm::WasmLimits {
        let mut limits = graphrt_wasm::WasmLimits::default();
        if let Some(v) = self.fuel {
            limits.fuel = v;
        }
        if let Some(v) = self.memory_bytes {
            limits.memory_bytes = v;
        }
        if let Some(v) = self.module_bytes {
            limits.module_bytes = v;
        }
        limits
    }
}

/// A configured runtime: node registry, host services and a thread pool.
/// Create one per app and share it.
#[derive(uniffi::Object)]
pub struct Runtime {
    /// Always `Some` until drop; taken in `Drop` to shut down without blocking.
    tokio: Option<tokio::runtime::Runtime>,
    registry: Mutex<Registry>,
    services: Arc<Services>,
    env: IndexMap<String, String>,
    limits: Limits,
    wasm_limits: graphrt_wasm::WasmLimits,
    credentials: graphrt_llm::credential_config::HostCredentials,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // Do not block the dropping thread (often a UI thread) on node work that is
        // still finishing in the blocking pool (e.g. an aborted code node).
        if let Some(rt) = self.tokio.take() {
            rt.shutdown_background();
        }
    }
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime").field("limits", &self.limits).finish_non_exhaustive()
    }
}

fn base_registry() -> Registry {
    let mut registry = Registry::with_builtins();
    graphrt_llm::register(&mut registry);
    graphrt_http::register(&mut registry);
    #[cfg(feature = "code-js")]
    graphrt_code::register(&mut registry);
    #[cfg(feature = "doc")]
    graphrt_doc::register(&mut registry);
    #[cfg(feature = "rag")]
    graphrt_rag::register(&mut registry);
    registry
}

#[uniffi::export]
impl Runtime {
    /// Create a runtime from a config JSON document (`"{}"` for defaults).
    /// May perform network I/O (MCP handshakes); call it off the UI thread.
    #[uniffi::constructor]
    pub fn new(config_json: String) -> Result<Arc<Self>, GraphrtError> {
        let text = if config_json.trim().is_empty() { "{}" } else { config_json.as_str() };
        let config: RuntimeConfig = serde_json::from_str(text).map_err(|e| GraphrtError::config(e.to_string()))?;
        let tokio = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("graphrt")
            .enable_time()
            .enable_io()
            .build()
            .map_err(|e| GraphrtError::Runtime { message: format!("cannot start runtime: {e}") })?;

        let vars: Vec<(String, String)> = config.providers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let lookup =
            |name: &str| -> Option<String> { config.providers.get(name).or_else(|| config.env.get(name)).cloned() };

        let mut services = Services::new();
        let mut models =
            graphrt_llm::ModelRegistry::from_vars(&vars).map_err(|e| GraphrtError::config(e.to_string()))?;
        // Credentialed providers replace env-configured ones of the same name; their
        // keys come from the host at request time, never from this config.
        let host_credentials = graphrt_llm::credential_config::HostCredentials::new();
        // A `typesafe`-kind entry is never registered as a chat model: it becomes the
        // System One client, built from the same cached credential source.
        let applied = graphrt_llm::credential_config::apply(&mut models, &config.credentials, &host_credentials)
            .map_err(|e| GraphrtError::config(e.to_string()))?;
        services.insert(models);

        // TypeSafe System One (Jev): a `typesafe` credentials entry (token broker /
        // host keychain) wins over a static `TYPESAFE_API_KEY` provider variable.
        let typesafe = match applied.system_one {
            Some(client) => Some(client),
            None => graphrt_llm::typesafe::SystemOneClient::from_vars(&vars)
                .map_err(|e| GraphrtError::config(e.to_string()))?,
        };
        if let Some(client) = typesafe {
            services.insert(Arc::new(client));
        }

        let policy = graphrt_http::EgressPolicy { allow_private: config.allow_private_network, ..Default::default() };
        let client = graphrt_http::HttpClient::new(policy).map_err(|e| GraphrtError::config(e.to_string()))?;
        services.insert(client);

        let resolver: Arc<dyn graphrt_core::file::FileResolver> =
            Arc::new(graphrt_core::file::ChainResolver::with_data_urls());
        services.insert(resolver);

        let tools = tokio.block_on(build_tools(config.mcp_servers.as_ref()))?;
        services.insert(tools);

        #[cfg(feature = "rag")]
        if let Some(raw) = &config.knowledge {
            let kb_config: graphrt_rag::KnowledgeConfig =
                serde_json::from_value(raw.clone()).map_err(|e| GraphrtError::config(format!("knowledge: {e}")))?;
            let kb = kb_config.build_with_env(&lookup).map_err(|e| GraphrtError::config(format!("knowledge: {e}")))?;
            services.insert(kb);
        }
        #[cfg(not(feature = "rag"))]
        if config.knowledge.is_some() {
            return Err(GraphrtError::config("`knowledge` requires the `rag` feature"));
        }
        let _ = &lookup;

        Ok(Arc::new(Self {
            tokio: Some(tokio),
            registry: Mutex::new(base_registry()),
            services: Arc::new(services),
            env: config.env,
            limits: config.limits.to_limits(),
            wasm_limits: config.wasm.to_limits(),
            credentials: host_credentials,
        }))
    }

    /// Register the host's credential callback for `provider` (a name from the
    /// config's `credentials` section), or `"*"` for every provider. Replaces any
    /// previous callback and drops every cached credential, so it takes effect for
    /// requests made after the call (e.g. a different user signed in).
    pub fn set_credential_provider(&self, provider: String, callback: Arc<dyn CredentialProvider>) {
        self.credentials.set(provider, Arc::new(credentials::ForeignCredentials::new(callback)));
    }

    /// Remove the credential callback registered for `provider` (or `"*"`) and drop
    /// every cached credential (sign-out takes effect on the next request).
    pub fn clear_credential_provider(&self, provider: String) {
        self.credentials.remove(&provider);
    }

    /// Drop every cached credential without changing the registered callbacks
    /// (e.g. the user signed out of your backend and broker tokens must not be reused).
    pub fn clear_credential_cache(&self) {
        self.credentials.clear_cache();
    }

    /// Load a sandboxed WebAssembly plugin; its node kinds become available to
    /// workflows loaded afterwards. Returns the registered kinds.
    pub fn register_wasm_plugin(&self, wasm: Vec<u8>) -> Result<Vec<String>, GraphrtError> {
        let plugin = graphrt_wasm::WasmPlugin::load(&wasm, self.wasm_limits.clone())
            .map_err(|e| GraphrtError::Plugin { message: e.to_string() })?;
        let kinds = plugin.descriptors().into_iter().map(|d| d.kind).collect();
        let mut registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        plugin.register(&mut registry).map_err(|e| GraphrtError::Plugin { message: e.to_string() })?;
        Ok(kinds)
    }

    /// Catalog of every node kind available in this runtime, as a JSON array.
    pub fn catalog_json(&self) -> String {
        let registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        serde_json::to_string(&registry.descriptors()).unwrap_or_else(|_| "[]".to_owned())
    }

    /// Parse and compile a workflow (YAML or JSON).
    pub fn load_workflow(self: Arc<Self>, document: String) -> Result<Arc<WorkflowHandle>, GraphrtError> {
        let workflow =
            Workflow::parse(&document).map_err(|e| GraphrtError::InvalidWorkflow { message: e.to_string() })?;
        let graph = {
            let registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
            Graph::build(workflow, &registry).map_err(|e| GraphrtError::InvalidWorkflow { message: e.to_string() })?
        };
        let graph = Arc::new(graph);
        let engine = Engine::new(graph.clone())
            .with_shared_services(self.services.clone())
            .with_clock(Arc::new(graphrt_core::node::TokioClock))
            .with_limits(self.limits.clone());
        Ok(Arc::new(WorkflowHandle { runtime: self, graph, engine }))
    }

    /// Open a portable bundle (`.grt` bytes), register its WebAssembly plugins and
    /// compile its entry workflow. Every file is checked against the manifest's
    /// SHA-256; when `trusted_keys` (hex Ed25519 public keys) is non-empty, the
    /// bundle must be signed by one of them. Bundles needing process or HTTP
    /// plugins are rejected: embedded hosts cannot spawn processes.
    pub fn load_bundle(
        self: Arc<Self>,
        bundle: Vec<u8>,
        trusted_keys: Vec<String>,
    ) -> Result<Arc<WorkflowHandle>, GraphrtError> {
        let mut options = graphrt_bundle::OpenOptions::default();
        for key in trusted_keys {
            options = options.trust_key(key);
        }
        let opened = graphrt_bundle::Bundle::open(&bundle, &options)
            .map_err(|e| GraphrtError::InvalidWorkflow { message: e.to_string() })?;
        let non_portable: Vec<&str> =
            opened.manifest().requires.plugins.iter().filter(|p| !p.portable).map(|p| p.name.as_str()).collect();
        if !non_portable.is_empty() {
            return Err(GraphrtError::Plugin {
                message: format!(
                    "bundle needs process/http plugins ({}), which cannot run embedded",
                    non_portable.join(", ")
                ),
            });
        }
        let workflow = opened.workflow(None).map_err(|e| GraphrtError::InvalidWorkflow { message: e.to_string() })?;
        let graph = {
            let mut registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
            opened
                .register_wasm_plugins(&mut registry, &self.wasm_limits)
                .map_err(|e| GraphrtError::Plugin { message: e.to_string() })?;
            let report = opened.check(&registry);
            if let Some(mismatch) = report.version_mismatch {
                return Err(GraphrtError::InvalidWorkflow { message: mismatch });
            }
            Graph::build(workflow, &registry).map_err(|e| GraphrtError::InvalidWorkflow { message: e.to_string() })?
        };
        let graph = Arc::new(graph);
        let engine = Engine::new(graph.clone())
            .with_shared_services(self.services.clone())
            .with_clock(Arc::new(graphrt_core::node::TokioClock))
            .with_limits(self.limits.clone());
        Ok(Arc::new(WorkflowHandle { runtime: self, graph, engine }))
    }
}

async fn build_tools(raw: Option<&serde_json::Value>) -> Result<graphrt_core::tool::ToolRegistry, GraphrtError> {
    let Some(raw) = raw else {
        return Ok(graphrt_core::tool::ToolRegistry::new());
    };
    #[cfg(feature = "mcp-http")]
    {
        let servers: graphrt_mcp::McpServers =
            serde_json::from_value(raw.clone()).map_err(|e| GraphrtError::config(format!("mcp_servers: {e}")))?;
        let configs = servers.into_configs();
        for cfg in &configs {
            if !matches!(cfg.transport, graphrt_mcp::McpTransport::Http(_)) {
                return Err(GraphrtError::config(format!(
                    "mcp server `{}`: only HTTP servers are supported when embedded (no process spawning)",
                    cfg.name
                )));
            }
        }
        graphrt_mcp::connect_all(&configs).await.map_err(|e| GraphrtError::config(format!("mcp: {e}")))
    }
    #[cfg(not(feature = "mcp-http"))]
    {
        let _ = raw;
        Err(GraphrtError::config("`mcp_servers` requires the `mcp-http` feature"))
    }
}

/// A compiled workflow, ready to run any number of times concurrently.
#[derive(uniffi::Object)]
pub struct WorkflowHandle {
    runtime: Arc<Runtime>,
    graph: Arc<Graph>,
    engine: Engine,
}

impl std::fmt::Debug for WorkflowHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowHandle").field("graph", &self.graph).finish_non_exhaustive()
    }
}

#[uniffi::export]
impl WorkflowHandle {
    /// Workflow name from the document, if any.
    pub fn name(&self) -> Option<String> {
        self.graph.workflow().name.clone()
    }

    /// Non-fatal findings from compilation (e.g. unreachable nodes).
    pub fn warnings(&self) -> Vec<String> {
        self.graph.warnings().to_vec()
    }

    /// Descriptors of the node kinds this workflow uses, as a JSON array.
    pub fn catalog_json(&self) -> String {
        let registry = self.runtime.registry.lock().unwrap_or_else(PoisonError::into_inner);
        let mut kinds: Vec<&str> = self.graph.nodes().iter().map(|n| n.kind.as_ref()).collect();
        kinds.sort_unstable();
        kinds.dedup();
        let descriptors: Vec<_> = kinds.into_iter().filter_map(|k| registry.descriptor(k)).collect();
        serde_json::to_string(&descriptors).unwrap_or_else(|_| "[]".to_owned())
    }

    /// The workflow document as canonical JSON.
    pub fn document_json(&self) -> String {
        serde_json::to_string(self.graph.workflow()).unwrap_or_default()
    }

    /// Start a run. `inputs_json` / `sys_json` are JSON objects (`"{}"` when empty).
    pub fn run(
        &self,
        inputs_json: String,
        sys_json: String,
        listener: Arc<dyn EventListener>,
    ) -> Result<Arc<RunHandle>, GraphrtError> {
        let mut input = self.base_input(&sys_json)?;
        input.inputs = parse_object(&inputs_json, "inputs")?;
        Ok(self.start(input, None, listener))
    }

    /// Resume a paused run from the snapshot carried by its `run_paused` event.
    /// `inputs_json` maps paused node ids (or `frame/node` ids) to their input,
    /// e.g. `{"review": {"action": "approve", "values": {}}}`. `secrets_json`
    /// re-supplies non-env secrets as `{"namespace.name": "value"}`.
    pub fn resume(
        &self,
        snapshot_json: String,
        inputs_json: String,
        secrets_json: String,
        listener: Arc<dyn EventListener>,
    ) -> Result<Arc<RunHandle>, GraphrtError> {
        let snapshot =
            Snapshot::from_json(&snapshot_json).map_err(|e| GraphrtError::input(format!("snapshot: {e}")))?;
        let mut input = self.base_input("{}")?;
        let mut resume = ResumeFrom::new(snapshot.clone(), parse_object(&inputs_json, "inputs")?);
        let secrets: IndexMap<String, String> = if secrets_json.trim().is_empty() {
            IndexMap::new()
        } else {
            serde_json::from_str(&secrets_json).map_err(|e| GraphrtError::input(format!("secrets: {e}")))?
        };
        for (selector, value) in secrets {
            resume = resume.secret(selector, value);
        }
        input.resume = Some(resume);
        Ok(self.start(input, Some(snapshot), listener))
    }
}

impl WorkflowHandle {
    fn base_input(&self, sys_json: &str) -> Result<RunInput, GraphrtError> {
        let mut input = RunInput::new();
        input.sys = parse_object(sys_json, "sys")?;
        for decl in &self.graph.workflow().env {
            if let Some(value) = self.runtime.env.get(&decl.name) {
                input.env.insert(decl.name.clone(), Value::from(value.as_str()));
            }
        }
        Ok(input)
    }

    fn start(
        &self,
        input: RunInput,
        resumed_from: Option<Snapshot>,
        listener: Arc<dyn EventListener>,
    ) -> Arc<RunHandle> {
        let mut assembler = ResponseAssembler::new(&self.graph);
        for (k, v) in &input.sys {
            assembler = assembler.with_value(&Selector::of("sys", k), v.clone());
        }
        if let Some(snapshot) = &resumed_from {
            assembler = assembler.resume_from(&snapshot.root);
        }
        let mut stream = self.engine.run(input);
        let run = Arc::new(RunHandle { control: stream.handle(), result: Mutex::new(None), done: Condvar::new() });
        let task_run = run.clone();
        let Some(tokio) = self.runtime.tokio.as_ref() else {
            let result = result_json(None, "");
            run.complete(result.clone());
            notify(|| listener.on_finish(result));
            return run;
        };
        tokio.spawn(async move {
            let mut terminal: Option<Event> = None;
            while let Some(event) = stream.next().await {
                if let Ok(json) = serde_json::to_string(&event) {
                    notify(|| listener.on_event(json));
                }
                for chunk in assembler.push(&event) {
                    if !chunk.text.is_empty() {
                        notify(|| listener.on_answer_chunk(chunk.text.clone()));
                    }
                }
                if event.is_terminal() {
                    terminal = Some(event);
                }
            }
            for chunk in assembler.finish() {
                if !chunk.text.is_empty() {
                    notify(|| listener.on_answer_chunk(chunk.text.clone()));
                }
            }
            let result = result_json(terminal.as_ref(), assembler.text_so_far());
            task_run.complete(result.clone());
            notify(|| listener.on_finish(result));
        });
        run
    }
}

/// Invoke a foreign callback, containing any panic it causes.
fn notify(f: impl FnOnce()) {
    let _ = catch_unwind(AssertUnwindSafe(f));
}

fn parse_object(json: &str, what: &str) -> Result<IndexMap<String, Value>, GraphrtError> {
    if json.trim().is_empty() {
        return Ok(IndexMap::new());
    }
    let raw: serde_json::Value = serde_json::from_str(json).map_err(|e| GraphrtError::input(format!("{what}: {e}")))?;
    match raw {
        serde_json::Value::Object(map) => Ok(map.into_iter().map(|(k, v)| (k, Value::from_json(v))).collect()),
        serde_json::Value::Null => Ok(IndexMap::new()),
        _ => Err(GraphrtError::input(format!("{what}: expected a JSON object"))),
    }
}

/// Final result: the terminal event's fields plus `status` and the assembled `answer`.
fn result_json(terminal: Option<&Event>, answer: &str) -> String {
    let status = match terminal {
        Some(Event::RunSucceeded { .. }) => "succeeded",
        Some(Event::RunFailed { .. }) => "failed",
        Some(Event::RunAborted { .. }) => "aborted",
        Some(Event::RunPaused { .. }) => "paused",
        _ => "failed",
    };
    let mut body = match terminal.map(serde_json::to_value) {
        Some(Ok(serde_json::Value::Object(map))) => map,
        _ => {
            let mut map = serde_json::Map::new();
            map.insert("error".into(), serde_json::Value::from("run ended without a terminal event"));
            map.insert("error_type".into(), serde_json::Value::from("internal"));
            map
        }
    };
    body.remove("type");
    body.insert("status".into(), serde_json::Value::from(status));
    body.insert("answer".into(), serde_json::Value::from(answer));
    serde_json::Value::Object(body).to_string()
}

/// Controls one run.
#[derive(uniffi::Object)]
pub struct RunHandle {
    control: graphrt_core::RunHandle,
    result: Mutex<Option<String>>,
    done: Condvar,
}

impl std::fmt::Debug for RunHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunHandle").field("finished", &self.is_finished()).finish_non_exhaustive()
    }
}

impl RunHandle {
    fn complete(&self, result: String) {
        let mut slot = self.result.lock().unwrap_or_else(PoisonError::into_inner);
        *slot = Some(result);
        self.done.notify_all();
    }
}

#[uniffi::export]
impl RunHandle {
    /// Cancel the run immediately. It ends with status `aborted`.
    pub fn abort(&self, reason: String) {
        self.control.abort(reason);
    }

    /// Stop scheduling new nodes; the run ends with status `paused` and a snapshot.
    pub fn pause(&self, reason: String) {
        self.control.pause(reason);
    }

    /// Write a variable (`"conversation.topic"`) while the run is in progress.
    pub fn update_variable(&self, selector: String, value_json: String) -> Result<(), GraphrtError> {
        let selector = Selector::parse(&selector).map_err(|e| GraphrtError::input(e.to_string()))?;
        let value: serde_json::Value =
            serde_json::from_str(&value_json).map_err(|e| GraphrtError::input(format!("value: {e}")))?;
        self.control.update_variable(selector, Value::from_json(value));
        Ok(())
    }

    pub fn is_finished(&self) -> bool {
        self.result.lock().unwrap_or_else(PoisonError::into_inner).is_some()
    }

    /// Block until the run finishes (or `timeout_ms` elapses) and return the result
    /// JSON (`{"status": "succeeded"|"failed"|"aborted"|"paused", "answer": ..., ...}`).
    /// Returns `None` on timeout. Do not call from a UI thread.
    pub fn wait_json(&self, timeout_ms: u64) -> Option<String> {
        let guard = self.result.lock().unwrap_or_else(PoisonError::into_inner);
        let (guard, _) = self
            .done
            .wait_timeout_while(guard, Duration::from_millis(timeout_ms), |r| r.is_none())
            .unwrap_or_else(PoisonError::into_inner);
        guard.clone()
    }
}
