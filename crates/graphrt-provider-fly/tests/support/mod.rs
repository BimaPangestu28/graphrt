//! A stateful fake of the Fly Machines API (the endpoints the provider and the
//! deploy driver use), served by the conformance harness's `FakeServer` so the
//! harness can inject 401/429/5xx/capacity faults in front of it.
//!
//! State transitions are immediate except boot: `start` moves a Machine to
//! `starting`, and the first `/wait?state=started` completes the boot, which is
//! how a real start looks to the client.

#![allow(dead_code, clippy::expect_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use graphrt_provider_api::conformance::{FakeResponse, FakeServer, RecordedRequest};
use serde_json::{Value, json};

pub const APP: &str = "chat-agents";
pub const TOKEN: &str = "fm2_conformance_token_value";
pub const SUSPEND_MEMORY_LIMIT_MB: u64 = 2048;

/// One fake Machine.
#[derive(Clone, Debug)]
pub struct FakeMachine {
    pub id: String,
    pub name: String,
    pub state: String,
    pub region: String,
    pub private_ip: String,
    pub config: Value,
}

impl FakeMachine {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id, "name": self.name, "state": self.state, "region": self.region,
            "private_ip": self.private_ip, "instance_id": format!("01H{}", self.id), "config": self.config,
        })
    }

    fn memory_mb(&self) -> u64 {
        self.config.pointer("/guest/memory_mb").and_then(Value::as_u64).unwrap_or(256)
    }

    fn metadata(&self, key: &str) -> Option<&str> {
        self.config.get("metadata").and_then(|metadata| metadata.get(key)).and_then(Value::as_str)
    }
}

/// Everything the fake knows.
#[derive(Debug, Default)]
pub struct FakeFlyState {
    pub apps: BTreeMap<String, String>,
    pub machines: Vec<FakeMachine>,
    pub secrets: BTreeMap<String, String>,
    /// Machine ids whose host is full: `start` answers 409 with flyd's phrase.
    pub full_hosts: Vec<String>,
    /// Calls per `METHOD /path-suffix` (e.g. `POST suspend`).
    pub calls: BTreeMap<String, u32>,
    next_id: u32,
}

impl FakeFlyState {
    pub fn machine(&self, id: &str) -> Option<&FakeMachine> {
        self.machines.iter().find(|machine| machine.id == id)
    }

    pub fn count(&self, call: &str) -> u32 {
        self.calls.get(call).copied().unwrap_or(0)
    }

    pub fn states_of(&self, version: &str) -> Vec<String> {
        self.machines
            .iter()
            .filter(|machine| machine.metadata("graphrt_version") == Some(version))
            .map(|machine| machine.state.clone())
            .collect()
    }

    fn add_machine(&mut self, name: String, region: String, config: Value, state: &str) -> FakeMachine {
        self.next_id += 1;
        let machine = FakeMachine {
            id: format!("e286{:04x}", self.next_id),
            name,
            state: state.to_owned(),
            region: if region.is_empty() { "ams".to_owned() } else { region },
            private_ip: format!("fdaa:0:1:a7b:{:x}::2", self.next_id),
            config,
        };
        self.machines.push(machine.clone());
        machine
    }
}

/// The fake API plus the server it runs on.
pub struct FakeFly {
    pub server: FakeServer,
    pub state: Arc<Mutex<FakeFlyState>>,
}

impl FakeFly {
    /// Starts a fake with `APP` existing (no Machines yet).
    pub async fn start() -> Self {
        let state = Arc::new(Mutex::new(FakeFlyState::default()));
        lock(&state).apps.insert(APP.to_owned(), "personal".to_owned());
        let handler_state = state.clone();
        let server = FakeServer::start(Arc::new(move |request: &RecordedRequest| handle(&handler_state, request)))
            .await
            .expect("bind fake Fly API");
        Self { server, state }
    }

