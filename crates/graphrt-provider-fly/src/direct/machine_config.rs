//! Machine configs of direct deploys, kept as JSON so a standby Machine's
//! config (fields Fly added included) survives a rollback unchanged except for
//! its services and role.

use serde_json::{Value, json};

use super::serve_options::STORE_PATH;
use super::{AgentFile, DeployRequest, DirectChannel, DirectError, ServiceMode};

/// Port `graphrt serve` listens on inside the Machine.
pub const AGENT_PORT: u16 = 8080;
/// Directory `graphrt serve` serves; the agent file is written here.
pub const WORKFLOW_DIR: &str = "/workflows";
/// Largest agent file shipped inside the Machine config (`config.files`). Fly
/// refuses Machine requests of about 2 MiB ("request body too large"), and the
/// file travels base64-encoded. Larger agents are uploaded onto the store
/// volume instead ([`AGENTS_DIR`]).
pub const MAX_AGENT_FILE_BYTES: usize = 1024 * 1024;
/// Where agents too large for the Machine config live on the store volume, one
/// directory per agent (named after its SHA-256) that `graphrt serve` serves.
/// Equal to `graphrt internal store-file`'s default root.
pub const AGENTS_DIR: &str = "/data/agents";
/// Bytes of an agent sent in one `exec` call while uploading it (hex-encoded on stdin).
pub const UPLOAD_PIECE_BYTES: usize = 2 * 1024 * 1024;
/// What a Machine serves while an agent is being uploaded onto its new volume:
/// nothing anyone can reach (the Machine has no services meanwhile).
pub(super) const PLACEHOLDER_WORKFLOW: &str = "version: \"1\"\nname: uploading\ngraph:\n  nodes:\n    - { id: start, type: start }\n    - { id: end, type: end }\n  edges:\n    - { source: start, target: end }\n";

/// Where a Machine finds its agent.
#[derive(Clone, Copy, Debug)]
pub(super) enum AgentSource<'a> {
    /// Written by Fly from the Machine config into [`WORKFLOW_DIR`].
    Inline(&'a AgentFile),
    /// Already on the store volume, in this directory (see [`AGENTS_DIR`]).
    OnVolume { dir: &'a str },
}

/// Whether the agent is too large for the Machine config and goes onto the store volume.
pub(super) fn agent_on_volume(agent: &AgentFile) -> bool {
    agent.contents.len() > MAX_AGENT_FILE_BYTES
}

/// Memory for a Machine serving `agent`: 256 MB for agents that fit in the
/// Machine config; knowledge stores are held in memory, so larger agents get
/// 4 MB per MB they take once unpacked (Fly sizes; at most 2 GB per shared CPU).
/// Returns `(cpus, memory_mb)`.
pub(super) fn guest_for(agent: &AgentFile) -> (u32, u32) {
    if !agent_on_volume(agent) {
        return (1, 256);
    }
    let agent_bytes = agent.unpacked_bytes.max(u64::try_from(agent.contents.len()).unwrap_or(u64::MAX));
    let agent_mb = u32::try_from(agent_bytes.div_ceil(1024 * 1024)).unwrap_or(u32::MAX);
    let wanted = 256u32.saturating_add(agent_mb.saturating_mul(4));
    let memory_mb = [256u32, 512, 1024, 2048, 4096, 8192].into_iter().find(|size| *size >= wanted).unwrap_or(8192);
    (memory_mb.div_ceil(2048).max(1), memory_mb)
}

/// Store volume size for an agent of `agent_bytes` on it: room for the agent,
/// a new version being uploaded next to it, and the store's own files.
pub(super) fn volume_gb_for(agent_bytes: usize) -> u32 {
    let gib = 1024 * 1024 * 1024;
    let needed = u32::try_from(agent_bytes.saturating_mul(3).div_ceil(gib)).unwrap_or(u32::MAX).saturating_add(1);
    needed.max(super::serve_options::STORE_VOLUME_GB)
}
/// SIGTERM → SIGKILL grace (`kill_timeout`); `serve --drain-timeout` stays 5 s below.
const STOP_GRACE_SECS: u64 = 30;

/// Metadata: marks Machines owned by direct deploys.
pub(super) const META_DIRECT: &str = "graphrt_direct";
/// Metadata: deployment name.
pub(super) const META_DEPLOYMENT: &str = "graphrt_deployment";
/// Metadata: version label.
pub(super) const META_VERSION: &str = "graphrt_version";
/// Metadata: `candidate`, `current` or `standby`.
pub(super) const META_ROLE: &str = "graphrt_role";
/// Metadata: [`ServiceMode::as_str`].
pub(super) const META_MODE: &str = "graphrt_mode";
/// Metadata: deploy time of the version (Unix ms).
pub(super) const META_DEPLOYED_AT: &str = "graphrt_deployed_at";
/// Metadata: workflow name the agent is served under.
pub(super) const META_WORKFLOW: &str = "graphrt_workflow";
/// Metadata: `"1"` while the deployment is paused.
pub(super) const META_PAUSED: &str = "graphrt_paused";

/// Metadata: `"1"` on the Machine that mounts the deployment's store volume.
pub(super) const META_STORE: &str = "graphrt_store";
/// Name of the Machine that mounts the store volume. It is updated in place by
/// every deploy that needs the store, so its name carries no version.
pub(super) const STORE_MACHINE_NAME: &str = "graphrt-store";

/// Role of a Machine in a direct deployment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Role {
    Candidate,
    Current,
    Standby,
}

