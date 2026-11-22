//! `graphrt serve --trust-gateway`: the agent side of the gateway protocol
//! (`Graphrt-Protocol: 1`).
//!
//! A graphrt gateway is the only writer of the run store; agents keep an
//! in-memory store and execute what the gateway sends them. Every request under
//! `/internal/v1` must carry a valid HMAC signature ([`signature`]) from a key
//! of `$GRAPHRT_GATEWAY_KEYS` for this agent's `--deployment`.
//!
//! | Method | Path | Purpose |
//! | --- | --- | --- |
//! | GET | `/internal/v1/hello` | protocol, version, workflow fingerprints, load, draining |
//! | POST | `/internal/v1/runs` | start (or resume, with the snapshot inline) run `Graphrt-Run-Id` at `Graphrt-Epoch`; SSE |
//! | GET | `/internal/v1/runs/{id}/stream?from=N` | reattach to an execution after a lost connection |
//! | POST | `/internal/v1/runs/{id}/abort` | abort the execution at `Graphrt-Epoch` |
//! | POST | `/internal/v1/drain` | stop accepting runs and pause the active ones |
//! | GET | `/internal/v1/orphans` | paused executions whose gateway stream was lost (reclaim) |
//! | DELETE | `/internal/v1/orphans/{id}` | the gateway took over an orphan |
//!
//! When the gateway's stream of a run breaks, the run is not aborted (as a
//! public streaming request's would be): the agent waits a few seconds for a
//! reattach, then preempts the run and holds the result as an *orphan* for the
//! reclaim grace (`--reclaim-grace-secs`, 60 s), until the gateway pulls it with
//! `GET /internal/v1/orphans`. The agent never calls the gateway. See
//! `docs/gateway-protocol.md`.

pub mod body;
mod json;
mod reclaim;
mod routes;
pub mod signature;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use graphrt_core::response::ResponseAssembler;
use graphrt_core::snapshot::FrameSnapshot;
use graphrt_core::{Graph, Selector, Value};
use indexmap::IndexMap;

pub use self::reclaim::Orphan;
use self::signature::{DEPLOYMENT_HEADER, EPOCH_HEADER, RUN_ID_HEADER, SIGNATURE_HEADER, Signed, Verifier};
use crate::serve::AppState;

/// The protocol version this agent speaks.
pub const PROTOCOL_VERSION: u32 = 1;
/// Protocol header, sent on every internal response and required on requests.
pub const PROTOCOL_HEADER: &str = "graphrt-protocol";
/// Environment variable holding the gateway keys (`kid:hexsecret[,kid:hexsecret]`).
pub const KEYS_ENV: &str = "GRAPHRT_GATEWAY_KEYS";
/// Default `--reclaim-grace-secs`.
pub const DEFAULT_RECLAIM_GRACE_SECS: u64 = 60;
/// Default time a broken gateway stream may be reattached before the run is paused.
pub const DEFAULT_REATTACH_WINDOW: Duration = Duration::from_secs(3);
/// Executions remembered (for reattach and replay of a retried start) before
/// finished ones are forgotten.
const MAX_EXECUTIONS: usize = 4_096;

/// Settings of `--trust-gateway`.
#[derive(Debug)]
pub struct GatewayAgentConfig {
    /// The deployment this agent serves (`--deployment`).
    pub deployment: String,
    pub keys: Vec<signature::GatewayKey>,
    pub reclaim_grace: Duration,
    /// How long a broken stream may be reattached before the run is paused.
    pub reattach_window: Duration,
    /// Largest signed request body read for verification.
    pub max_body_bytes: usize,
}

/// One execution started by the gateway: the epoch it was given and how many
/// gateway streams are attached to it.
#[derive(Clone)]
pub(crate) struct Execution {
    pub epoch: u64,
    /// Global index of the execution's first event (the gateway's `event_index`).
    pub base_index: u64,
    attached: usize,
    /// Rebuilds the response assembler of the execution, for reattached streams.
    assembler_seed: Arc<AssemblerSeed>,
}

impl std::fmt::Debug for Execution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Execution").field("epoch", &self.epoch).field("base_index", &self.base_index).finish()
    }
}

/// What a run's response assembler is built from, so a reattached stream
/// assembles exactly the same `response` events as the original one.
#[derive(Debug)]
pub(crate) struct AssemblerSeed {
    pub graph: Arc<Graph>,
    pub resume_root: Option<FrameSnapshot>,
    pub sys: IndexMap<String, Value>,
}

impl AssemblerSeed {
    pub fn assembler(&self) -> ResponseAssembler {
        let mut assembler = ResponseAssembler::new(&self.graph);
        if let Some(root) = &self.resume_root {
            assembler = assembler.resume_from(root);
        }
        for (k, v) in &self.sys {
            assembler = assembler.with_value(&Selector::of("sys", k), v.clone());
        }
        assembler
    }
}

