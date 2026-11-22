//! Test doubles: a fake agent speaking protocol 1 (signed `/internal/v1/*`,
//! SSE with `id:`s, orphan reports) and a provider whose replicas are fake
//! agents on loopback ports, plus a helper that runs a gateway in-process.

#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::result_large_err,
    clippy::type_complexity
)]

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::StreamExt as _;
use graphrt_core::Snapshot;
use graphrt_core::snapshot::FrameSnapshot;
use graphrt_gateway::auth::DeploymentAuth;
use graphrt_gateway::deployment::{Deployment, VersionInfo};
use graphrt_gateway::pool::PoolConfig;
use graphrt_gateway::server::{App, router};
use graphrt_gateway::signature::{
    DEPLOYMENT_HEADER, EPOCH_HEADER, GatewayKey, RUN_ID_HEADER, SIGNATURE_HEADER, Signed, Verifier, sign, unix_now,
};
use graphrt_gateway::store::SqliteStore;
use graphrt_gateway::supervisor::{Gateway, SupervisorSettings};
use graphrt_provider_api::{
    Addressing, BoxFuture, Capabilities, DeploymentRef, Endpoint, EndpointAddress, Provider, ProviderError,
    ReleaseMode, ReplicaPhase, ReplicaRef, ReplicaState, Scaler,
};
use serde_json::{Value as Json, json};

pub const DEPLOYMENT: &str = "shop";
pub const API_KEY: &str = "grk_test_key";

pub fn key() -> GatewayKey {
    GatewayKey::new("k1", &[42u8; 32]).unwrap()
}

fn snapshot(run_id: &str) -> Snapshot {
    Snapshot {
        format: 1,
        run_id: run_id.into(),
        graph_fingerprint: String::new(),
        elapsed_ms: 0,
        steps: 1,
        usage: Default::default(),
        handled_errors: 0,
        root: FrameSnapshot::default(),
    }
}

/// Shared knobs of all fake agents.
#[derive(Default)]
pub struct AgentWorld {
    /// Where agents report orphans.
    pub gateway_url: Mutex<Option<String>>,
    /// Executions started, per run id.
    pub executions: Mutex<HashMap<String, Vec<u64>>>,
    /// Bodies the agents received (to check that secrets reach the agent but nothing else).
    pub bodies: Mutex<Vec<Json>>,
    pub starts: AtomicU32,
    /// `preempt_after_ms` of every run start.
    pub preempt_after: Mutex<Vec<Option<u64>>>,
}

struct AgentState {
    world: Arc<AgentWorld>,
    verifier: Verifier,
    replica: String,
}

fn frame(event: &str, id: Option<String>, data: &Json) -> Bytes {
    let mut text = format!("event: {event}\n");
    if let Some(id) = id {
        text.push_str(&format!("id: {id}\n"));
    }
    text.push_str(&format!("data: {data}\n\n"));
    Bytes::from(text)
}

fn verify(state: &AgentState, method: &str, path: &str, headers: &HeaderMap, body: &[u8]) -> Result<(), Response> {
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok()).unwrap_or_default().to_owned();
    let (deployment, run_id, epoch) = (header(DEPLOYMENT_HEADER), header(RUN_ID_HEADER), header(EPOCH_HEADER));
    let signed = Signed { deployment: &deployment, method, path, run_id: &run_id, epoch: &epoch, body };
    let signature = header(SIGNATURE_HEADER);
    state
        .verifier
        .verify(Some(&signature), &signed, unix_now())
        .map_err(|error| (StatusCode::UNAUTHORIZED, error.to_string()).into_response())
}

fn orphan_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

