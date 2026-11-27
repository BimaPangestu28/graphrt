//! A stateful fake of the Fly Machines API (the endpoints the provider, the
//! deploy driver and the direct deployer use), served by the conformance
//! harness's `FakeServer` so tests can inject 401/429/5xx/capacity faults in
//! front of it. Feature `fake`: for tests (this crate's and the CLI's), never
//! in a binary.
//!
//! State transitions are immediate except boot: `start` (or a create, or an
//! update of a started Machine, without `skip_launch`) moves a Machine to `starting`, and the first
//! `/wait?state=started` completes the boot, which is how a real start looks
//! to the client. A started Machine with `config.checks` reports each check as
//! `passing`, or `critical` while [`FakeFlyState::failing_checks`] is set.

#![allow(clippy::expect_used, clippy::indexing_slicing, missing_docs)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use graphrt_provider_api::conformance::RecordedRequest;
// Re-exported so other crates can inject faults without depending on the harness.
pub use graphrt_provider_api::conformance::{FakeResponse, FakeServer, Fault};
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
    fn to_json(&self, failing_checks: bool) -> Value {
        json!({
            "id": self.id, "name": self.name, "state": self.state, "region": self.region,
            "private_ip": self.private_ip, "instance_id": format!("01H{}", self.id), "config": self.config,
            "checks": self.check_statuses(failing_checks),
        })
    }

    /// What Fly reports for the Machine's `config.checks` (only once started).
    fn check_statuses(&self, failing_checks: bool) -> Vec<Value> {
        let Some(checks) = self.config.get("checks").and_then(Value::as_object) else { return Vec::new() };
        if self.state != "started" {
            return Vec::new();
        }
        let status = if failing_checks { "critical" } else { "passing" };
        let output = if failing_checks { "HTTP GET /readyz: 503 Service Unavailable" } else { "200 OK" };
        checks.keys().map(|name| json!({"name": name, "status": status, "output": output})).collect()
    }

    fn memory_mb(&self) -> u64 {
        self.config.pointer("/guest/memory_mb").and_then(Value::as_u64).unwrap_or(256)
    }

    pub fn metadata(&self, key: &str) -> Option<&str> {
        self.config.get("metadata").and_then(|metadata| metadata.get(key)).and_then(Value::as_str)
    }
}

/// One fake volume.
#[derive(Clone, Debug)]
pub struct FakeVolume {
    pub id: String,
    pub name: String,
    pub region: String,
    pub size_gb: u64,
    /// The Machine whose config mounts it.
    pub attached_machine_id: Option<String>,
}

impl FakeVolume {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id, "name": self.name, "state": "created", "region": self.region, "size_gb": self.size_gb,
            "encrypted": true, "attached_machine_id": self.attached_machine_id,
        })
    }
}