impl Role {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Current => "current",
            Self::Standby => "standby",
        }
    }
}

/// The Fly app name of a deployment: lowercase letters, digits and dashes, at most 63 characters.
///
/// @param deployment - deployment name (&str)
/// @returns The app name
/// @throws DirectError(InvalidInput) - nothing usable is left after sanitising
///
/// @example
/// assert_eq!(graphrt_provider_fly::direct::app_name_for("Support Bot!").unwrap(), "support-bot");
pub fn app_name_for(deployment: &str) -> Result<String, DirectError> {
    let slug = slugify(deployment, 63);
    if slug.is_empty() {
        return Err(DirectError::invalid(format!("deployment name {deployment:?} has no letters or digits")));
    }
    Ok(slug)
}

/// Lowercase `[a-z0-9-]`, no leading/trailing/double dashes, at most `max_len` bytes.
fn slugify(text: &str, max_len: usize) -> String {
    let mut slug = String::with_capacity(text.len().min(max_len));
    for character in text.chars().flat_map(char::to_lowercase) {
        let mapped = if character.is_ascii_lowercase() || character.is_ascii_digit() { character } else { '-' };
        if mapped == '-' && (slug.is_empty() || slug.ends_with('-')) {
            continue;
        }
        if slug.len() == max_len {
            break;
        }
        slug.push(mapped);
    }
    slug.trim_end_matches('-').to_owned()
}

/// Deterministic Machine name of a version: a readable slug plus a hash of the
/// exact label, so `v1.0` and `v1-0` never share a Machine.
pub(super) fn machine_name_for(version: &str) -> String {
    let slug = slugify(version, 40);
    format!("graphrt-{slug}-{:08x}", fnv1a_32(version.as_bytes()))
}

fn fnv1a_32(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5_u32, |hash, byte| (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193))
}

