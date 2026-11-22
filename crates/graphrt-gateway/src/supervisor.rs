//! Run supervisor: the gateway holds the stream of every run and is the only
//! writer of the store (plan §2, §3.4).
//!
//! ```text
//! create(queued, epoch 0)          record = idempotency marker
//!   → lease a replica (activator)  in memory
//!   → claim_dispatch (CAS epoch+1) before the push
//!   → POST /internal/v1/runs        signed, SSE back
//!   → events: broadcast to clients + batch to the store (32 events / 250 ms)
//!   ├ terminal       → put_if(final) + outbox callback in one transaction
//!   ├ paused(hitl)   → snapshot + put_if(paused); inputs stay in memory until the token expires
//!   ├ paused(external: drained / preempted / gateway_disconnected)
//!   │                → re-dispatch at once with the snapshot and the in-memory inputs
//!   └ stream broke   → Replica: reattach ≤ 3 s; else poll the agents' orphans
//!                      (`GET /internal/v1/orphans`, backoff to 2 s) ≤ reclaim_grace
//!                      → re-dispatch / finish
//!                      Url (no instance to pull from), or no orphan: re-dispatch from the
//!                      latest stored snapshot, else from the request with the same run id
//!                      (nodes may run again: at-least-once, `Idempotency-Key`);
//!                      failed{owner_lost} only when that is impossible, with the reason
//! ```
//!
//! Request bodies (which carry secret inputs and `user_jwt`) live in
//! [`LiveRun`] memory; the store keeps a copy without them for a restart
//! ([`StoredRequest`]), and no secret reaches the store or logs (decision 7, I6).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use graphrt_core::Event;
use graphrt_core::store::{RunRecord, RunStatus, RunStore, StoreError, StoredEvent, now_ms};
use graphrt_provider_api::Addressing;
use serde_json::Value as Json;
use tokio::sync::{broadcast, oneshot, watch};
use tokio::time::Instant;

use crate::agent_client::{AgentClient, AgentError, AgentStream, RunScope};
use crate::deployment::{Deployment, VersionEntry, VersionError};
use crate::metrics::{Metrics, bump};
use crate::pool::{ActivateError, Lease};
use crate::sse::Frame;
use crate::store::SqliteStore;
use crate::stored_request::{CREDENTIALS_REQUIRED, StoredRequest};

/// Timing knobs of the supervisor.
#[derive(Debug, Clone)]
pub struct SupervisorSettings {
    /// How long an orphan is polled for after a lost stream (plan: 60 s).
    pub reclaim_grace: Duration,
    /// How long a broken `Replica` stream may be reattached (plan: 3 s).
    pub reattach_window: Duration,
    /// Events per store batch.
    pub batch_events: usize,
    /// Longest an event waits before its batch is written.
    pub batch_interval: Duration,
    /// Replicas tried for one dispatch.
    pub max_dispatch_attempts: u32,
    /// Upper bound of one run (I1).
    pub run_timeout: Duration,
    /// Frames kept in memory per run for `Last-Event-ID` replay.
    pub replay_ring: usize,
    /// Executions in a row a run may lose (executor gone, no outcome) before it
    /// fails with `owner_lost`: a run that kills its executor every time must
    /// not be re-dispatched forever.
    pub max_lost_executions: u32,
}

impl Default for SupervisorSettings {
    fn default() -> Self {
        Self {
            reclaim_grace: Duration::from_secs(60),
            reattach_window: Duration::from_secs(3),
            batch_events: 32,
            batch_interval: Duration::from_millis(250),
            max_dispatch_attempts: 3,
            run_timeout: Duration::from_secs(24 * 3600),
            replay_ring: 512,
            max_lost_executions: 3,
        }
    }
}

/// A run request as the client sent it (`inputs`, `sys`, `conversation`,
/// `metadata`, `session_id`, `user_id`, `tags`). In memory; the store keeps
/// only a copy without its secrets ([`StoredRequest`]).
#[derive(Debug, Clone, Default)]
pub struct RunInput {
    /// The JSON object.
    pub body: serde_json::Map<String, Json>,
    /// When the credentials inside expire (JWT `exp`, unix seconds): the
    /// in-memory copy of a paused run is dropped then.
    pub expires_at: Option<u64>,
}

/// What a run settles as.
#[derive(Debug, Clone, PartialEq)]
pub enum Settled {
    /// Still executing.
    Running,
    /// Terminal or paused record.
    Done(Box<RunRecord>),
}

/// The in-memory side of a run the gateway is executing.
pub struct LiveRun {
    /// Run id.
    pub run_id: String,
    /// Deployment.
    pub deployment: Arc<Deployment>,
    frames: broadcast::Sender<Arc<Frame>>,
    ring: Mutex<VecDeque<Arc<Frame>>>,
    ring_capacity: usize,
    settled: watch::Sender<Settled>,
    input: Mutex<Option<RunInput>>,
    orphan: Mutex<Option<oneshot::Sender<Json>>>,
    current: Mutex<Option<(graphrt_provider_api::EndpointAddress, u64)>>,
    callback: Option<String>,
}

impl std::fmt::Debug for LiveRun {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("LiveRun").field("run_id", &self.run_id).finish_non_exhaustive()
    }
}

impl LiveRun {
    fn new(
        run_id: &str,
        deployment: Arc<Deployment>,
        input: RunInput,
        callback: Option<String>,
        ring_capacity: usize,
    ) -> Arc<Self> {
        let (frames, _) = broadcast::channel(1024);
        let (settled, _) = watch::channel(Settled::Running);
        Arc::new(Self {
            run_id: run_id.to_owned(),
            deployment,
            frames,
            ring: Mutex::new(VecDeque::with_capacity(ring_capacity.min(64))),
            ring_capacity,
            settled,
            input: Mutex::new(Some(input)),
            orphan: Mutex::new(None),
            current: Mutex::new(None),
            callback,
        })
    }

    fn publish(&self, frame: Frame) {
        let frame = Arc::new(frame);
        {
            let mut ring = self.ring.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if ring.len() >= self.ring_capacity {
                ring.pop_front();
            }
            ring.push_back(frame.clone());
        }
        let _no_subscribers = self.frames.send(frame);
    }

