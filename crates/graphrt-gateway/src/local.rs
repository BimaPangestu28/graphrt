//! `local-process` provider (feature `gw-local`, plan §5): one
//! `graphrt serve --trust-gateway` process per replica, listening on a Unix
//! socket the gateway chose, in its own process group.
//!
//! - **wake**: spawns the missing processes and waits until their socket
//!   accepts connections (no plugin: ~10 ms).
//! - **release**: `SIGTERM` to the process group (the agent drains), `SIGKILL`
//!   after the grace. Signals go through `/bin/kill` because the workspace
//!   forbids `unsafe` (no `libc::kill`).
//! - **list**: every replica of a deployment version, including ones started
//!   by a previous gateway process: each replica leaves a `.pid` file next to
//!   its socket, so after a `kill -9` of the gateway the new one can adopt
//!   their orphans or release them (C12).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use graphrt_provider_api::{
    Addressing, BoxFuture, Capabilities, DeploymentRef, Endpoint, EndpointAddress, Provider, ProviderError,
    ReleaseMode, ReplicaPhase, ReplicaRef, ReplicaState, Scaler,
};
use tokio::process::{Child, Command};

/// How the agents of one deployment version are started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalVersion {
    /// Program and arguments, e.g. `["graphrt", "serve", "agent.yaml"]`;
    /// the provider appends `--listen unix:<socket> --trust-gateway --deployment <name>`.
    pub command: Vec<String>,
    /// `GRAPHRT_GATEWAY_KEYS` value for the agents (`kid:hex,...`).
    pub keys: String,
    /// Extra environment (never secrets of the gateway itself).
    pub env: Vec<(String, String)>,
}

#[derive(Debug)]
struct LocalReplica {
    child: Option<Child>,
    pid: u32,
    socket: PathBuf,
    phase: ReplicaPhase,
}

/// The local-process provider.
#[derive(Debug)]
pub struct LocalProvider {
    socket_dir: PathBuf,
    grace: Duration,
    ready_timeout: Duration,
    versions: Mutex<HashMap<(String, String), LocalVersion>>,
    replicas: Mutex<HashMap<ReplicaRef, LocalReplica>>,
    wake_lock: tokio::sync::Mutex<()>,
    next_socket: std::sync::atomic::AtomicU64,
    /// Pids this process released: their pid files stay until they exit, but
    /// they must not be adopted back as live replicas.
    released: Mutex<std::collections::HashSet<u32>>,
}

fn short_hash(text: &str) -> String {
    crate::hex::sha256_hex(text.as_bytes()).chars().take(10).collect()
}

fn version_prefix(deployment: &DeploymentRef) -> String {
    format!("g{}", short_hash(&format!("{}/{}", deployment.name, deployment.version)))
}

async fn signal_group(pid: u32, signal: &str) {
    let _best_effort = Command::new("kill").arg(format!("-{signal}")).arg(format!("-{pid}")).status().await;
}

impl LocalProvider {
    /// A provider placing sockets in `socket_dir` (created if missing).
    ///
    /// # Errors
    /// When the directory cannot be created.
    pub fn new(socket_dir: &Path, grace: Duration) -> std::io::Result<Self> {
        std::fs::create_dir_all(socket_dir)?;
        Ok(Self {
            socket_dir: socket_dir.to_path_buf(),
            grace,
            ready_timeout: Duration::from_secs(30),
            versions: Mutex::new(HashMap::new()),
            replicas: Mutex::new(HashMap::new()),
            wake_lock: tokio::sync::Mutex::new(()),
            next_socket: std::sync::atomic::AtomicU64::new(0),
            released: Mutex::new(std::collections::HashSet::new()),
        })
    }

    /// Registers how to start `deployment`'s agents.
    pub fn register(&self, deployment: &DeploymentRef, version: LocalVersion) {
        self.versions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert((deployment.name.clone(), deployment.version.clone()), version);
    }