/// Checks a deploy request before any call.
pub(super) fn validate_request(request: &DeployRequest) -> Result<(), DirectError> {
    if request.version.is_empty() || request.version.len() > 128 || request.version.chars().any(char::is_control) {
        return Err(DirectError::invalid("--version must be 1-128 printable characters"));
    }
    if request.image.is_empty() {
        return Err(DirectError::invalid("the container image is empty"));
    }
    if request.agent.contents.is_empty() {
        return Err(DirectError::invalid(format!("{} is empty", request.agent.file_name)));
    }
    if request.drop_store && agent_on_volume(&request.agent) {
        return Err(DirectError::invalid(format!(
            "--drop-store deletes the store volume, which holds this agent ({} bytes, more than a Machine \
             config carries): leave out --drop-store",
            request.agent.contents.len()
        )));
    }
    if let Some(bad) = request.secrets.keys().find(|name| !is_env_name(name)) {
        return Err(DirectError::invalid(format!("secret name {bad:?} is not a valid environment variable name")));
    }
    request.serve.validate().map_err(DirectError::invalid)?;
    if request.drop_store && request.serve.needs_store() {
        return Err(DirectError::invalid(
            "--drop-store deletes the store this deploy needs: leave out --monitor, --devices, --push, \
             --conversations and --channel, or leave out --drop-store (an agent with `long_term_memory` keeps \
             its notes on the store too: it always needs one)",
        ));
    }
    if request.mode == ServiceMode::ScaleToZero && request.serve.channels.contains(&DirectChannel::Telegram) {
        return Err(DirectError::invalid(
            "--channel telegram needs --mode always-on: a stopped Machine does not ask Telegram for new messages",
        ));
    }
    if request.mode == ServiceMode::ScaleToZero
        && request.serve.channels.contains(&DirectChannel::Slack)
        && request.secrets.contains_key("SLACK_APP_TOKEN")
    {
        return Err(DirectError::invalid(
            "--channel slack with SLACK_APP_TOKEN (Socket Mode) needs --mode always-on: a stopped Machine keeps no \
             socket open. Or leave out SLACK_APP_TOKEN and use the Events API with SLACK_SIGNING_SECRET",
        ));
    }
    if request.secrets.contains_key(super::API_KEY_SECRET) {
        return Err(DirectError::invalid(format!(
            "{} is managed by graphrt deploy (use --rotate-api-key)",
            super::API_KEY_SECRET
        )));
    }
    Ok(())
}

/// Whether `name` is a portable environment variable name.
pub(super) fn is_env_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters.next().is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// The full config of a new version's Machine (role `candidate`, public services).
///
/// @param request - the deploy (&DeployRequest)
/// @param workflow - the name the agent is served under (&str)
/// @param public_url - the deployment's address, for device pairing links (&str)
/// @param store_volume - the volume to mount as `graphrt serve --store`, if the deploy needs one (Option<&str>)
#[cfg(test)]
pub(super) fn new_machine_config(
    request: &DeployRequest,
    workflow: &str,
    public_url: &str,
    store_volume: Option<&str>,
) -> Value {
    machine_config_for(request, workflow, public_url, store_volume, AgentSource::Inline(&request.agent))
}

/// [`new_machine_config`] with the agent taken from `agent`.
pub(super) fn machine_config_for(
    request: &DeployRequest,
    workflow: &str,
    public_url: &str,
    store_volume: Option<&str>,
    agent: AgentSource<'_>,
) -> Value {
    let drain_timeout = format!("{}s", STOP_GRACE_SECS - 5);
    let served_dir = match agent {
        AgentSource::Inline(_) => WORKFLOW_DIR,
        AgentSource::OnVolume { dir } => dir,
    };
    let mut command: Vec<String> = vec!["serve".to_owned(), served_dir.to_owned(), "--addr".to_owned()];
    command.extend([format!("0.0.0.0:{AGENT_PORT}"), "--drain-timeout".to_owned(), drain_timeout]);
    // The volume may only hold the agent: `--store` is for deploys that keep state.
    let with_store = store_volume.is_some() && request.serve.needs_store();
    command.extend(request.serve.serve_flags(workflow, public_url, with_store));
    let (cpus, memory_mb) = guest_for(&request.agent);
    let mut config = json!({
        "image": request.image,
        "init": { "cmd": command },
        "env": {
            "GRAPHRT_DEPLOYMENT": request.deployment,
            "GRAPHRT_DEPLOYMENT_VERSION": request.version,
        },
        "guest": { "cpu_kind": "shared", "cpus": cpus, "memory_mb": memory_mb },
        "checks": {
            "readyz": {
                "type": "http", "port": AGENT_PORT, "method": "GET", "path": "/readyz", "protocol": "http",
                "interval": "10s", "timeout": "5s", "grace_period": "5s", "kind": "readiness",
            }
        },
        "restart": { "policy": "on-failure", "max_retries": 3 },
        "stop_config": { "signal": "SIGTERM", "timeout": format!("{STOP_GRACE_SECS}s") },
        "metadata": {
            META_DIRECT: "1",
            META_DEPLOYMENT: request.deployment,
            META_VERSION: request.version,
            META_DEPLOYED_AT: request.now_ms.to_string(),
            META_WORKFLOW: workflow,
        },
        "auto_destroy": false,
    });
    if let (AgentSource::Inline(file), Some(object)) = (agent, config.as_object_mut()) {
        let guest_path = format!("{WORKFLOW_DIR}/{}", agent_file_name(file, workflow));
        object.insert(
            "files".to_owned(),
            json!([{ "guest_path": guest_path, "raw_value": base64_encode(&file.contents), "mode": 0o444 }]),
        );
    }
    if let Some(volume) = store_volume {
        mount_store(&mut config, volume);
    }
    with_role(&config, Role::Candidate, Some(request.mode))
}