    /// Subscribes to new frames and returns the buffered ones (oldest first).
    /// Subscribe before reading the ring, so nothing falls between them.
    pub fn subscribe(&self) -> (Vec<Arc<Frame>>, broadcast::Receiver<Arc<Frame>>) {
        let receiver = self.frames.subscribe();
        let buffered = self.ring.lock().unwrap_or_else(std::sync::PoisonError::into_inner).iter().cloned().collect();
        (buffered, receiver)
    }

    /// Lowest engine event index still in the ring.
    pub fn ring_start(&self) -> Option<u64> {
        self.ring.lock().unwrap_or_else(std::sync::PoisonError::into_inner).iter().find_map(|frame| frame.event_index())
    }

    /// Watches the settled state.
    pub fn settled(&self) -> watch::Receiver<Settled> {
        self.settled.subscribe()
    }

    fn input(&self) -> Option<RunInput> {
        self.input.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    fn set_input(&self, input: Option<RunInput>) {
        *self.input.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = input;
    }

    fn expect_orphan(&self) -> oneshot::Receiver<Json> {
        let (sender, receiver) = oneshot::channel();
        *self.orphan.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sender);
        receiver
    }

    /// Stops waiting for an orphan (it came, or the run continues without it).
    fn stop_awaiting_orphan(&self) {
        drop(self.orphan.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take());
    }

    /// Whether a supervisor waits for this run's orphan.
    fn awaits_orphan(&self) -> bool {
        self.orphan.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_some()
    }

    /// Hands a pulled orphan to the waiting supervisor. Returns whether one was waiting.
    pub fn deliver_orphan(&self, orphan: Json) -> bool {
        match self.orphan.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take() {
            Some(sender) => sender.send(orphan).is_ok(),
            None => false,
        }
    }
}

/// Why a run could not be started.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StartError {
    /// No such deployment / version.
    #[error(transparent)]
    Version(#[from] VersionError),
    /// No replica could take it.
    #[error(transparent)]
    Activate(#[from] ActivateError),
    /// The store refused.
    #[error("store: {0}")]
    Store(String),
    /// The run is not in a state that allows the operation.
    #[error("{0}")]
    Conflict(String),
    /// Unknown run.
    #[error("run not found")]
    NotFound,
}

impl From<StoreError> for StartError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::NotFound(_) => Self::NotFound,
            StoreError::Conflict(message) | StoreError::Precondition(message) => Self::Conflict(message),
            other => Self::Store(other.to_string()),
        }
    }
}

/// Result of [`Gateway::start_run`].
#[derive(Debug, Clone)]
pub enum Started {
    /// A new run.
    New(Arc<LiveRun>),
    /// The run id existed already (idempotency key, webhook, cron): its record.
    Duplicate(Box<RunRecord>),
}

/// How a new run is started.
#[derive(Debug, Clone, Default)]
pub struct StartOptions {
    /// Deterministic run id (idempotency key, webhook, cron); random otherwise.
    pub run_id: Option<String>,
    /// Lease a replica before the record is created and fail fast with 503
    /// (interactive requests); otherwise the record is created first and the
    /// supervisor waits for capacity (webhooks, cron).
    pub interactive: bool,
    /// Completion callback URL (webhooks).
    pub callback: Option<String>,
    /// Record metadata (no secrets).
    pub metadata: Vec<(String, String)>,
}

/// The gateway's run-time core.
pub struct Gateway {
    /// T1 store.
    pub store: SqliteStore,
    /// Timing knobs.
    pub settings: SupervisorSettings,
    /// Agent client.
    pub agents: AgentClient,
    /// Counters.
    pub metrics: Metrics,
    deployments: RwLock<HashMap<String, Arc<Deployment>>>,
    live: Mutex<HashMap<String, Arc<LiveRun>>>,
}

impl std::fmt::Debug for Gateway {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Gateway").finish_non_exhaustive()
    }
}

/// A random run id: `run-` + 32 hex digits from the system RNG.
pub fn new_run_id() -> String {
    use ring::rand::SecureRandom as _;
    let mut bytes = [0u8; 16];
    let _fallback_is_timestamp = ring::rand::SystemRandom::new().fill(&mut bytes);
    if bytes == [0; 16] {
        bytes[..8].copy_from_slice(&now_ms().to_be_bytes());
    }
    format!("run-{}", crate::hex::encode(&bytes))
}

fn event_type(data: &str) -> Option<String> {
    serde_json::from_str::<Json>(data).ok()?.get("type")?.as_str().map(str::to_owned)
}

fn is_terminal_type(kind: &str) -> bool {
    matches!(kind, "run_succeeded" | "run_failed" | "run_aborted" | "run_paused")
}

/// The stored form of an agent event: the JSON as sent, without the inline
/// snapshot of `run_paused` (stores keep it separately).
fn stored_event(data: &str) -> StoredEvent {
    let mut value: Json = serde_json::from_str(data).unwrap_or(Json::Null);
    if let Some(object) = value.as_object_mut()
        && object.get("type").and_then(Json::as_str) == Some("run_paused")
    {
        object.remove("snapshot");
    }
    serde_json::value::RawValue::from_string(value.to_string())
        .unwrap_or_else(|_| serde_json::value::RawValue::NULL.to_owned())
}

/// Milliseconds after which an execution must be preempted on a platform
/// with a request cap and a grace period: `cap - min(60 s, cap / 4)`
/// (3600 s → 3540 s, 240 s → 180 s). `None` without a cap, or when the
/// platform gives no grace (Lambda: it fails with `platform_timeout` instead).
pub fn planned_preemption_ms(capabilities: &graphrt_provider_api::Capabilities) -> Option<u64> {
    let cap_ms = capabilities.max_request_secs?.saturating_mul(1000);
    if !capabilities.can_preempt() || cap_ms == 0 {
        return None;
    }
    Some(cap_ms - (cap_ms / 4).min(60_000))
}

/// What ended one execution.
enum Outcome {
    /// A terminal or paused event (the full event, snapshot included).
    Finished(Box<Event>),
    /// Stream ended without a terminal event.
    Broken(String),
    /// The platform ended the request at its limit; no preemption or re-dispatch.
    PlatformTimeout(String),
}