/// State of the agent side of the gateway protocol.
#[derive(Debug)]
pub struct GatewayAgent {
    config: GatewayAgentConfig,
    verifier: Verifier,
    executions: Mutex<HashMap<String, Execution>>,
    orphans: Mutex<HashMap<String, Orphan>>,
}

/// What a verified request is about (its signed headers).
#[derive(Clone, Debug, Default)]
pub struct GatewayRequest {
    pub run_id: Option<String>,
    pub epoch: Option<u64>,
}

impl GatewayAgentConfig {
    /// The configuration of `--trust-gateway`, or `None` without it.
    ///
    /// @param args - The parsed flags.
    /// @param lookup_env - Reads an environment variable (process env, then `--env-file`).
    /// @param max_body_bytes - Largest signed request body (`--max-body-bytes`).
    /// @throws CliError - Usage error when the deployment or keys are missing or invalid.
    pub fn from_args(
        args: &crate::args::GatewayTrustArgs,
        lookup_env: impl Fn(&str) -> Option<String>,
        max_body_bytes: usize,
    ) -> Result<Option<Self>, crate::error::CliError> {
        use crate::error::CliError;
        if !args.trust_gateway {
            return Ok(None);
        }
        let deployment = args
            .deployment
            .clone()
            .or_else(|| lookup_env("GRAPHRT_DEPLOYMENT"))
            .filter(|d| !d.is_empty())
            .ok_or_else(|| CliError::usage("--trust-gateway needs --deployment NAME (or $GRAPHRT_DEPLOYMENT)"))?;
        graphrt_core::coordination::validate_key(&deployment)
            .ok()
            .filter(|()| !deployment.contains('/'))
            .ok_or_else(|| CliError::usage(format!("invalid deployment name `{deployment}`")))?;
        let keys_text = lookup_env(&args.gateway_keys_env).filter(|k| !k.is_empty()).ok_or_else(|| {
            CliError::usage(format!("--trust-gateway needs the gateway keys in ${}", args.gateway_keys_env))
        })?;
        let keys = signature::parse_keys(&keys_text)
            .map_err(|e| CliError::usage(format!("${}: {e}", args.gateway_keys_env)))?;
        Ok(Some(Self {
            deployment,
            keys,
            reclaim_grace: Duration::from_secs(args.reclaim_grace_secs),
            reattach_window: DEFAULT_REATTACH_WINDOW,
            max_body_bytes,
        }))
    }
}

impl GatewayAgent {
    /// Agent state from `config`.
    pub fn new(config: GatewayAgentConfig) -> Self {
        let verifier = Verifier::new(config.keys.clone());
        Self { config, verifier, executions: Mutex::new(HashMap::new()), orphans: Mutex::new(HashMap::new()) }
    }

    /// The deployment this agent serves.
    pub fn deployment(&self) -> &str {
        &self.config.deployment
    }

    fn executions(&self) -> MutexGuard<'_, HashMap<String, Execution>> {
        self.executions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn orphans(&self) -> MutexGuard<'_, HashMap<String, Orphan>> {
        self.orphans.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn execution(&self, run_id: &str) -> Option<Execution> {
        self.executions().get(run_id).cloned()
    }

    /// Register a new execution with its first stream attached. Past
    /// [`MAX_EXECUTIONS`], executions that are neither running (`is_active`),
    /// attached nor orphaned are forgotten.
    fn begin(
        &self,
        run_id: &str,
        epoch: u64,
        base_index: u64,
        assembler_seed: AssemblerSeed,
        is_active: impl Fn(&str) -> bool,
    ) {
        // A new execution supersedes an orphan of an older epoch.
        self.orphans().remove(run_id);
        let orphaned: Vec<String> = self.orphans().keys().cloned().collect();
        let mut executions = self.executions();
        if executions.len() >= MAX_EXECUTIONS {
            executions.retain(|id, e| e.attached > 0 || is_active(id) || orphaned.contains(id));
        }
        let execution = Execution { epoch, base_index, attached: 1, assembler_seed: Arc::new(assembler_seed) };
        executions.insert(run_id.to_owned(), execution);
    }

    /// Attach one more stream to the execution of `run_id` at `epoch`.
    fn attach(&self, run_id: &str, epoch: u64) -> Option<Execution> {
        let mut executions = self.executions();
        let execution = executions.get_mut(run_id).filter(|e| e.epoch == epoch)?;
        execution.attached += 1;
        Some(execution.clone())
    }