/// The config a new store Machine boots with while its agent is uploaded:
/// a placeholder agent and no services, so nothing reaches it meanwhile.
pub(super) fn holding_config(request: &DeployRequest, workflow: &str, public_url: &str, volume: &str) -> Value {
    let placeholder = AgentFile::new(format!("{workflow}.yaml"), PLACEHOLDER_WORKFLOW.as_bytes().to_vec());
    let config = machine_config_for(request, workflow, public_url, Some(volume), AgentSource::Inline(&placeholder));
    with_role(&config, Role::Standby, None)
}

/// File name of the agent inside its directory on the volume (`<workflow>.<ext>`).
pub(super) fn volume_agent_file_name(agent: &AgentFile, workflow: &str) -> String {
    agent_file_name(agent, workflow)
}

/// Mounts `volume` at [`STORE_PATH`]. A new Fly volume belongs to root while
/// the graphrt image runs as an unprivileged user, so the server of a Machine
/// with a store runs as root inside its Machine (a microVM of its own). The
/// user is a property of a Machine process, so the command moves from `init`
/// to the Machine's one process.
fn mount_store(config: &mut Value, volume: &str) {
    let Some(object) = config.as_object_mut() else { return };
    object.insert("mounts".to_owned(), json!([{ "volume": volume, "path": STORE_PATH }]));
    let command = object.remove("init").and_then(|init| init.get("cmd").cloned()).unwrap_or(Value::Null);
    object.insert("processes".to_owned(), json!([{ "cmd": command, "user": "root" }]));
    if let Some(metadata) = object.get_mut("metadata").and_then(Value::as_object_mut) {
        metadata.insert(META_STORE.to_owned(), Value::from("1"));
    }
}

/// The volume a Machine's config mounts at [`STORE_PATH`], if any.
pub(super) fn store_volume_of(config: Option<&Value>) -> Option<&str> {
    config?
        .get("mounts")?
        .as_array()?
        .iter()
        .find(|mount| mount.get("path").and_then(Value::as_str) == Some(STORE_PATH))?
        .get("volume")?
        .as_str()
}

/// The agent file name under [`WORKFLOW_DIR`]: `<workflow>.<ext>`.
fn agent_file_name(agent: &AgentFile, workflow: &str) -> String {
    let extension = agent.file_name.rsplit_once('.').map_or("yaml", |(_, extension)| extension);
    format!("{workflow}.{}", extension.to_ascii_lowercase())
}

/// `config` with `role`, and public services for `mode` (`None`: no services,
/// so the Fly proxy neither routes to nor autostarts the Machine). Clears the
/// paused marker whenever services are set.
pub(super) fn with_role(config: &Value, role: Role, mode: Option<ServiceMode>) -> Value {
    let mut config = config.clone();
    let Some(object) = config.as_object_mut() else { return config };
    match mode {
        Some(mode) => {
            object.insert("services".to_owned(), services_for(mode));
        }
        None => {
            object.remove("services");
        }
    }
    let metadata = object.entry("metadata").or_insert_with(|| json!({}));
    if let Some(metadata) = metadata.as_object_mut() {
        metadata.insert(META_ROLE.to_owned(), Value::from(role.as_str()));
        if let Some(mode) = mode {
            metadata.insert(META_MODE.to_owned(), Value::from(mode.as_str()));
            metadata.remove(META_PAUSED);
        }
    }
    config
}