/// The terminal event of a pulled orphan, if it has a valid one.
fn orphan_terminal(orphan: Option<&Json>) -> Option<Event> {
    serde_json::from_value(orphan?.get("terminal")?.clone()).ok()
}

/// Whether every pause reason is external (drain, preemption, lost gateway):
/// such a run is re-dispatched at once instead of waiting for a human.
fn is_external_pause(event: &Event) -> bool {
    match event {
        Event::RunPaused { reasons, .. } => {
            !reasons.is_empty()
                && reasons.iter().all(|reason| reason.kind == "external" || reason.kind == "gateway_disconnected")
        }
        _ => false,
    }
}

impl Gateway {
    /// A gateway over `store`.
    pub fn new(store: SqliteStore, settings: SupervisorSettings) -> Arc<Self> {
        Arc::new(Self {
            store,
            settings,
            agents: AgentClient::default(),
            metrics: Metrics::default(),
            deployments: RwLock::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
        })
    }

    /// Registers a deployment.
    pub fn add_deployment(&self, deployment: Arc<Deployment>) {
        self.deployments
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(deployment.name.clone(), deployment);
    }

    /// A deployment by name.
    pub fn deployment(&self, name: &str) -> Option<Arc<Deployment>> {
        self.deployments.read().unwrap_or_else(std::sync::PoisonError::into_inner).get(name).cloned()
    }

    /// All deployments.
    pub fn deployments(&self) -> Vec<Arc<Deployment>> {
        self.deployments.read().unwrap_or_else(std::sync::PoisonError::into_inner).values().cloned().collect()
    }

    /// A run the gateway is executing (or settled moments ago).
    pub fn live_run(&self, run_id: &str) -> Option<Arc<LiveRun>> {
        self.live.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(run_id).cloned()
    }

    /// Runs held in memory.
    pub fn live_count(&self) -> usize {
        self.live.lock().unwrap_or_else(std::sync::PoisonError::into_inner).len()
    }

    fn register(&self, run: &Arc<LiveRun>) {
        self.live.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(run.run_id.clone(), run.clone());
    }