    pub fn state(&self) -> MutexGuard<'_, FakeFlyState> {
        lock(&self.state)
    }

    /// Adds a pool Machine of `deployment`/`version` in `state` with `memory_mb`.
    pub fn seed_machine(&self, deployment: &str, version: &str, index: u32, state: &str, memory_mb: u32) -> String {
        let config = json!({
            "image": "ghcr.io/example/agent:1",
            "guest": {"cpu_kind": "shared", "cpus": 1, "memory_mb": memory_mb},
            "metadata": {"graphrt_pool": "1", "graphrt_deployment": deployment, "graphrt_version": version},
            "stop_config": {"signal": "SIGTERM", "timeout": "300s"},
            "restart": {"policy": "no"},
            "auto_destroy": false,
        });
        self.state().add_machine(format!("{deployment}-{version}-{index}"), "ams".to_owned(), config, state).id
    }
}

fn lock(state: &Mutex<FakeFlyState>) -> MutexGuard<'_, FakeFlyState> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

fn error(status: u16, message: &str) -> FakeResponse {
    FakeResponse::json(status, &json!({ "error": message }))
}

fn handle(state: &Mutex<FakeFlyState>, request: &RecordedRequest) -> FakeResponse {
    let expected_header = format!("FlyV1 {TOKEN}");
    if request.header("authorization") != Some(expected_header.as_str()) {
        return error(401, "unauthorized");
    }
    let (path, query) = request.path.split_once('?').unwrap_or((request.path.as_str(), ""));
    let segments: Vec<&str> = path.trim_start_matches("/v1/").split('/').collect();
    let mut state = lock(state);
    let call_name = format!("{} {}", request.method, segments.get(4).or(segments.get(2)).copied().unwrap_or("app"));
    *state.calls.entry(call_name).or_default() += 1;
    let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
    match (request.method.as_str(), segments.as_slice()) {
        ("POST", ["apps"]) => create_app(&mut state, &body),
        ("GET", ["apps", app]) => {
            if state.apps.contains_key(*app) {
                FakeResponse::json(200, &json!({"name": app}))
            } else {
                error(404, "app not found")
            }
        }
        (_, ["apps", app, ..]) if !state.apps.contains_key(*app) => error(404, "app not found"),
        ("GET", ["apps", _, "machines"]) => list_machines(&state, query),
        ("POST", ["apps", _, "machines"]) => create_machine(&mut state, &body),
        ("GET", ["apps", _, "machines", id]) => {
            with_machine(&mut state, id, |machine| FakeResponse::json(200, &machine.to_json()))
        }
        ("DELETE", ["apps", _, "machines", id]) => destroy_machine(&mut state, id),
        ("POST", ["apps", _, "machines", id, "start"]) => start_machine(&mut state, id),
        ("POST", ["apps", _, "machines", id, "suspend"]) => suspend_machine(&mut state, id),
        ("POST", ["apps", _, "machines", id, "stop"]) => with_machine(&mut state, id, |machine| {
            machine.state = "stopped".to_owned();
            FakeResponse::json(200, &json!({"ok": true}))
        }),
        ("GET", ["apps", _, "machines", id, "wait"]) => wait_machine(&mut state, id, query),
        ("GET", ["apps", _, "secrets"]) => {
            let names: Vec<Value> = state.secrets.keys().map(|name| json!({"name": name, "digest": "d"})).collect();
            FakeResponse::json(200, &json!({ "secrets": names }))
        }
        ("POST", ["apps", _, "secrets"]) => update_secrets(&mut state, &body),
        _ => error(404, "no such route in the fake"),
    }
}

fn create_app(state: &mut FakeFlyState, body: &Value) -> FakeResponse {
    let name = body["app_name"].as_str().unwrap_or_default().to_owned();
    state.apps.insert(name, body["org_slug"].as_str().unwrap_or_default().to_owned());
    FakeResponse::json(201, &json!({}))
}