    fn ready_count(&self, deployment: &DeploymentRef) -> usize {
        self.replicas
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(replica, state)| replica.deployment == *deployment && state.phase == ReplicaPhase::Ready)
            .count()
    }

    async fn spawn_one(&self, deployment: &DeploymentRef, version: &LocalVersion) -> Result<(), ProviderError> {
        let (program, arguments) =
            version.command.split_first().ok_or_else(|| ProviderError::fatal("empty agent command"))?;
        let prefix = version_prefix(deployment);
        // Concurrent spawns each take their own number; files left by an earlier
        // gateway process are skipped.
        let socket = loop {
            let number = self.next_socket.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let candidate = self.socket_dir.join(format!("{prefix}-{number}.sock"));
            if !candidate.exists() && !candidate.with_extension("pid").exists() {
                break candidate;
            }
        };
        let mut command = Command::new(program);
        command
            .args(arguments)
            .arg("--listen")
            .arg(format!("unix:{}", socket.display()))
            .arg("--trust-gateway")
            .arg("--deployment")
            .arg(&deployment.name)
            .env("GRAPHRT_GATEWAY_KEYS", &version.keys)
            .envs(version.env.iter().map(|(name, value)| (name.as_str(), value.as_str())))
            .stdin(std::process::Stdio::null())
            .process_group(0)
            .kill_on_drop(false);
        let child = command.spawn().map_err(|error| ProviderError::fatal(format!("spawning the agent: {error}")))?;
        let pid = child.id().ok_or_else(|| ProviderError::transient("agent exited at once"))?;
        let pid_file = socket.with_extension("pid");
        std::fs::write(&pid_file, format!("{pid}\n{}\n{}\n", deployment.name, deployment.version))
            .map_err(|error| ProviderError::transient(error.to_string()))?;
        let replica = ReplicaRef { deployment: deployment.clone(), id: pid.to_string() };
        self.replicas.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(
            replica.clone(),
            LocalReplica { child: Some(child), pid, socket: socket.clone(), phase: ReplicaPhase::Starting },
        );
        let deadline = tokio::time::Instant::now() + self.ready_timeout;
        loop {
            if tokio::net::UnixStream::connect(&socket).await.is_ok() {
                if let Some(state) =
                    self.replicas.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get_mut(&replica)
                {
                    state.phase = ReplicaPhase::Ready;
                }
                return Ok(());
            }
            let exited = self
                .replicas
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_mut(&replica)
                .and_then(|state| state.child.as_mut())
                .and_then(|child| child.try_wait().ok().flatten());
            if exited.is_some() || tokio::time::Instant::now() >= deadline {
                self.forget(&replica);
                let _cleanup = std::fs::remove_file(&pid_file);
                signal_group(pid, "KILL").await;
                return Err(ProviderError::transient(match exited {
                    Some(status) => format!("agent exited during start-up ({status})"),
                    None => "agent did not listen in time".to_owned(),
                }));
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    fn forget(&self, replica: &ReplicaRef) -> Option<LocalReplica> {
        self.replicas.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(replica)
    }

    /// Replicas recorded by `.pid` files (possibly of a previous gateway process).
    fn adopt_from_disk(&self, deployment: &DeploymentRef) {
        let prefix = version_prefix(deployment);
        let Ok(entries) = std::fs::read_dir(&self.socket_dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(&prefix) || path.extension().is_none_or(|extension| extension != "pid") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let Some(pid) = text.lines().next().and_then(|line| line.trim().parse::<u32>().ok()) else { continue };
            if self.released.lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains(&pid) {
                continue;
            }
            let replica = ReplicaRef { deployment: deployment.clone(), id: pid.to_string() };
            let socket = path.with_extension("sock");
            let mut replicas = self.replicas.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            replicas.entry(replica).or_insert_with(|| LocalReplica {
                child: None,
                pid,
                socket,
                phase: ReplicaPhase::Ready,
            });
        }
    }

    fn refresh(&self, deployment: &DeploymentRef) {
        let mut replicas = self.replicas.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        replicas.retain(|replica, state| {
            if replica.deployment != *deployment {
                return true;
            }
            let exited = match state.child.as_mut() {
                Some(child) => child.try_wait().ok().flatten().is_some(),
                // Adopted: alive while its socket accepts connections.
                None => std::os::unix::net::UnixStream::connect(&state.socket).is_err(),
            };
            if exited {
                let _cleanup = std::fs::remove_file(state.socket.with_extension("pid"));
                let _cleanup = std::fs::remove_file(&state.socket);
            }
            !exited
        });
    }
}

impl Provider for LocalProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            addressing: Addressing::Replica,
            scaler: Scaler::Gateway,
            max_request_secs: None,
            idle_timeout_secs: None,
            max_grace_secs: self.grace.as_secs(),
            background_cpu: true,
            supports_suspend: false,
            wake_p95_target_ms: 15,
        }
    }

    fn wake<'a>(
        &'a self,
        deployment: &'a DeploymentRef,
        replicas: u32,
    ) -> BoxFuture<'a, Result<Vec<Endpoint>, ProviderError>> {
        Box::pin(async move {
            let _serialised = self.wake_lock.lock().await;
            let version = self
                .versions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&(deployment.name.clone(), deployment.version.clone()))
                .cloned();
            let version = version.ok_or_else(|| {
                ProviderError::not_found(format!("{}@{} is not registered", deployment.name, deployment.version))
            })?;
            self.refresh(deployment);
            let missing = (replicas as usize).saturating_sub(self.ready_count(deployment));
            let started = futures::future::join_all((0..missing).map(|_| self.spawn_one(deployment, &version))).await;
            if let Some(Err(error)) = started.into_iter().find(Result::is_err)
                && self.ready_count(deployment) == 0
            {
                return Err(error);
            }
            let replicas = self.replicas.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            Ok(replicas
                .iter()
                .filter(|(replica, state)| replica.deployment == *deployment && state.phase == ReplicaPhase::Ready)
                .map(|(replica, state)| Endpoint {
                    replica: replica.clone(),
                    address: EndpointAddress::Unix(state.socket.clone()),
                })
                .collect())
        })
    }

    fn release<'a>(&'a self, replica: &'a ReplicaRef, _mode: ReleaseMode) -> BoxFuture<'a, Result<(), ProviderError>> {
        Box::pin(async move {
            let Some(mut state) = self.forget(replica) else { return Ok(()) };
            self.released.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(state.pid);
            signal_group(state.pid, "TERM").await;
            let grace = self.grace;
            tokio::spawn(async move {
                let exited = match state.child.as_mut() {
                    Some(child) => tokio::time::timeout(grace, child.wait()).await.is_ok(),
                    None => {
                        tokio::time::sleep(grace).await;
                        false
                    }
                };
                if !exited {
                    signal_group(state.pid, "KILL").await;
                }
                if let Some(child) = state.child.as_mut() {
                    let _reaped = child.wait().await;
                }
                let _cleanup = std::fs::remove_file(state.socket.with_extension("pid"));
                let _cleanup = std::fs::remove_file(&state.socket);
            });
            Ok(())
        })
    }

    fn list<'a>(&'a self, deployment: &'a DeploymentRef) -> BoxFuture<'a, Result<Vec<ReplicaState>, ProviderError>> {
        Box::pin(async move {
            self.adopt_from_disk(deployment);
            self.refresh(deployment);
            let replicas = self.replicas.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            Ok(replicas
                .iter()
                .filter(|(replica, _)| replica.deployment == *deployment)
                .map(|(replica, state)| ReplicaState {
                    replica: replica.clone(),
                    phase: state.phase,
                    address: Some(EndpointAddress::Unix(state.socket.clone())),
                    started_at_ms: None,
                })
                .collect())
        })
    }
}