    /// Settles a run and drops it from memory after a linger (late `Last-Event-ID`
    /// reconnects replay from the ring meanwhile, then from the store).
    fn settle(self: &Arc<Self>, run: &Arc<LiveRun>, record: RunRecord) {
        let keep_input = record.status == RunStatus::Paused;
        if !keep_input {
            run.set_input(None);
        }
        run.settled.send_replace(Settled::Done(Box::new(record)));
        let gateway = Arc::clone(self);
        let run = Arc::clone(run);
        graphrt_core::task::spawn(async move {
            if keep_input {
                // A paused run keeps its inputs (credentials) in memory until they expire.
                let expires_at = run.input().and_then(|input| input.expires_at).unwrap_or(0);
                let wait = expires_at.saturating_sub(now_ms() / 1000).min(7 * 24 * 3600);
                tokio::time::sleep(Duration::from_secs(wait.max(30))).await;
                run.set_input(None);
            } else {
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            let mut live = gateway.live.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if live.get(&run.run_id).is_some_and(|current| Arc::ptr_eq(current, &run))
                && !matches!(*run.settled.borrow(), Settled::Running)
            {
                live.remove(&run.run_id);
            }
        });
    }

    /// Starts a run of the current version of `deployment`.
    ///
    /// # Errors
    /// See [`StartError`].
    pub async fn start_run(
        self: &Arc<Self>,
        deployment: &Arc<Deployment>,
        input: RunInput,
        options: StartOptions,
    ) -> Result<Started, StartError> {
        let version = deployment.current()?;
        let run_id = options.run_id.clone().unwrap_or_else(new_run_id);
        graphrt_core::store::validate_run_id(&run_id).map_err(|error| StartError::Conflict(error.to_string()))?;
        // Only a caller-chosen id (webhook, cron, Idempotency-Key) can already
        // exist; a fresh random one skips the read (one store round trip less
        // before the first byte). `create` still refuses a duplicate.
        if options.run_id.is_some()
            && let Some(existing) = self.store.get(&run_id).await?
        {
            return Ok(Started::Duplicate(Box::new(existing)));
        }
        let lease = if options.interactive { Some(version.pool.acquire().await?) } else { None };
        let mut record = RunRecord::queued(
            &run_id,
            &version.info.workflow,
            &version.info.fingerprint,
            Some(deployment.name.clone()),
            now_ms(),
        );
        record.deployment_version = Some(version.info.version.clone());
        for (key, value) in options.metadata {
            record.metadata.insert(key, value);
        }
        if input.body.contains_key("user_id")
            && let Some(user) = input.body.get("user_id").and_then(Json::as_str)
        {
            record.user_id = Some(user.to_owned());
        }
        // Written with the record (one transaction), without its secrets: a
        // restart re-dispatches a run that has no checkpoint from it.
        let header_inputs: Vec<&str> = deployment.input_headers.iter().map(|mapping| mapping.input.as_str()).collect();
        let stored = StoredRequest::strip(&input, version.info.secret_inputs.as_deref(), &header_inputs);
        // With a replica already leased (interactive runs), the record is created
        // claimed for it: the same state `claim_dispatch` would write next, in the
        // create transaction instead of a second one before the first byte.
        let claimed = lease.is_some();
        if let Some(lease) = &lease {
            record.status = RunStatus::Running;
            record.epoch = record.epoch.saturating_add(1);
            record.owner = Some(lease.endpoint.replica.id.clone());
        }
        match self.store.create_with_request(record.clone(), serde_json::to_string(&stored).ok()).await {
            Ok(()) => {}
            Err(StoreError::Conflict(_)) => {
                let existing = self.store.get(&run_id).await?.ok_or(StartError::NotFound)?;
                return Ok(Started::Duplicate(Box::new(existing)));
            }
            Err(error) => {
                bump(&self.metrics.store_write_errors);
                return Err(error.into());
            }
        }
        bump(&self.metrics.runs_started);
        let run = LiveRun::new(&run_id, deployment.clone(), input, options.callback, self.settings.replay_ring);
        self.register(&run);
        let gateway = Arc::clone(self);
        let supervised = run.clone();
        if claimed {
            graphrt_core::task::spawn(async move {
                gateway.supervise_claimed(supervised, version, record, lease, None).await;
            });
        } else {
            graphrt_core::task::spawn(async move { gateway.supervise(supervised, version, record, lease, None).await });
        }
        Ok(Started::New(run))
    }

    /// Resumes a paused (HITL) run with the human's `inputs` and re-supplied
    /// secrets (merged over the in-memory ones).
    ///
    /// # Errors
    /// `NotFound`, `Conflict` (not paused / already claimed), `Version(Retired)`.
    pub async fn resume_run(
        self: &Arc<Self>,
        deployment: &Arc<Deployment>,
        run_id: &str,
        inputs: Json,
        secrets: serde_json::Map<String, Json>,
        input: Option<RunInput>,
    ) -> Result<Arc<LiveRun>, StartError> {
        let record = self.store.get(run_id).await?.ok_or(StartError::NotFound)?;
        if record.deployment.as_deref() != Some(deployment.name.as_str()) {
            return Err(StartError::NotFound);
        }
        if record.status != RunStatus::Paused {
            return Err(StartError::Conflict(format!("run is {:?}, not paused", record.status)));
        }
        let version_name = record.deployment_version.clone().unwrap_or_default();
        let version = deployment.version(&version_name, now_ms())?;
        let snapshot = self
            .store
            .load_snapshot(run_id)
            .await?
            .ok_or_else(|| StartError::Conflict("paused run has no snapshot".to_owned()))?;
        let lease = version.pool.acquire().await?;
        // Claim before answering, so a second resume of the same pause is refused
        // and no reader sees the run still paused after a 202.
        let record =
            self.store.claim_dispatch(run_id, record.epoch, Some(lease.endpoint.replica.id.clone()), now_ms()).await?;
        let previous = self.live_run(run_id).and_then(|run| run.input());
        let run_input = input.or(previous).unwrap_or_default();
        let run = LiveRun::new(run_id, deployment.clone(), run_input, None, self.settings.replay_ring);
        self.register(&run);
        let resume = serde_json::json!({ "snapshot": snapshot, "inputs": inputs, "secrets": secrets });
        let gateway = Arc::clone(self);
        let supervised = run.clone();
        graphrt_core::task::spawn(async move {
            gateway.supervise_claimed(supervised, version, record, Some(lease), Some(resume)).await
        });
        Ok(run)
    }

    /// Aborts a run: the executing agent is asked to abort; a queued or
    /// paused run is marked aborted directly.
    ///
    /// # Errors
    /// `NotFound`, `Conflict` for finished runs.
    pub async fn abort_run(self: &Arc<Self>, deployment: &Deployment, run_id: &str) -> Result<RunRecord, StartError> {
        let mut record = self.store.get(run_id).await?.ok_or(StartError::NotFound)?;
        if record.deployment.as_deref() != Some(deployment.name.as_str()) {
            return Err(StartError::NotFound);
        }
        if record.status.is_terminal() {
            return Err(StartError::Conflict("run already finished".to_owned()));
        }
        let current = self
            .live_run(run_id)
            .and_then(|run| run.current.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone());
        if record.status == RunStatus::Running
            && let Some((address, epoch)) = current
            && let Some(key) = deployment.signing_key()
        {
            self.agents
                .abort(&address, key, &deployment.name, RunScope { run_id, epoch: Some(epoch) })
                .await
                .map_err(|error| StartError::Conflict(error.to_string()))?;
            return Ok(record);
        }
        let epoch = record.epoch;
        record.status = RunStatus::Aborted;
        record.error = Some("aborted by client".to_owned());
        record.updated_ms = now_ms();
        self.store.put_if(record.clone(), epoch).await?;
        Ok(record)
    }

    fn dispatch_body(
        &self,
        version: &VersionEntry,
        record: &RunRecord,
        input: Option<&RunInput>,
        resume: Option<&Json>,
    ) -> Json {
        let mut body = input.map(|input| input.body.clone()).unwrap_or_default();
        body.remove("stream");
        body.remove("async");
        body.insert("workflow".into(), Json::from(version.info.workflow.clone()));
        if !version.info.fingerprint.is_empty() {
            body.insert("fingerprint".into(), Json::from(version.info.fingerprint.clone()));
        }
        body.insert("deployment_version".into(), Json::from(version.info.version.clone()));
        // Planned preemption before the platform's request cap (Cloud Run 60 min,
        // ACA standard ingress 240 s): the agent pauses the run itself in time
        // and the gateway re-dispatches it, instead of the platform killing it.
        if let Some(after_ms) = planned_preemption_ms(version.pool.capabilities()) {
            body.insert("preempt_after_ms".into(), Json::from(after_ms));
        }
        body.insert("event_index".into(), Json::from(record.event_count));
        if let Some(resume) = resume {
            let mut resume = resume.clone();
            // Inputs of the original request re-supply secret inputs after a migration.
            if let (Some(object), Some(inputs)) =
                (resume.as_object_mut(), input.and_then(|input| input.body.get("inputs")).and_then(Json::as_object))
            {
                let secrets = object.entry("secrets").or_insert_with(|| Json::Object(Default::default()));
                if let Some(secrets) = secrets.as_object_mut() {
                    for (name, value) in inputs {
                        if let Some(text) = value.as_str() {
                            secrets.entry(format!("inputs.{name}")).or_insert_with(|| Json::from(text));
                        }
                    }
                }
            }
            body.insert("resume".into(), resume);
        }
        Json::Object(body)
    }

    /// Writes buffered events, retrying transient failures (never dropping
    /// them silently: a final failure is counted and logged).
    async fn flush(&self, run_id: &str, buffer: &mut Vec<(u64, StoredEvent)>) -> Option<u64> {
        let first = buffer.first().map(|(index, _)| *index)?;
        let events: Vec<StoredEvent> = buffer.drain(..).map(|(_, event)| event).collect();
        for attempt in 0..3u32 {
            match self.store.append_events(run_id, first, &events).await {
                Ok(count) => return Some(count),
                Err(error) if attempt == 2 => {
                    bump(&self.metrics.store_write_errors);
                    eprintln!("gateway: run {run_id}: storing {} events failed: {error}", events.len());
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(50 << attempt)).await,
            }
        }
        None
    }

    /// Pumps one agent stream until a terminal event or a break.
    async fn pump(&self, run: &LiveRun, stream: &mut AgentStream, record: &mut RunRecord) -> Outcome {
        let mut buffer: Vec<(u64, StoredEvent)> = Vec::new();
        let mut flush_at: Option<Instant> = None;
        loop {
            let next = match flush_at {
                Some(deadline) => tokio::select! {
                    frame = stream.next_frame() => Some(frame),
                    () = tokio::time::sleep_until(deadline) => None,
                },
                None => Some(stream.next_frame().await),
            };
            let frame = match next {
                None => {
                    if let Some(count) = self.flush(&run.run_id, &mut buffer).await {
                        record.event_count = count;
                    }
                    flush_at = None;
                    continue;
                }
                Some(Ok(Some(frame))) => frame,
                Some(Ok(None)) => break,
                Some(Err(error)) => {
                    if let Some(count) = self.flush(&run.run_id, &mut buffer).await {
                        record.event_count = count;
                    }
                    return match error {
                        AgentError::PlatformTimeout(message) => Outcome::PlatformTimeout(message),
                        other => Outcome::Broken(other.to_string()),
                    };
                }
            };
            if frame.event == "hello" || frame.event == "lagged" {
                continue;
            }
            let Some(index) = frame.event_index() else {
                run.publish(frame);
                continue;
            };
            if index < record.event_count + buffer.len() as u64 {
                // A reattached stream repeats events we already hold.
                continue;
            }
            let kind = event_type(&frame.data).unwrap_or_default();
            let terminal = is_terminal_type(&kind).then(|| serde_json::from_str::<Event>(&frame.data).ok()).flatten();
            buffer.push((index, stored_event(&frame.data)));
            let public = if kind == "run_paused" {
                Frame { data: stored_event(&frame.data).get().to_owned(), ..frame }
            } else {
                frame
            };
            run.publish(public);
            if let Some(event) = terminal {
                if let Some(count) = self.flush(&run.run_id, &mut buffer).await {
                    record.event_count = count;
                }
                return Outcome::Finished(Box::new(event));
            }
            if buffer.len() >= self.settings.batch_events {
                if let Some(count) = self.flush(&run.run_id, &mut buffer).await {
                    record.event_count = count;
                }
                flush_at = None;
            } else if flush_at.is_none() {
                flush_at = Some(Instant::now() + self.settings.batch_interval);
            }
        }
        if let Some(count) = self.flush(&run.run_id, &mut buffer).await {
            record.event_count = count;
        }
        Outcome::Broken("agent closed the stream without a terminal event".to_owned())
    }

    /// Opens the stream of `record` at its current epoch on `lease`'s replica,
    /// moving to another replica when one is unreachable or draining.
    async fn open_stream(
        &self,
        run: &LiveRun,
        version: &VersionEntry,
        record: &RunRecord,
        lease: &mut Option<Lease>,
        resume: Option<&Json>,
    ) -> Result<AgentStream, (&'static str, String)> {
        let deployment = &run.deployment;
        let key = deployment.signing_key().ok_or(("dispatch_failed", "deployment has no signing key".to_owned()))?;
        let mut last_error = String::new();
        // An instance may still hold the lost execution of the previous epoch
        // until its reattach window passed and it preempted it (a platform that
        // routes the re-dispatch to the same instance): it answers
        // `409 epoch_conflict` meanwhile.
        let conflict_deadline = Instant::now() + self.settings.reattach_window + Duration::from_secs(10);
        let mut attempts = 0;
        while attempts < self.settings.max_dispatch_attempts {
            attempts += 1;
            let current = match lease.take() {
                Some(current) => current,
                None => version.pool.acquire().await.map_err(|error| ("dispatch_failed", error.to_string()))?,
            };
            let body = self.dispatch_body(version, record, run.input().as_ref(), resume);
            let scope = RunScope { run_id: &run.run_id, epoch: Some(record.epoch) };
            match self
                .agents
                .start_run_on(deployment.provider.as_ref(), &current.endpoint, key, &deployment.name, scope, &body)
                .await
            {
                Ok(stream) => {
                    *run.current.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some((current.endpoint.address.clone(), record.epoch));
                    *lease = Some(current);
                    return Ok(stream);
                }
                Err(AgentError::Unreachable(error)) => {
                    last_error = error;
                    self.agents.forget(&current.endpoint.address);
                    current.release_broken();
                }
                Err(AgentError::Refused { status: 503, body }) => last_error = body,
                Err(AgentError::Refused { status: 409, body })
                    if body.contains("epoch_conflict") && Instant::now() < conflict_deadline =>
                {
                    attempts -= 1;
                    last_error = body;
                    *lease = Some(current);
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(AgentError::PlatformTimeout(message)) => return Err(("platform_timeout", message)),
                Err(error) => return Err(("dispatch_failed", error.to_string())),
            }
        }
        Err(("dispatch_failed", format!("no replica accepted the run: {last_error}")))
    }

    async fn fail(self: &Arc<Self>, run: &Arc<LiveRun>, mut record: RunRecord, error_type: &str, message: &str) {
        let epoch = record.epoch;
        record.status = RunStatus::Failed;
        record.error = Some(message.to_owned());
        record.error_type = Some(error_type.to_owned());
        record.updated_ms = now_ms();
        if let Err(error) = self.store.put_if(record.clone(), epoch).await {
            bump(&self.metrics.store_write_errors);
            eprintln!("gateway: run {}: storing failure: {error}", run.run_id);
        }
        bump(&self.metrics.runs_finished);
        self.settle(run, record);
    }

    async fn finish(self: &Arc<Self>, run: &Arc<LiveRun>, mut record: RunRecord, event: &Event) -> Option<Json> {
        let epoch = record.epoch;
        let snapshot = record.apply_terminal(event, now_ms());
        if let Some(snapshot) = &snapshot
            && let Err(error) = self.store.save_snapshot(&run.run_id, snapshot).await
        {
            bump(&self.metrics.store_write_errors);
            eprintln!("gateway: run {}: storing snapshot: {error}", run.run_id);
        }
        let migrate = record.status == RunStatus::Paused && is_external_pause(event);
        if migrate {
            // Not visible as paused: the run is queued for its next execution, and a
            // restart re-dispatches it from the stored snapshot.
            record.status = RunStatus::Queued;
        }
        let callback = run.callback.clone().filter(|_| record.status.is_terminal()).map(|url| {
            let body = serde_json::json!({
                "run_id": record.run_id, "deployment": record.deployment, "status": record.status,
                "outputs": record.outputs, "error": record.error, "error_type": record.error_type,
            });
            (url, body.to_string())
        });
        if let Err(error) = self.store.finish_with_callback(record.clone(), epoch, callback, now_ms()).await {
            bump(&self.metrics.store_write_errors);
            eprintln!("gateway: run {}: storing final status: {error}", run.run_id);
        }
        if migrate {
            return snapshot.map(|snapshot| serde_json::json!({ "snapshot": snapshot }));
        }
        if record.status == RunStatus::Paused {
            bump(&self.metrics.runs_paused);
        } else {
            bump(&self.metrics.runs_finished);
        }
        self.settle(run, record);
        None
    }

    /// Waits for the run's orphan (pulled by [`Self::keep_polling_orphans`]).
    async fn await_orphan(&self, run: &LiveRun, receiver: oneshot::Receiver<Json>, epoch: u64) -> Option<Json> {
        let orphan = tokio::time::timeout(self.settings.reclaim_grace, receiver).await.ok()?.ok()?;
        (orphan.get("epoch").and_then(Json::as_u64) == Some(epoch)).then_some(orphan).filter(|_| !run.run_id.is_empty())
    }

    /// Drives `record` through executions until it settles.
    async fn supervise(
        self: Arc<Self>,
        run: Arc<LiveRun>,
        version: VersionEntry,
        record: RunRecord,
        lease: Option<Lease>,
        resume: Option<Json>,
    ) {
        self.supervise_inner(run, version, record, lease, resume, false).await;
    }

    /// [`Self::supervise`] for a record already claimed at its current epoch.
    async fn supervise_claimed(
        self: Arc<Self>,
        run: Arc<LiveRun>,
        version: VersionEntry,
        record: RunRecord,
        lease: Option<Lease>,
        resume: Option<Json>,
    ) {
        self.supervise_inner(run, version, record, lease, resume, true).await;
    }

    async fn supervise_inner(
        self: Arc<Self>,
        run: Arc<LiveRun>,
        version: VersionEntry,
        record: RunRecord,
        lease: Option<Lease>,
        resume: Option<Json>,
        claimed: bool,
    ) {
        let deadline = Instant::now() + self.settings.run_timeout;
        match tokio::time::timeout_at(
            deadline,
            self.clone().executions(run.clone(), version, record.clone(), lease, resume, claimed),
        )
        .await
        {
            Ok(()) => {}
            Err(_) => {
                let current = self.store.get(&run.run_id).await.ok().flatten().unwrap_or(record);
                self.fail(&run, current, "run_timeout", "run exceeded the gateway run timeout").await;
            }
        }
    }

    async fn executions(
        self: Arc<Self>,
        run: Arc<LiveRun>,
        version: VersionEntry,
        mut record: RunRecord,
        mut lease: Option<Lease>,
        mut resume: Option<Json>,
        mut claimed: bool,
    ) {
        let mut lost = 0u32;
        loop {
            let owner = lease.as_ref().map(|lease| lease.endpoint.replica.id.clone());
            let claim = if std::mem::take(&mut claimed) {
                Ok(record.clone())
            } else {
                self.store.claim_dispatch(&run.run_id, record.epoch, owner, now_ms()).await
            };
            record = match claim {
                Ok(claimed) => claimed,
                Err(error) => {
                    eprintln!("gateway: run {}: dispatch claim refused: {error}", run.run_id);
                    let current = self.store.get(&run.run_id).await.ok().flatten();
                    run.settled.send_replace(Settled::Done(Box::new(current.unwrap_or(record))));
                    return;
                }
            };
            let mut stream = match self.open_stream(&run, &version, &record, &mut lease, resume.as_ref()).await {
                Ok(stream) => stream,
                Err((error_type, message)) => return self.fail(&run, record, error_type, &message).await,
            };
            let capabilities = version.pool.capabilities().clone();
            let request_cap =
                capabilities.max_request_secs.filter(|_| !capabilities.can_preempt()).map(Duration::from_secs);
            let outcome = loop {
                let pumped = match request_cap {
                    // No grace, no preemption (Lambda): the platform kills the run at its cap.
                    Some(cap) => match tokio::time::timeout(cap, self.pump(&run, &mut stream, &mut record)).await {
                        Ok(outcome) => outcome,
                        Err(_) => {
                            return self
                                .fail(&run, record, "platform_timeout", "run exceeded the platform's request limit")
                                .await;
                        }
                    },
                    None => self.pump(&run, &mut stream, &mut record).await,
                };
                match pumped {
                    Outcome::Finished(event) => break Outcome::Finished(event),
                    Outcome::PlatformTimeout(message) => break Outcome::PlatformTimeout(message),
                    Outcome::Broken(reason) => match self.reattach(&run, &version, &record, lease.as_ref()).await {
                        Some(reattached) => stream = reattached,
                        None => break Outcome::Broken(reason),
                    },
                }
            };
            match outcome {
                Outcome::Finished(event) => match self.finish(&run, record.clone(), &event).await {
                    Some(next) => {
                        lost = 0;
                        bump(&self.metrics.redispatches);
                        record = self.store.get(&run.run_id).await.ok().flatten().unwrap_or(record);
                        resume = Some(next);
                        drop(lease.take());
                    }
                    None => return,
                },
                // Lambda-like platforms end the request themselves: no preemption
                // happened, so there is nothing to re-dispatch from.
                Outcome::PlatformTimeout(message) => {
                    return self
                        .fail(&run, record, "platform_timeout", &format!("platform ended the run: {message}"))
                        .await;
                }
                Outcome::Broken(reason) => {
                    if let Some(broken) = lease.take() {
                        self.agents.forget(&broken.endpoint.address);
                        broken.release_broken();
                    }
                    let terminal = if capabilities.addressing == Addressing::Url {
                        // No instance can be addressed to pull an orphan from.
                        None
                    } else {
                        let receiver = run.expect_orphan();
                        // Agents never call the gateway: pull the orphan from them.
                        graphrt_core::task::spawn(Arc::clone(&self).keep_polling_orphans(vec![run.run_id.clone()]));
                        let orphan = self.await_orphan(&run, receiver, record.epoch).await;
                        run.stop_awaiting_orphan();
                        orphan_terminal(orphan.as_ref())
                    };
                    let next = match terminal {
                        Some(event) => {
                            lost = 0;
                            let Some(next) = self.finish(&run, record.clone(), &event).await else { return };
                            bump(&self.metrics.redispatches);
                            record = self.store.get(&run.run_id).await.ok().flatten().unwrap_or(record);
                            Some(next)
                        }
                        None => {
                            lost += 1;
                            let Some((requeued, next)) = self.requeue(&run, record, &reason, lost).await else {
                                return;
                            };
                            record = requeued;
                            next
                        }
                    };
                    resume = next;
                }
            }
        }
    }

    /// Where a run whose executor was lost without an outcome continues: from
    /// the latest stored snapshot, else from its request (same run id, next
    /// epoch; after a restart the stored one). `Err` says why it cannot: its
    /// credentials expired, or it needs secret inputs that are never stored.
    async fn checkpoint(&self, run: &LiveRun) -> Result<Option<Json>, String> {
        if run.input().is_none() {
            run.set_input(self.restored_input(&run.run_id).await?);
        }
        let input = run.input();
        if let Some(expires_at) = input.as_ref().and_then(|input| input.expires_at)
            && expires_at <= now_ms() / 1000
        {
            return Err("the request's credentials expired; resume it with fresh ones".to_owned());
        }
        match self.store.load_snapshot(&run.run_id).await {
            Ok(Some(snapshot)) => Ok(Some(serde_json::json!({ "snapshot": snapshot }))),
            Ok(None) if input.is_some() => Ok(None),
            Ok(None) => Err("it has no checkpoint and no stored request (accepted by an older gateway)".to_owned()),
            Err(error) => Err(format!("its checkpoint cannot be read: {error}")),
        }
    }

    /// The request stored with a run, for a gateway that restarted. `Ok(None)`:
    /// none was stored (an older gateway accepted the run). `Err`
    /// (`credentials_required`): it needs secret inputs, which are never stored.
    async fn restored_input(&self, run_id: &str) -> Result<Option<RunInput>, String> {
        let text = match self.store.load_request(run_id).await {
            Ok(Some(text)) => text,
            Ok(None) => return Ok(None),
            Err(error) => return Err(format!("its stored request cannot be read: {error}")),
        };
        let stored: StoredRequest =
            serde_json::from_str(&text).map_err(|error| format!("its stored request is corrupt: {error}"))?;
        if !stored.needs_secrets.is_empty() {
            return Err(format!(
                "{CREDENTIALS_REQUIRED}: it needs the secret inputs {} and secrets are never stored; submit it again",
                stored.needs_secrets.join(", ")
            ));
        }
        Ok(Some(stored.into_input()))
    }

    /// Queues a run whose executor was lost (the `lost`-th time in a row) for
    /// re-dispatch from [`Self::checkpoint`], and returns the queued record and
    /// the resume body (`None`: start again from the request). Nodes that ran
    /// may run again: at-least-once per node, keyed by `Idempotency-Key`. Fails
    /// the run with `owner_lost` and the reason when it cannot continue.
    async fn requeue(
        self: &Arc<Self>,
        run: &Arc<LiveRun>,
        mut record: RunRecord,
        reason: &str,
        lost: u32,
    ) -> Option<(RunRecord, Option<Json>)> {
        if lost > self.settings.max_lost_executions {
            let message = format!("executor lost {lost} times in a row (last: {reason})");
            self.fail(run, record, "owner_lost", &message).await;
            return None;
        }
        let resume = match self.checkpoint(run).await {
            Ok(resume) => resume,
            Err(why) => {
                let message = format!("executor lost ({reason}) and the run cannot be re-dispatched: {why}");
                self.fail(run, record, "owner_lost", &message).await;
                return None;
            }
        };
        let epoch = record.epoch;
        record.status = RunStatus::Queued;
        record.owner = None;
        record.updated_ms = now_ms();
        if let Err(error) = self.store.put_if(record.clone(), epoch).await {
            // Another writer moved the run (a client abort): leave it to that.
            eprintln!("gateway: run {}: re-queue refused: {error}", run.run_id);
            let current = self.store.get(&run.run_id).await.ok().flatten().unwrap_or(record);
            run.settled.send_replace(Settled::Done(Box::new(current)));
            return None;
        }
        bump(&self.metrics.redispatches);
        bump(&self.metrics.lost_executions);
        Some((record, resume))
    }

    /// Reattaches a broken `Replica` stream within the reattach window.
    async fn reattach(
        &self,
        run: &LiveRun,
        version: &VersionEntry,
        record: &RunRecord,
        lease: Option<&Lease>,
    ) -> Option<AgentStream> {
        if version.pool.capabilities().addressing != Addressing::Replica {
            return None;
        }
        let lease = lease?;
        let key = run.deployment.signing_key()?;
        let deadline = Instant::now() + self.settings.reattach_window;
        while Instant::now() < deadline {
            let scope = RunScope { run_id: &run.run_id, epoch: Some(record.epoch) };
            match self
                .agents
                .reattach(&lease.endpoint.address, key, &run.deployment.name, scope, record.event_count)
                .await
            {
                Ok(stream) => return Some(stream),
                Err(AgentError::Refused { .. }) => return None,
                Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
            }
        }
        None
    }

    /// Accepts an orphan pulled from an agent (`GET /internal/v1/orphans`).
    /// Returns whether a supervisor was waiting for it.
    pub fn accept_orphan(&self, run_id: &str, orphan: Json) -> bool {
        self.live_run(run_id).is_some_and(|run| run.deliver_orphan(orphan))
    }

    /// After a restart: re-dispatches queued runs and waits for the orphans of
    /// runs that were executing (agents report them, or are polled here), so
    /// ≥ 99 % of active runs are reclaimed (plan §9). Returns `(queued, running)`.
    ///
    /// # Errors
    /// Store errors.
    pub async fn recover(self: &Arc<Self>) -> Result<(usize, usize), StoreError> {
        let queued = self.store.runs_with_status(RunStatus::Queued).await?;
        let running = self.store.runs_with_status(RunStatus::Running).await?;
        for record in &queued {
            let Some(deployment) = record.deployment.as_deref().and_then(|name| self.deployment(name)) else {
                continue;
            };
            let Ok(version) = deployment.version(record.deployment_version.as_deref().unwrap_or_default(), now_ms())
            else {
                continue;
            };
            let run =
                LiveRun::new(&record.run_id, deployment.clone(), RunInput::default(), None, self.settings.replay_ring);
            self.register(&run);
            // Its in-memory request died with the previous gateway (decision 7).
            match self.restored_input(&record.run_id).await {
                Ok(input) => run.set_input(input),
                Err(why) => {
                    let message = format!("the gateway restarted before the run was dispatched: {why}");
                    self.fail(&run, record.clone(), "owner_lost", &message).await;
                    continue;
                }
            }
            // A queued run with a snapshot was migrating when the gateway stopped.
            let resume = self
                .store
                .load_snapshot(&record.run_id)
                .await?
                .map(|snapshot| serde_json::json!({ "snapshot": snapshot }));
            graphrt_core::task::spawn(Arc::clone(self).supervise(run, version, record.clone(), None, resume));
        }
        let mut waiting = Vec::new();
        for record in &running {
            let Some(deployment) = record.deployment.as_deref().and_then(|name| self.deployment(name)) else {
                continue;
            };
            let run =
                LiveRun::new(&record.run_id, deployment.clone(), RunInput::default(), None, self.settings.replay_ring);
            // The request inputs died with the previous gateway (decision 7):
            // a re-dispatch reads the stored request without its secrets.
            run.set_input(None);
            let receiver = run.expect_orphan();
            self.register(&run);
            waiting.push(record.run_id.clone());
            let gateway = Arc::clone(self);
            let record = record.clone();
            graphrt_core::task::spawn(async move { gateway.reclaim(run, record, receiver).await });
        }
        self.poll_orphans(&waiting).await;
        graphrt_core::task::spawn(Arc::clone(self).keep_polling_orphans(waiting));
        Ok((queued.len(), running.len()))
    }

    /// Polls the agents for the orphans of `waiting` with backoff (100 ms
    /// doubling to 2 s) until each is taken or the reclaim grace ends. Used after
    /// a lost stream (the agent turns the run into an orphan once its reattach
    /// window passed) and after a restart (an agent notices the lost stream only
    /// some milliseconds after the gateway died, and an active/passive takeover
    /// takes tens of milliseconds).
    async fn keep_polling_orphans(self: Arc<Self>, mut waiting: Vec<String>) {
        let deadline = tokio::time::Instant::now() + self.settings.reclaim_grace;
        let mut delay = Duration::from_millis(100);
        loop {
            waiting.retain(|run_id| self.live_run(run_id).is_some_and(|run| run.awaits_orphan()));
            if waiting.is_empty() || tokio::time::Instant::now() + delay >= deadline {
                return;
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(2));
            self.poll_orphans(&waiting).await;
        }
    }

    async fn reclaim(self: Arc<Self>, run: Arc<LiveRun>, record: RunRecord, receiver: oneshot::Receiver<Json>) {
        let Ok(version) = run.deployment.version(record.deployment_version.as_deref().unwrap_or_default(), now_ms())
        else {
            return self.fail(&run, record, "version_retired", "deployment version retired").await;
        };
        let terminal = if version.pool.capabilities().addressing == Addressing::Url {
            // No instance can be addressed to pull an orphan from.
            None
        } else {
            orphan_terminal(self.await_orphan(&run, receiver, record.epoch).await.as_ref())
        };
        run.stop_awaiting_orphan();
        let (current, next) = match terminal {
            Some(event) => {
                let Some(next) = self.finish(&run, record.clone(), &event).await else { return };
                bump(&self.metrics.redispatches);
                (self.store.get(&run.run_id).await.ok().flatten().unwrap_or(record), Some(next))
            }
            None => {
                let Some(requeued) = self.requeue(&run, record, "gateway restarted", 1).await else { return };
                requeued
            }
        };
        self.supervise(run, version, current, None, next).await;
    }

    /// Asks every listed replica of every deployment for orphans of `run_ids`.
    async fn poll_orphans(&self, run_ids: &[String]) {
        if run_ids.is_empty() {
            return;
        }
        for deployment in self.deployments() {
            let Some(key) = deployment.signing_key() else { continue };
            for version in deployment.versions() {
                let Ok(replicas) = deployment.provider.list(&version.info.reference(&deployment.name)).await else {
                    continue;
                };
                for replica in replicas.into_iter().filter_map(|state| state.address) {
                    let Ok(orphans) = self.agents.orphans(&replica, key, &deployment.name).await else { continue };
                    for orphan in orphans {
                        let Some(run_id) = orphan.get("run_id").and_then(Json::as_str).map(str::to_owned) else {
                            continue;
                        };
                        if run_ids.contains(&run_id) && self.accept_orphan(&run_id, orphan.clone()) {
                            let epoch = orphan.get("epoch").and_then(Json::as_u64);
                            let _ = self
                                .agents
                                .release_orphan(&replica, key, &deployment.name, RunScope { run_id: &run_id, epoch })
                                .await;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod preemption_tests {
    use graphrt_provider_api::{Addressing, Capabilities, Scaler};

    fn caps(max_request_secs: Option<u64>, max_grace_secs: u64) -> Capabilities {
        Capabilities {
            addressing: Addressing::Url,
            scaler: Scaler::Platform,
            max_request_secs,
            idle_timeout_secs: None,
            max_grace_secs,
            background_cpu: true,
            supports_suspend: false,
            wake_p95_target_ms: 1000,
        }
    }

    #[test]
    fn preempts_with_a_margin_before_the_cap() {
        assert_eq!(super::planned_preemption_ms(&caps(Some(3600), 10)), Some(3_540_000));
        assert_eq!(super::planned_preemption_ms(&caps(Some(240), 120)), Some(180_000));
        assert_eq!(super::planned_preemption_ms(&caps(Some(2), 10)), Some(1_500));
        assert_eq!(super::planned_preemption_ms(&caps(None, 30)), None);
        assert_eq!(super::planned_preemption_ms(&caps(Some(840), 0)), None, "no grace: platform_timeout instead");
    }
}