fn list_machines(state: &FakeFlyState, query: &str) -> FakeResponse {
    let filters: Vec<(String, String)> = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter_map(|(key, value)| key.strip_prefix("metadata.").map(|key| (key.to_owned(), percent_decode(value))))
        .collect();
    let machines: Vec<Value> = state
        .machines
        .iter()
        .filter(|machine| filters.iter().all(|(key, value)| machine.metadata(key) == Some(value.as_str())))
        .map(FakeMachine::to_json)
        .collect();
    FakeResponse::json(200, &Value::Array(machines))
}

fn create_machine(state: &mut FakeFlyState, body: &Value) -> FakeResponse {
    let name = body["name"].as_str().unwrap_or_default().to_owned();
    if state.machines.iter().any(|machine| machine.name == name) {
        return FakeResponse::json(
            422,
            &json!({"error": "a machine with this name already exists", "status": "name_taken"}),
        );
    }
    let initial_state = if body["skip_launch"].as_bool() == Some(true) { "created" } else { "starting" };
    let machine = state.add_machine(
        name,
        body["region"].as_str().unwrap_or_default().to_owned(),
        body["config"].clone(),
        initial_state,
    );
    FakeResponse::json(200, &machine.to_json())
}

fn with_machine(
    state: &mut FakeFlyState,
    id: &str,
    action: impl FnOnce(&mut FakeMachine) -> FakeResponse,
) -> FakeResponse {
    match state.machines.iter_mut().find(|machine| machine.id == id) {
        Some(machine) => action(machine),
        None => error(404, "machine not found"),
    }
}

fn destroy_machine(state: &mut FakeFlyState, id: &str) -> FakeResponse {
    let before = state.machines.len();
    state.machines.retain(|machine| machine.id != id);
    if state.machines.len() < before {
        FakeResponse::json(200, &json!({"ok": true}))
    } else {
        error(404, "machine not found")
    }
}

fn start_machine(state: &mut FakeFlyState, id: &str) -> FakeResponse {
    if state.full_hosts.iter().any(|full| full == id) {
        return error(409, "could not reserve resource for machine: insufficient memory available to fulfill request");
    }
    with_machine(state, id, |machine| match machine.state.as_str() {
        "created" | "stopped" | "suspended" => {
            machine.state = "starting".to_owned();
            FakeResponse::json(200, &json!({"previous_state": "stopped"}))
        }
        _ => error(412, "machine is not in a startable state"),
    })
}

fn suspend_machine(state: &mut FakeFlyState, id: &str) -> FakeResponse {
    with_machine(state, id, |machine| {
        if machine.memory_mb() > SUSPEND_MEMORY_LIMIT_MB {
            return error(400, "suspend is not supported for machines with more than 2GB of memory");
        }
        if machine.state != "started" {
            return error(412, "machine must be started to suspend");
        }
        machine.state = "suspended".to_owned();
        FakeResponse::json(200, &json!({"ok": true}))
    })
}

fn wait_machine(state: &mut FakeFlyState, id: &str, query: &str) -> FakeResponse {
    let target_state = query.split('&').find_map(|pair| pair.strip_prefix("state=")).unwrap_or("started").to_owned();
    with_machine(state, id, |machine| {
        if machine.state == "starting" && target_state == "started" {
            machine.state = "started".to_owned();
        }
        if machine.state == target_state {
            FakeResponse::json(200, &json!({"ok": true}))
        } else {
            error(408, "timeout waiting for state")
        }
    })
}

fn update_secrets(state: &mut FakeFlyState, body: &Value) -> FakeResponse {
    if let Some(values) = body["values"].as_object() {
        for (name, value) in values {
            match value.as_str() {
                Some(value) => state.secrets.insert(name.clone(), value.to_owned()),
                None => state.secrets.remove(name),
            };
        }
    }
    FakeResponse::json(200, &json!({"version": 1}))
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&text[index + 1..index + 3], 16)
        {
            decoded.push(byte);
            index += 3;
            continue;
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// Installs the rustls crypto provider reqwest needs (the binary does it in `main`).
pub fn install_crypto_provider() {
    let _already_installed = rustls::crypto::ring::default_provider().install_default();
}