    /// Detach a stream; returns how many remain attached.
    fn detach(&self, run_id: &str, epoch: u64) -> usize {
        let mut executions = self.executions();
        match executions.get_mut(run_id).filter(|e| e.epoch == epoch) {
            Some(execution) => {
                execution.attached = execution.attached.saturating_sub(1);
                execution.attached
            }
            None => 0,
        }
    }

    fn attached(&self, run_id: &str, epoch: u64) -> usize {
        self.executions().get(run_id).filter(|e| e.epoch == epoch).map_or(0, |e| e.attached)
    }
}

/// Run `task` in the background. Every background task of the gateway protocol
/// goes through here as one boxed type, so the binary holds one copy of
/// tokio's task code for them instead of one per future type.
pub(crate) fn spawn_task(runtime: &tokio::runtime::Handle, task: futures::future::BoxFuture<'static, ()>) {
    drop(runtime.spawn(task));
}

/// JSON error body `{error, error_type}` with `status`.
pub(crate) fn protocol_error(status: StatusCode, error_type: &str, message: &str) -> Response {
    json::response(status, json::Object::new().str("error", message).str("error_type", error_type).finish())
}

fn header_text<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// A request whose signature checked out, with its body read.
pub(crate) struct Verified {
    pub parts: axum::http::request::Parts,
    pub body: axum::body::Bytes,
    pub request: GatewayRequest,
}

/// The one handler of `/internal/v1/*`: check the protocol version and the
/// request signature (over the body, read here, bounded), refuse other
/// deployments, dispatch, and stamp `Graphrt-Protocol` on the response.
///
/// A single handler that routes by itself, instead of one axum route per
/// endpoint behind a middleware, keeps the agent binary small: every axum
/// handler and extractor is a separate copy of generic code.
async fn handle(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let mut response = match state.gateway_agent().cloned() {
        None => protocol_error(StatusCode::NOT_FOUND, "not_found", "this server does not trust a gateway"),
        Some(agent) => match verified(&agent, request).await {
            Ok(verified) => routes::dispatch(Arc::clone(&state), agent, verified).await,
            Err(refused) => refused,
        },
    };
    response.headers_mut().insert(PROTOCOL_HEADER, HeaderValue::from(PROTOCOL_VERSION));
    response
}

async fn verified(agent: &GatewayAgent, request: Request) -> Result<Verified, Response> {
    let (parts, body) = request.into_parts();
    let protocol = header_text(&parts.headers, PROTOCOL_HEADER);
    if protocol.and_then(|p| p.trim().parse::<u32>().ok()) != Some(PROTOCOL_VERSION) {
        return Err(protocol_error(StatusCode::BAD_REQUEST, "unsupported_protocol", "Graphrt-Protocol must be 1"));
    }
    let body = axum::body::to_bytes(body, agent.config.max_body_bytes)
        .await
        .map_err(|_| protocol_error(StatusCode::PAYLOAD_TOO_LARGE, "body_too_large", "request body too large"))?;
    let deployment = header_text(&parts.headers, DEPLOYMENT_HEADER).unwrap_or("");
    let run_id = header_text(&parts.headers, RUN_ID_HEADER).unwrap_or("");
    let epoch = header_text(&parts.headers, EPOCH_HEADER).unwrap_or("");
    let path = parts.uri.path_and_query().map_or_else(|| parts.uri.path(), |pq| pq.as_str());
    let signed = Signed { deployment, method: parts.method.as_str(), path, run_id, epoch, body: &body };
    let signature = header_text(&parts.headers, SIGNATURE_HEADER);
    if let Err(e) = agent.verifier.verify(signature, &signed, signature::unix_now()) {
        // Never echo signature material; the reason is enough to fix a clock or key.
        eprintln!("warning: refused a gateway request to {}: {e}", parts.uri.path());
        let status = if e == signature::SignatureError::Busy {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::UNAUTHORIZED
        };
        return Err(protocol_error(status, "bad_signature", e.as_str()));
    }
    if deployment != agent.deployment() {
        return Err(protocol_error(StatusCode::FORBIDDEN, "wrong_deployment", "another deployment"));
    }
    let epoch = if epoch.is_empty() {
        None
    } else {
        Some(epoch.parse::<u64>().map_err(|_| {
            protocol_error(StatusCode::BAD_REQUEST, "bad_request", "Graphrt-Epoch must be a decimal number")
        })?)
    };
    let run_id = (!run_id.is_empty()).then(|| run_id.to_owned());
    Ok(Verified { request: GatewayRequest { run_id, epoch }, parts, body })
}

/// The `/internal/v1` routes, signature-checked.
pub(crate) fn router() -> axum::Router<Arc<AppState>> {
    axum::Router::new().route("/internal/v1/{*endpoint}", axum::routing::any(handle))
}