/// `config` of a paused current Machine: no services, paused marker set.
pub(super) fn paused(config: &Value) -> Value {
    let mut config = with_role(config, Role::Current, None);
    if let Some(metadata) = config.get_mut("metadata").and_then(Value::as_object_mut) {
        metadata.insert(META_PAUSED.to_owned(), Value::from("1"));
    }
    config
}

/// The public HTTP service: 80 (redirect to HTTPS) and 443, with Fly proxy
/// autostop/autostart for scale-to-zero and one Machine kept running for always-on.
fn services_for(mode: ServiceMode) -> Value {
    let (autostop, min_running) = match mode {
        ServiceMode::AlwaysOn => ("off", 1),
        ServiceMode::ScaleToZero => ("stop", 0),
    };
    json!([{
        "protocol": "tcp",
        "internal_port": AGENT_PORT,
        "ports": [
            { "port": 80, "handlers": ["http"], "force_https": true },
            { "port": 443, "handlers": ["tls", "http"] },
        ],
        "autostop": autostop,
        "autostart": true,
        "min_machines_running": min_running,
        "concurrency": { "type": "requests", "soft_limit": 20, "hard_limit": 25 },
    }])
}

/// Standard base64 with padding (Fly's `raw_value`), without another dependency.
pub(super) fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk.first().copied().unwrap_or(0);
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        let triple = (u32::from(first) << 16) | (u32::from(second) << 8) | u32::from(third);
        let symbols = [(triple >> 18) & 63, (triple >> 12) & 63, (triple >> 6) & 63, triple & 63];
        for (position, symbol) in symbols.iter().enumerate() {
            if position <= chunk.len() {
                let index = usize::try_from(*symbol).unwrap_or(0);
                encoded.push(char::from(ALPHABET.get(index).copied().unwrap_or(b'A')));
            } else {
                encoded.push('=');
            }
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::direct::{ChatAccess, ChatOptions, DirectChannel, ServeOptions, ServiceMode};

    const URL: &str = "https://support.fly.dev";

    fn request(mode: ServiceMode) -> DeployRequest {
        DeployRequest {
            deployment: "support".into(),
            version: "v1".into(),
            region: String::new(),
            mode,
            org_slug: "personal".into(),
            image: "ghcr.io/graphrt/graphrt:0.5.0".into(),
            secrets: BTreeMap::from([("OPENAI_API_KEY".into(), "sk-secret".into())]),
            api_key_candidate: "grk_x".into(),
            rotate_api_key: false,
            agent: AgentFile::new("Support Bot.GRT", b"bundle".to_vec()),
            now_ms: 1_700_000_000_000,
            serve: ServeOptions::default(),
            drop_store: false,
        }
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        let vectors = [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foobar", "Zm9vYmFy")];
        for (plain, encoded) in vectors {
            assert_eq!(base64_encode(plain.as_bytes()), encoded);
        }
    }

    #[test]
    fn names_are_fly_safe_and_distinct() {
        assert_eq!(app_name_for("Support Bot!").unwrap(), "support-bot");
        assert!(app_name_for("!!!").is_err());
        assert_eq!(app_name_for(&"a".repeat(100)).unwrap().len(), 63);
        assert_ne!(machine_name_for("v1.0"), machine_name_for("v1-0"));
        assert!(machine_name_for("V 1.0").starts_with("graphrt-v-1-0-"));
    }

    #[test]
    fn scale_to_zero_config_autostops_and_carries_no_secret() {
        let config = new_machine_config(&request(ServiceMode::ScaleToZero), "support", URL, None);
        let service = &config["services"][0];
        assert_eq!(service["autostop"], "stop");
        assert_eq!(service["autostart"], true);
        assert_eq!(service["min_machines_running"], 0);
        assert_eq!(config["checks"]["readyz"]["path"], "/readyz");
        assert_eq!(config["files"][0]["guest_path"], "/workflows/support.grt");
        assert_eq!(config["files"][0]["raw_value"], "YnVuZGxl");
        assert_eq!(config["metadata"]["graphrt_role"], "candidate");
        let encoded = config.to_string();
        assert!(!encoded.contains("sk-secret") && !encoded.contains("grk_x"), "{encoded}");
    }

    #[test]
    fn a_plain_deploy_runs_serve_as_before_without_a_volume() {
        let config = new_machine_config(&request(ServiceMode::ScaleToZero), "support", URL, None);
        assert_eq!(
            config["init"]["cmd"],
            json!(["serve", "/workflows", "--addr", "0.0.0.0:8080", "--drain-timeout", "25s"])
        );
        assert!(config.get("mounts").is_none() && config.get("processes").is_none());
        assert!(config["metadata"].get(META_STORE).is_none());
        assert_eq!(store_volume_of(Some(&config)), None);
    }

    #[test]
    fn serve_options_become_flags_and_a_store_mounts_the_volume() {
        let mut with_chat = request(ServiceMode::ScaleToZero);
        with_chat.serve.chat = Some(ChatOptions { access: ChatAccess::Guests, ..ChatOptions::default() });
        let config = new_machine_config(&with_chat, "support", URL, None);
        let command: Vec<&str> = config["init"]["cmd"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        assert_eq!(command[6..], ["--chat", "support", "--chat-access", "guests"]);
        assert!(config.get("mounts").is_none(), "a chat page keeps nothing: no volume");

        let mut with_monitor = request(ServiceMode::ScaleToZero);
        with_monitor.serve.monitor = true;
        let config = new_machine_config(&with_monitor, "support", URL, Some("vol_1"));
        let process = &config["processes"][0];
        let command: Vec<&str> = process["cmd"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        assert_eq!(command[..3], ["serve", "/workflows", "--addr"]);
        assert_eq!(command[6..], ["--store", "/data", "--store-ttl-days", "30", "--monitor", "--public-url", URL]);
        assert_eq!(config["mounts"], json!([{ "volume": "vol_1", "path": "/data" }]));
        assert_eq!(process["user"], "root", "a new volume belongs to root");
        assert!(config.get("init").is_none(), "the command lives in the process");
        assert_eq!(config["metadata"][META_STORE], "1");
        assert_eq!(store_volume_of(Some(&config)), Some("vol_1"));
        // A standby or paused config keeps the mount: Fly never lets a Machine drop its volume.
        assert_eq!(store_volume_of(Some(&with_role(&config, Role::Standby, None))), Some("vol_1"));
        assert_eq!(store_volume_of(Some(&paused(&config))), Some("vol_1"));
    }

    #[test]
    fn always_on_keeps_one_machine_running() {
        let config = new_machine_config(&request(ServiceMode::AlwaysOn), "support", URL, None);
        assert_eq!(config["services"][0]["autostop"], "off");
        assert_eq!(config["services"][0]["min_machines_running"], 1);
    }

    #[test]
    fn standby_and_paused_configs_have_no_services() {
        let config = new_machine_config(&request(ServiceMode::AlwaysOn), "support", URL, None);
        let standby = with_role(&config, Role::Standby, None);
        assert!(standby.get("services").is_none());
        assert_eq!(standby["metadata"]["graphrt_role"], "standby");
        let paused_config = paused(&config);
        assert!(paused_config.get("services").is_none());
        assert_eq!(paused_config["metadata"]["graphrt_paused"], "1");
        let resumed = with_role(&paused_config, Role::Current, Some(ServiceMode::AlwaysOn));
        assert!(resumed["metadata"].get("graphrt_paused").is_none());
    }

    #[test]
    fn sizes_large_agents_and_rejects_reserved_secrets() {
        let mut too_big = request(ServiceMode::AlwaysOn);
        let sized = |bytes: usize| AgentFile::new("a.grt", vec![0; bytes]);
        assert_eq!(guest_for(&sized(MAX_AGENT_FILE_BYTES)), (1, 256));
        assert_eq!(guest_for(&sized(MAX_AGENT_FILE_BYTES + 1)), (1, 512));
        assert_eq!(guest_for(&sized(2 * 1024 * 1024).unpacked(100 * 1024 * 1024)), (1, 1024), "unpacked size counts");
        assert_eq!(guest_for(&sized(2 * 1024 * 1024).unpacked(5 * 1024 * 1024 * 1024)), (4, 8192));
        assert_eq!(volume_gb_for(10 * 1024 * 1024), 2);
        assert_eq!(volume_gb_for(1024 * 1024 * 1024), 4);
        too_big.agent.contents = vec![0; MAX_AGENT_FILE_BYTES + 1];
        assert!(validate_request(&too_big).is_ok(), "large agents go onto the store volume");
        too_big.drop_store = true;
        assert!(validate_request(&too_big).unwrap_err().message.contains("holds this agent"));
        let mut reserved = request(ServiceMode::AlwaysOn);
        reserved.secrets.insert("GRAPHRT_API_KEY".into(), "x".into());
        assert!(validate_request(&reserved).is_err());
        let mut bad_name = request(ServiceMode::AlwaysOn);
        bad_name.secrets.insert("1BAD".into(), "x".into());
        assert!(validate_request(&bad_name).is_err());
        assert!(validate_request(&request(ServiceMode::AlwaysOn)).is_ok());
    }

    #[test]
    fn rejects_serve_options_that_cannot_work() {
        let mut dropping_what_it_needs = request(ServiceMode::AlwaysOn);
        dropping_what_it_needs.serve.monitor = true;
        dropping_what_it_needs.drop_store = true;
        assert!(validate_request(&dropping_what_it_needs).unwrap_err().message.contains("--drop-store"));
        let mut bad_origin = request(ServiceMode::AlwaysOn);
        bad_origin.serve.cors_origins = vec!["example.com".into()];
        assert!(validate_request(&bad_origin).is_err());
        let mut dropping = request(ServiceMode::AlwaysOn);
        dropping.drop_store = true;
        assert!(validate_request(&dropping).is_ok());
    }

    #[test]
    fn a_telegram_channel_needs_a_machine_that_keeps_running() {
        let mut sleeping = request(ServiceMode::ScaleToZero);
        sleeping.serve.channels = vec![DirectChannel::Telegram];
        assert!(validate_request(&sleeping).unwrap_err().message.contains("always-on"));
        sleeping.mode = ServiceMode::AlwaysOn;
        assert!(validate_request(&sleeping).is_ok());
        // Meta's webhook request starts a stopped Machine, like any request.
        let mut whatsapp = request(ServiceMode::ScaleToZero);
        whatsapp.serve.channels = vec![DirectChannel::WhatsApp];
        assert!(validate_request(&whatsapp).is_ok());
        whatsapp.drop_store = true;
        assert!(validate_request(&whatsapp).unwrap_err().message.contains("--channel"));
        let mut socket = request(ServiceMode::ScaleToZero);
        socket.serve.channels = vec![DirectChannel::Slack];
        socket.secrets.insert("SLACK_APP_TOKEN".into(), "xapp-1".into());
        assert!(validate_request(&socket).unwrap_err().message.contains("always-on"));
    }

    #[test]
    fn channel_secrets_stay_out_of_the_machine_config() {
        let mut with_channel = request(ServiceMode::AlwaysOn);
        with_channel.serve.channels = vec![DirectChannel::WhatsApp];
        with_channel.secrets.insert("WHATSAPP_APP_SECRET".into(), "app-secret-value".into());
        let config = new_machine_config(&with_channel, "support", URL, Some("vol_1"));
        let text = config.to_string();
        assert!(text.contains("whatsapp=support"), "{text}");
        assert!(!text.contains("app-secret-value"), "{text}");
    }
}