/// The volume ids a Machine config mounts.
fn mounted_volumes(config: &Value) -> Vec<String> {
    config
        .get("mounts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|mount| mount.get("volume").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

/// Everything the fake knows.
#[derive(Debug, Default)]
pub struct FakeFlyState {
    pub apps: BTreeMap<String, String>,
    pub machines: Vec<FakeMachine>,
    /// Volumes of the app. Like Fly, a volume is mounted by at most one
    /// Machine, in its region, and only a new Machine can mount one.
    pub volumes: Vec<FakeVolume>,
    pub secrets: BTreeMap<String, String>,
    /// Version of the app's secrets, bumped by every secrets update.
    pub secrets_version: u64,
    /// Public IPs assigned to each app (`shared_v4`, `v6`).
    pub ips: BTreeMap<String, Vec<String>>,
    /// Started Machines report their checks as `critical` instead of `passing`.
    pub failing_checks: bool,
    /// Secrets updates answer 400 with the request body echoed back (leak tests).
    pub echo_secrets_on_error: bool,
    /// App names some other organization holds: creating them fails.
    pub taken_app_names: Vec<String>,
    /// Machine ids whose host is full: `start` answers 409 with flyd's phrase.
    pub full_hosts: Vec<String>,
    /// Machines the organization may have, every app counted (Fly's Free plan: 3).
    /// Creating one more answers 422 with Fly's machine limit message.
    pub machine_limit: Option<usize>,
    /// Like Fly: after an update with `skip_launch`, the next this many reads of the
    /// Machine (get and list) still show its previous config, in state `replacing`.
    pub replacing_reads: u32,
    /// Machines still `replacing`: reads left and the config they show meanwhile.
    replacing: BTreeMap<String, (u32, Value)>,
    /// Calls per `METHOD /path-suffix` (e.g. `POST suspend`).
    pub calls: BTreeMap<String, u32>,
    /// Files on each volume (by volume id), written through `exec` of
    /// `graphrt internal store-file`, by path relative to `/data/agents`.
    pub volume_files: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    /// Every `exec` command, in order.
    pub exec_commands: Vec<Vec<String>>,
    next_id: u32,
}

impl FakeFlyState {
    pub fn machine(&self, id: &str) -> Option<&FakeMachine> {
        self.machines.iter().find(|machine| machine.id == id)
    }

    pub fn count(&self, call: &str) -> u32 {
        self.calls.get(call).copied().unwrap_or(0)
    }

    pub fn volume_named(&self, name: &str) -> Option<&FakeVolume> {
        self.volumes.iter().find(|volume| volume.name == name)
    }

    pub fn machine_named(&self, name: &str) -> Option<&FakeMachine> {
        self.machines.iter().find(|machine| machine.name == name)
    }

    pub fn states_of(&self, version: &str) -> Vec<String> {
        self.machines
            .iter()
            .filter(|machine| machine.metadata("graphrt_version") == Some(version))
            .map(|machine| machine.state.clone())
            .collect()
    }

    pub fn add_machine(&mut self, name: String, region: String, config: Value, state: &str) -> FakeMachine {
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
#[derive(Debug)]
pub struct FakeFly {
    pub server: FakeServer,
    pub state: Arc<Mutex<FakeFlyState>>,
}

impl FakeFly {
    /// Starts a fake with `APP` existing (no Machines yet).
    pub async fn start() -> Self {
        let fake = Self::start_empty().await;
        fake.state().apps.insert(APP.to_owned(), "personal".to_owned());
        fake
    }

    /// Starts a fake without any app.
    pub async fn start_empty() -> Self {
        let state = Arc::new(Mutex::new(FakeFlyState::default()));
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
        ("DELETE", ["apps", app]) => delete_app(&mut state, app),
        (_, ["apps", app, ..]) if !state.apps.contains_key(*app) => error(404, "app not found"),
        ("GET", ["apps", app, "ip_assignments"]) => {
            let ips: Vec<Value> = state
                .ips
                .get(*app)
                .into_iter()
                .flatten()
                .map(|ip| json!({"ip": ip, "shared": !ip.contains(':'), "region": "global"}))
                .collect();
            FakeResponse::json(200, &json!({ "ips": ips }))
        }
        ("POST", ["apps", app, "ip_assignments"]) => assign_ip(&mut state, app, &body),
        ("GET", ["apps", _, "machines"]) => list_machines(&mut state, query),
        ("POST", ["apps", _, "machines"]) => create_machine(&mut state, &body),
        ("GET", ["apps", _, "machines", id]) => match read_machine(&mut state, id) {
            Some(machine) => FakeResponse::json(200, &machine),
            None => error(404, "machine not found"),
        },
        ("POST", ["apps", _, "machines", id]) => update_machine(&mut state, id, &body),
        ("POST", ["apps", _, "machines", id, "metadata", key]) => {
            let value = body["value"].clone();
            let key = percent_decode(key);
            with_machine(&mut state, id, |machine| {
                if let Some(metadata) = machine.config.get_mut("metadata").and_then(Value::as_object_mut) {
                    metadata.insert(key, value);
                } else if let Some(config) = machine.config.as_object_mut() {
                    config.insert("metadata".to_owned(), json!({ key: value }));
                }
                FakeResponse::status(204)
            })
        }
        ("DELETE", ["apps", _, "machines", id]) => destroy_machine(&mut state, id),
        ("POST", ["apps", _, "machines", id, "start"]) => start_machine(&mut state, id),
        ("POST", ["apps", _, "machines", id, "suspend"]) => suspend_machine(&mut state, id),
        ("POST", ["apps", _, "machines", id, "stop"]) => with_machine(&mut state, id, |machine| {
            machine.state = "stopped".to_owned();
            FakeResponse::json(200, &json!({"ok": true}))
        }),
        ("GET", ["apps", _, "machines", id, "wait"]) => wait_machine(&mut state, id, query),
        ("GET", ["apps", _, "volumes"]) => {
            FakeResponse::json(200, &Value::Array(state.volumes.iter().map(FakeVolume::to_json).collect()))
        }
        ("POST", ["apps", _, "volumes"]) => create_volume(&mut state, &body),
        ("DELETE", ["apps", _, "volumes", id]) => delete_volume(&mut state, id),
        ("PUT", ["apps", _, "volumes", id, "extend"]) => extend_volume(&mut state, id, &body),
        ("POST", ["apps", _, "machines", id, "exec"]) => exec_machine(&mut state, id, &body),
        ("GET", ["apps", _, "secrets"]) => {
            let names: Vec<Value> = state.secrets.keys().map(|name| json!({"name": name, "digest": "d"})).collect();
            FakeResponse::json(200, &json!({ "secrets": names }))
        }
        ("POST", ["apps", _, "secrets"]) => update_secrets(&mut state, &body, &request.body_text()),
        _ => error(404, "no such route in the fake"),
    }
}

fn create_app(state: &mut FakeFlyState, body: &Value) -> FakeResponse {
    let name = body["app_name"].as_str().unwrap_or_default().to_owned();
    if state.taken_app_names.contains(&name) {
        return FakeResponse::json(422, &json!({"error": "Name has already been taken"}));
    }
    state.apps.insert(name, body["org_slug"].as_str().unwrap_or_default().to_owned());
    FakeResponse::json(201, &json!({}))
}

fn delete_app(state: &mut FakeFlyState, app: &str) -> FakeResponse {
    if state.apps.remove(app).is_none() {
        return error(404, "app not found");
    }
    state.machines.clear();
    state.volumes.clear();
    state.secrets.clear();
    state.ips.remove(app);
    FakeResponse::status(202)
}

fn assign_ip(state: &mut FakeFlyState, app: &str, body: &Value) -> FakeResponse {
    let address = match body["type"].as_str() {
        Some("shared_v4") => "66.241.124.10".to_owned(),
        Some("v6") => format!("2a09:8280:1::{:x}", state.ips.values().map(Vec::len).sum::<usize>() + 1),
        _ => return error(400, "unsupported ip type"),
    };
    state.ips.entry(app.to_owned()).or_default().push(address.clone());
    FakeResponse::json(200, &json!({"ip": address}))
}

fn create_volume(state: &mut FakeFlyState, body: &Value) -> FakeResponse {
    let (Some(name), Some(region)) =
        (body["name"].as_str(), body["region"].as_str().filter(|region| !region.is_empty()))
    else {
        return error(400, "a volume needs a name and a region");
    };
    let size_gb = body["size_gb"].as_u64().unwrap_or(1);
    state.next_id += 1;
    let volume = FakeVolume {
        id: format!("vol_{:08x}", state.next_id),
        name: name.to_owned(),
        region: region.to_owned(),
        size_gb,
        attached_machine_id: None,
    };
    state.volumes.push(volume.clone());
    FakeResponse::json(200, &volume.to_json())
}

fn extend_volume(state: &mut FakeFlyState, id: &str, body: &Value) -> FakeResponse {
    let size_gb = body["size_gb"].as_u64().unwrap_or(0);
    let machines = state.machines.clone();
    let Some(volume) = state.volumes.iter_mut().find(|volume| volume.id == id) else {
        return error(404, "volume not found");
    };
    if size_gb <= volume.size_gb {
        return error(400, "new size must be larger than the current size");
    }
    volume.size_gb = size_gb;
    let needs_restart = volume
        .attached_machine_id
        .as_ref()
        .and_then(|machine_id| machines.iter().find(|machine| &machine.id == machine_id))
        .is_some_and(|machine| machine.state == "started");
    let volume_json = volume.to_json();
    FakeResponse::json(200, &json!({ "needs_restart": needs_restart, "volume": volume_json }))
}

/// `exec` of `graphrt internal store-file` against the files of the Machine's volume.
fn exec_machine(state: &mut FakeFlyState, id: &str, body: &Value) -> FakeResponse {
    use sha2::{Digest, Sha256};
    let command: Vec<String> =
        body["command"].as_array().into_iter().flatten().filter_map(|part| part.as_str().map(str::to_owned)).collect();
    state.exec_commands.push(command.clone());
    let Some(machine) = state.machine(id).cloned() else { return error(404, "machine not found") };
    if machine.state != "started" {
        return error(412, "machine is not running");
    }
    let answer = |stdout: String, stderr: &str, exit_code: i64| {
        FakeResponse::json(200, &json!({ "stdout": stdout, "stderr": stderr, "exit_code": exit_code }))
    };
    let Some(volume) = mounted_volumes(&machine.config).into_iter().next() else {
        return answer(String::new(), "no volume mounted", 1);
    };
    let args: Vec<&str> = command.iter().map(String::as_str).collect();
    let files = state.volume_files.entry(volume).or_default();
    let hex = |bytes: &[u8]| bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    match args.as_slice() {
        ["graphrt", "internal", "store-file", "hash", path] => match files.get(*path) {
            Some(bytes) => answer(format!("{}\n", hex(&Sha256::digest(bytes))), "", 0),
            None => answer("missing\n".to_owned(), "", 0),
        },
        ["graphrt", "internal", "store-file", "begin", path] => {
            files.insert(format!("{path}.part"), Vec::new());
            answer("ok 0\n".to_owned(), "", 0)
        }
        ["graphrt", "internal", "store-file", "append", path, "--at", at] => {
            let stdin = body["stdin"].as_str().unwrap_or_default();
            let bytes: Option<Vec<u8>> = (0..stdin.len())
                .step_by(2)
                .map(|index| stdin.get(index..index + 2).and_then(|pair| u8::from_str_radix(pair, 16).ok()))
                .collect();
            let (Some(bytes), Some(part)) = (bytes, files.get_mut(&format!("{path}.part"))) else {
                return answer(String::new(), "bad piece or no begin", 1);
            };
            let at: usize = at.parse().unwrap_or(usize::MAX);
            if part.len() == at + bytes.len() {
                return answer(format!("{}\n", part.len()), "", 0);
            }
            if part.len() != at {
                return answer(String::new(), "wrong offset", 1);
            }
            part.extend_from_slice(&bytes);
            answer(format!("{}\n", part.len()), "", 0)
        }
        ["graphrt", "internal", "store-file", "commit", path, sha256] => {
            let Some(part) = files.remove(&format!("{path}.part")) else {
                return answer(String::new(), "no part", 1);
            };
            if hex(&Sha256::digest(&part)) != *sha256 {
                return answer(String::new(), "incomplete or damaged", 1);
            }
            let size = part.len();
            files.insert((*path).to_owned(), part);
            answer(format!("ok {size}\n"), "", 0)
        }
        ["graphrt", "internal", "store-file", "prune", keep @ ..] => {
            let kept: Vec<&str> = keep.chunks(2).filter_map(|pair| pair.get(1).copied()).collect();
            let before = files.len();
            files.retain(|path, _| path.split('/').next().is_some_and(|dir| kept.contains(&dir)));
            answer(format!("ok {}\n", before - files.len()), "", 0)
        }
        _ => answer(String::new(), "unknown command", 127),
    }
}

fn delete_volume(state: &mut FakeFlyState, id: &str) -> FakeResponse {
    match state.volumes.iter().position(|volume| volume.id == id) {
        Some(index) if state.volumes[index].attached_machine_id.is_some() => {
            error(400, "volume is attached to a machine")
        }
        Some(index) => {
            state.volumes.remove(index);
            FakeResponse::json(200, &json!({"ok": true}))
        }
        None => error(404, "volume not found"),
    }
}

fn update_machine(state: &mut FakeFlyState, id: &str, body: &Value) -> FakeResponse {
    let failing_checks = state.failing_checks;
    let skip_launch = body["skip_launch"].as_bool() == Some(true);
    let config = body["config"].clone();
    // Like Fly: the volumes of a Machine are decided when it is created.
    if let Some(machine) = state.machine(id)
        && mounted_volumes(&machine.config) != mounted_volumes(&config)
    {
        return error(400, "invalid_argument: unable to add volume after machine is created");
    }
    if skip_launch
        && state.replacing_reads > 0
        && let Some(machine) = state.machine(id)
    {
        let previous = machine.config.clone();
        state.replacing.insert(id.to_owned(), (state.replacing_reads, previous));
    }
    with_machine(state, id, |machine| {
        machine.config = config;
        // Like Fly: a running Machine reboots with the new config, a stopped one
        // stays stopped (`update stopped`) until it is started.
        if !skip_launch && machine.state == "started" {
            machine.state = "starting".to_owned();
        }
        FakeResponse::json(200, &machine.to_json(failing_checks))
    })
}

fn list_machines(state: &mut FakeFlyState, query: &str) -> FakeResponse {
    let filters: Vec<(String, String)> = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter_map(|(key, value)| key.strip_prefix("metadata.").map(|key| (key.to_owned(), percent_decode(value))))
        .collect();
    let ids: Vec<String> = state.machines.iter().map(|machine| machine.id.clone()).collect();
    let machines: Vec<Value> = ids
        .iter()
        .filter_map(|id| read_machine(state, id))
        .filter(|machine| {
            filters.iter().all(|(key, value)| {
                machine.pointer(&format!("/config/metadata/{key}")).and_then(Value::as_str) == Some(value.as_str())
            })
        })
        .collect();
    FakeResponse::json(200, &Value::Array(machines))
}

/// What a read of Machine `id` returns: its previous config in state `replacing`
/// while an update is still being applied (see [`FakeFlyState::replacing_reads`]).
fn read_machine(state: &mut FakeFlyState, id: &str) -> Option<Value> {
    let machine = state.machine(id)?;
    let mut shown = machine.to_json(state.failing_checks);
    if let Some((reads_left, previous_config)) = state.replacing.get_mut(id) {
        shown["state"] = Value::from("replacing");
        shown["config"] = previous_config.clone();
        shown["checks"] = json!([]);
        *reads_left = reads_left.saturating_sub(1);
        if *reads_left == 0 {
            state.replacing.remove(id);
        }
    }
    Some(shown)
}

fn create_machine(state: &mut FakeFlyState, body: &Value) -> FakeResponse {
    let name = body["name"].as_str().unwrap_or_default().to_owned();
    if state.machines.iter().any(|machine| machine.name == name) {
        return FakeResponse::json(
            422,
            &json!({"error": "a machine with this name already exists", "status": "name_taken"}),
        );
    }
    if state.machine_limit.is_some_and(|limit| state.machines.len() >= limit) {
        return FakeResponse::json(
            422,
            &json!({"error": "Your organization has reached its machine limit. Please contact billing@fly.io"}),
        );
    }
    let region = body["region"].as_str().unwrap_or_default().to_owned();
    let mounts = mounted_volumes(&body["config"]);
    for volume_id in &mounts {
        match state.volumes.iter().find(|volume| &volume.id == volume_id) {
            None => return error(400, "volume not found"),
            Some(volume) if volume.attached_machine_id.is_some() => {
                return error(400, "volume is already attached to a machine");
            }
            Some(volume) if volume.region != region => {
                return error(400, "the volume is in another region than the machine");
            }
            Some(_) => {}
        }
    }
    let initial_state = if body["skip_launch"].as_bool() == Some(true) { "created" } else { "starting" };
    let machine = state.add_machine(name, region, body["config"].clone(), initial_state);
    for volume in state.volumes.iter_mut().filter(|volume| mounts.contains(&volume.id)) {
        volume.attached_machine_id = Some(machine.id.clone());
    }
    FakeResponse::json(200, &machine.to_json(state.failing_checks))
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
    for volume in &mut state.volumes {
        if volume.attached_machine_id.as_deref() == Some(id) {
            volume.attached_machine_id = None;
        }
    }
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

fn update_secrets(state: &mut FakeFlyState, body: &Value, body_text: &str) -> FakeResponse {
    if state.echo_secrets_on_error {
        return FakeResponse::json(400, &json!({ "error": format!("invalid request: {body_text}") }));
    }
    if let Some(values) = body["values"].as_object() {
        for (name, value) in values {
            match value.as_str() {
                Some(value) => state.secrets.insert(name.clone(), value.to_owned()),
                None => state.secrets.remove(name),
            };
        }
    }
    state.secrets_version += 1;
    FakeResponse::json(200, &json!({"version": state.secrets_version}))
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