async fn post_orphan(world: &AgentWorld, run_id: &str, epoch: u64, terminal: Json, event_count: u64) {
    let Some(url) = world.gateway_url.lock().unwrap().clone() else { return };
    let body = serde_json::to_vec(&json!({ "run_id": run_id, "epoch": epoch, "deployment": DEPLOYMENT, "status": "paused", "event_count": event_count, "terminal": terminal, "answer": "", "orphaned_ms": 0 })).unwrap();
    let path = format!("/internal/v1/orphans/{run_id}");
    let epoch_text = epoch.to_string();
    let signed =
        Signed { deployment: DEPLOYMENT, method: "POST", path: &path, run_id, epoch: &epoch_text, body: &body };
    for _ in 0..50 {
        let response = orphan_client()
            .post(format!("{url}{path}"))
            .header(SIGNATURE_HEADER, sign(&key(), unix_now(), &signed))
            .header(DEPLOYMENT_HEADER, DEPLOYMENT)
            .header(RUN_ID_HEADER, run_id)
            .header(EPOCH_HEADER, &epoch_text)
            .body(body.clone())
            .send()
            .await;
        if response.is_ok_and(|response| response.status().is_success()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn start_run(State(state): State<Arc<AgentState>>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(refused) = verify(&state, "POST", "/internal/v1/runs", &headers, &body) {
        return refused;
    }
    let request: Json = serde_json::from_slice(&body).unwrap();
    state.world.bodies.lock().unwrap().push(request.clone());
    state.world.starts.fetch_add(1, Ordering::SeqCst);
    let run_id = headers.get(RUN_ID_HEADER).unwrap().to_str().unwrap().to_owned();
    let epoch: u64 = headers.get(EPOCH_HEADER).unwrap().to_str().unwrap().parse().unwrap();
    let previous = {
        let mut executions = state.world.executions.lock().unwrap();
        let entry = executions.entry(run_id.clone()).or_default();
        entry.push(epoch);
        entry.len() - 1
    };
    let inputs = request["inputs"].clone();
    let mode = inputs["mode"].as_str().unwrap_or("ok").to_owned();
    let resumed_run = request.get("resume").is_some();
    // `resume_events`: how many events a resumed execution still has to emit.
    let count = match inputs["resume_events"].as_u64() {
        Some(remaining) if resumed_run => remaining,
        _ => inputs["events"].as_u64().unwrap_or(2),
    };
    let preempt_after = request["preempt_after_ms"].as_u64().map(Duration::from_millis);
    state.world.preempt_after.lock().unwrap().push(request["preempt_after_ms"].as_u64());
    let delay = Duration::from_millis(inputs["delay_ms"].as_u64().unwrap_or(0));
    let base = request["event_index"].as_u64().unwrap_or(0);
    let resumed = request.get("resume").is_some();
    let (mut sender, receiver) = futures::channel::mpsc::channel::<Bytes>(16);
    let world = state.world.clone();
    tokio::spawn(async move {
        use futures::SinkExt as _;
        let mut index = base;
        let _ = sender.send(frame("hello", None, &json!({ "run_id": run_id, "epoch": epoch, "protocol": 1 }))).await;
        let mut emit = |event: Json| {
            let id = index;
            index += 1;
            (event["type"].as_str().unwrap().to_owned(), id, event)
        };
        let started = emit(json!({ "type": "run_started", "run_id": run_id, "resumed": resumed }));
        let _ = sender.send(frame(&started.0, Some(started.1.to_string()), &started.2)).await;
        let execution_started = tokio::time::Instant::now();
        for step in 0..count {
            tokio::time::sleep(delay).await;
            // Like `serve --trust-gateway`: preempt itself before the platform's request cap.
            if preempt_after.is_some_and(|after| execution_started.elapsed() >= after) {
                let paused = json!({ "type": "run_paused", "reasons": [{ "frame": "", "node_id": null, "kind": "external", "data": "request_cap" }], "snapshot": snapshot(&run_id) });
                let id = index;
                let _ = sender.send(frame("run_paused", Some(id.to_string()), &paused)).await;
                return;
            }
            let (kind, id, data) = emit(json!({ "type": "node_succeeded", "step": step, "execution": previous }));
            let _ = sender
                .send(frame("response", Some(format!("{id}.0")), &json!({ "text": format!("chunk{step}") })))
                .await;
            if sender.send(frame(&kind, Some(id.to_string()), &data)).await.is_err() {
                return;
            }
        }
        let first_execution = previous == 0 && !resumed;
        let terminal = match mode.as_str() {
            "hitl" if !resumed => {
                json!({ "type": "run_paused", "reasons": [{ "frame": "", "node_id": "ask", "kind": "human_input", "data": null }], "snapshot": snapshot(&run_id) })
            }
            "preempt_once" if first_execution => {
                json!({ "type": "run_paused", "reasons": [{ "frame": "", "node_id": null, "kind": "external", "data": "drained" }], "snapshot": snapshot(&run_id) })
            }
            "break_once" if first_execution => {
                // The stream breaks; the agent preempts and reports the orphan.
                drop(sender);
                let terminal = json!({ "type": "run_paused", "reasons": [{ "frame": "", "node_id": null, "kind": "external", "data": "gateway_disconnected" }], "snapshot": snapshot(&run_id) });
                tokio::time::sleep(Duration::from_millis(100)).await;
                post_orphan(&world, &run_id, epoch, terminal, index).await;
                return;
            }
            "vanish" => {
                drop(sender);
                return;
            }
            "fail" => {
                json!({ "type": "run_failed", "error": "boom", "error_type": "node", "usage": { "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 }, "elapsed_ms": 1 })
            }
            _ => {
                let echo = if resumed {
                    request["resume"]["inputs"].clone()
                } else {
                    inputs.get("text").cloned().unwrap_or(Json::Null)
                };
                json!({ "type": "run_succeeded", "outputs": { "echo": echo, "user": request["user_id"], "executions": previous + 1 }, "usage": { "prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3 }, "handled_errors": 0, "elapsed_ms": 1 })
            }
        };
        let kind = terminal["type"].as_str().unwrap().to_owned();
        let id = index;
        let _ = sender.send(frame(&kind, Some(id.to_string()), &terminal)).await;
    });
    let body = Body::from_stream(receiver.map(Ok::<_, Infallible>));
    ([("content-type", "text/event-stream")], body).into_response()
}

async fn hello(State(state): State<Arc<AgentState>>, headers: HeaderMap) -> Response {
    if let Err(refused) = verify(&state, "GET", "/internal/v1/hello", &headers, b"") {
        return refused;
    }
    axum::Json(json!({ "protocol": 1, "deployment": DEPLOYMENT, "replica": state.replica, "draining": false }))
        .into_response()
}

async fn orphans(State(state): State<Arc<AgentState>>, headers: HeaderMap) -> Response {
    if let Err(refused) = verify(&state, "GET", "/internal/v1/orphans", &headers, b"") {
        return refused;
    }
    axum::Json(json!({ "orphans": [] })).into_response()
}

/// One fake protocol-1 agent on a loopback port.
pub async fn serve_agent(world: Arc<AgentWorld>, id: &str) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let state = Arc::new(AgentState { world, verifier: Verifier::new(vec![key()]), replica: id.to_owned() });
    let app = Router::new()
        .route("/internal/v1/runs", post(start_run))
        .route("/internal/v1/hello", get(hello))
        .route("/internal/v1/orphans", get(orphans))
        // Platform readiness probes (Cloud Run wake) hit the agent's /healthz.
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (address, task)
}

/// A provider whose replicas are fake agents on loopback.
#[derive(Default)]
pub struct FakeAgentProvider {
    pub world: Arc<AgentWorld>,
    replicas: Mutex<HashMap<ReplicaRef, (SocketAddr, tokio::task::JoinHandle<()>)>>,
    next: AtomicU64,
    pub wakes: AtomicU32,
    wake_lock: tokio::sync::Mutex<()>,
}

impl FakeAgentProvider {
    pub fn new(world: Arc<AgentWorld>) -> Self {
        Self { world, ..Self::default() }
    }

    async fn spawn_agent(&self, deployment: &DeploymentRef) -> (ReplicaRef, SocketAddr) {
        let id = format!("agent-{}", self.next.fetch_add(1, Ordering::SeqCst) + 1);
        let (address, task) = serve_agent(self.world.clone(), &id).await;
        let replica = ReplicaRef { deployment: deployment.clone(), id };
        self.replicas.lock().unwrap().insert(replica.clone(), (address, task));
        (replica, address)
    }

    /// Kills one agent (like `kill -9`): connections drop, no orphan report.
    pub fn kill(&self, replica_id: &str) {
        let mut replicas = self.replicas.lock().unwrap();
        let key = replicas.keys().find(|replica| replica.id == replica_id).cloned();
        if let Some(key) = key
            && let Some((_, task)) = replicas.remove(&key)
        {
            task.abort();
        }
    }

    pub fn live(&self) -> usize {
        self.replicas.lock().unwrap().len()
    }
}

impl Provider for FakeAgentProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            addressing: Addressing::Replica,
            scaler: Scaler::Gateway,
            max_request_secs: None,
            idle_timeout_secs: None,
            max_grace_secs: 30,
            background_cpu: true,
            supports_suspend: false,
            wake_p95_target_ms: 50,
        }
    }

    fn wake<'a>(
        &'a self,
        deployment: &'a DeploymentRef,
        replicas: u32,
    ) -> BoxFuture<'a, Result<Vec<Endpoint>, ProviderError>> {
        Box::pin(async move {
            let _serialised = self.wake_lock.lock().await;
            self.wakes.fetch_add(1, Ordering::SeqCst);
            while (self.replicas.lock().unwrap().keys().filter(|replica| replica.deployment == *deployment).count()
                as u32)
                < replicas
            {
                self.spawn_agent(deployment).await;
            }
            Ok(self
                .replicas
                .lock()
                .unwrap()
                .iter()
                .filter(|(replica, _)| replica.deployment == *deployment)
                .map(|(replica, (address, _))| Endpoint {
                    replica: replica.clone(),
                    address: EndpointAddress::Http(format!("http://{address}")),
                })
                .collect())
        })
    }

    fn release<'a>(&'a self, replica: &'a ReplicaRef, _mode: ReleaseMode) -> BoxFuture<'a, Result<(), ProviderError>> {
        if let Some((_, task)) = self.replicas.lock().unwrap().remove(replica) {
            task.abort();
        }
        Box::pin(async { Ok(()) })
    }

    fn list<'a>(&'a self, deployment: &'a DeploymentRef) -> BoxFuture<'a, Result<Vec<ReplicaState>, ProviderError>> {
        let rows = self
            .replicas
            .lock()
            .unwrap()
            .iter()
            .filter(|(replica, _)| replica.deployment == *deployment)
            .map(|(replica, (address, _))| ReplicaState {
                replica: replica.clone(),
                phase: ReplicaPhase::Ready,
                address: Some(EndpointAddress::Http(format!("http://{address}"))),
                started_at_ms: None,
            })
            .collect();
        Box::pin(async move { Ok(rows) })
    }
}

/// A gateway serving on loopback.
pub struct TestGateway {
    pub url: String,
    pub gateway: Arc<Gateway>,
    pub app: Arc<App>,
    pub provider: Arc<FakeAgentProvider>,
    pub deployment: Arc<Deployment>,
    http: reqwest::Client,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TestGateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub fn version(version: &str) -> VersionInfo {
    VersionInfo {
        version: version.into(),
        workflow: "agent".into(),
        fingerprint: String::new(),
        target: "fake".into(),
        command: Vec::new(),
        activated_ms: 0,
        superseded_ms: None,
    }
}

pub struct Options {
    pub store: SqliteStore,
    pub world: Arc<AgentWorld>,
    pub pool: PoolConfig,
    pub reclaim_grace: Duration,
    pub configure_app: Option<Box<dyn FnOnce(&mut App) + Send>>,
    /// Run the deployment on this provider instead of the fake-agent provider.
    pub provider: Option<Arc<dyn Provider>>,
    /// `DeploymentRef::target` of version v1.
    pub target: String,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            store: SqliteStore::in_memory().unwrap(),
            world: Arc::default(),
            pool: PoolConfig::default(),
            reclaim_grace: Duration::from_secs(5),
            configure_app: None,
            provider: None,
            target: "fake".into(),
        }
    }
}

pub async fn start_gateway(options: Options) -> TestGateway {
    graphrt_gateway::install_crypto_provider();
    let settings = SupervisorSettings {
        reclaim_grace: options.reclaim_grace,
        reattach_window: Duration::from_millis(200),
        ..SupervisorSettings::default()
    };
    let gateway = Gateway::new(options.store, settings);
    let provider = Arc::new(FakeAgentProvider::new(options.world.clone()));
    let chosen: Arc<dyn Provider> = options.provider.clone().unwrap_or_else(|| provider.clone());
    let auth =
        DeploymentAuth::new(vec![graphrt_gateway::hex::sha256_hex(API_KEY.as_bytes())], None, &reqwest::Client::new())
            .unwrap();
    let deployment = Arc::new(Deployment::new(DEPLOYMENT, auth, vec![key()], chosen, options.pool));
    let mut first = version("v1");
    first.target = options.target.clone();
    deployment.add_version(first, graphrt_core::store::now_ms());
    gateway.add_deployment(deployment.clone());
    let mut app = App::new(gateway.clone());
    if let Some(configure) = options.configure_app {
        configure(&mut app);
    }
    let app = Arc::new(app);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    *options.world.gateway_url.lock().unwrap() = Some(url.clone());
    let service = router(app.clone()).into_make_service_with_connect_info::<SocketAddr>();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, service).await;
    });
    TestGateway { url, gateway, app, provider, deployment, http: reqwest::Client::new(), task }
}

impl TestGateway {
    pub fn client(&self) -> reqwest::Client {
        self.http.clone()
    }

    pub async fn post(&self, path: &str, body: Json) -> (u16, Json) {
        let response = self
            .client()
            .post(format!("{}{path}", self.url))
            .header("x-api-key", API_KEY)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Json::String(text)))
    }

    pub async fn get(&self, path: &str) -> (u16, Json) {
        let response =
            self.client().get(format!("{}{path}", self.url)).header("x-api-key", API_KEY).send().await.unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Json::Null))
    }

    /// Reads an SSE response fully: (id, event, data) triples.
    pub async fn sse(
        &self,
        path: &str,
        body: Option<Json>,
        last_event_id: Option<&str>,
    ) -> Vec<(Option<String>, String, Json)> {
        let mut request = match &body {
            Some(body) => self.client().post(format!("{}{path}", self.url)).json(body),
            None => self.client().get(format!("{}{path}", self.url)),
        }
        .header("x-api-key", API_KEY);
        if let Some(id) = last_event_id {
            request = request.header("last-event-id", id);
        }
        let text = request.send().await.unwrap().text().await.unwrap();
        let mut parser = graphrt_gateway::sse::Parser::default();
        parser
            .feed(text.as_bytes())
            .unwrap()
            .into_iter()
            .map(|frame| (frame.id, frame.event, serde_json::from_str(&frame.data).unwrap_or(Json::Null)))
            .collect()
    }

    /// Polls a run until it leaves `running` / `queued`.
    pub async fn wait_settled(&self, run_id: &str) -> Json {
        for _ in 0..400 {
            let (_, record) = self.get(&format!("/v1/deployments/{DEPLOYMENT}/runs/{run_id}")).await;
            if !matches!(record["status"].as_str(), Some("running" | "queued") | None) {
                return record;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("run {run_id} did not settle");
    }
}
