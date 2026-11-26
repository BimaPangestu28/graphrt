//! `local-process` provider (feature `gw-local`, plan §5): one
//! `graphrt serve --trust-gateway` process per replica, listening on a Unix
//! socket (a named pipe on Windows) the gateway chose, in its own process group.
//!
//! - **wake**: spawns the missing processes and waits until their socket
//!   accepts connections (no plugin: ~10 ms).
//! - **release**: `SIGTERM` to the process group (the agent drains), `SIGKILL`
//!   after the grace. Signals go through `/bin/kill` because the workspace
//!   forbids `unsafe` (no `libc::kill`). On Windows, which has no signal to
//!   send, a signed `POST /internal/v1/stop` asks the agent to drain and exit,
//!   and `taskkill /F /T` ends it (and its children) after the grace.
//! - **list**: every replica of a deployment version, including ones started
//!   by a previous gateway process: each replica leaves a `.pid` file next to
//!   its socket, so after a `kill -9` of the gateway the new one can adopt
//!   their orphans or release them (C12). On Windows the pid file also names
//!   the replica's pipe (`\\.\pipe\graphrt-…` with 128 random bits, so no
//!   other process can create it first).
//! - **socket paths** must fit in `sun_path` (108 bytes on Linux, 104 on macOS
//!   and the BSDs, NUL included), or the agent cannot bind and exits. When
//!   `socket_dir` is too long, sockets and pid files go to a short directory
//!   instead, `/tmp/graphrt-gw-<hash of socket_dir>` (mode 0700, owned by the
//!   gateway's user), the same one after every restart so orphans are adopted.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use graphrt_provider_api::{
    Addressing, BoxFuture, Capabilities, DeploymentRef, Endpoint, EndpointAddress, Provider, ProviderError,
    ReleaseMode, ReplicaPhase, ReplicaRef, ReplicaState, Scaler,
};
use tokio::process::{Child, Command};

/// `CREATE_NEW_PROCESS_GROUP` process creation flag (Windows).
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// Whether an agent accepts connections at `address` (start-up readiness).
async fn accepts(address: &EndpointAddress) -> bool {
    match address {
        #[cfg(unix)]
        EndpointAddress::Unix(socket) => tokio::net::UnixStream::connect(socket).await.is_ok(),
        _ => is_listening(address),
    }
}

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
    /// The `.pid` file recording the replica for a restarted gateway.
    pid_file: PathBuf,
    /// Where the agent listens: `Unix(socket)` (next to the pid file) or `Pipe(name)`.
    address: EndpointAddress,
    phase: ReplicaPhase,
}

impl LocalReplica {
    /// Removes the pid file and, on Unix, the socket file.
    fn remove_files(&self) {
        let _cleanup = std::fs::remove_file(&self.pid_file);
        if let EndpointAddress::Unix(socket) = &self.address {
            let _cleanup = std::fs::remove_file(socket);
        }
    }
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

/// Longest Unix socket path `bind` accepts (`sun_path` minus the NUL).
#[cfg(unix)]
const MAX_SOCKET_PATH: usize = if cfg!(target_os = "linux") { 107 } else { 103 };

/// Longest file name `spawn_one` creates: `g<10 hex>-<u64::MAX>.sock`.
#[cfg(unix)]
const MAX_SOCKET_NAME: usize = "g0123456789-18446744073709551615.sock".len();

/// Base of the short directory used when `socket_dir` is too long.
#[cfg(unix)]
const SHORT_SOCKET_BASE: &str = "/tmp";

#[cfg(unix)]
fn fits_sun_path(directory: &Path) -> bool {
    directory.as_os_str().len() + 1 + MAX_SOCKET_NAME <= MAX_SOCKET_PATH
}

/// The directory sockets and pid files actually go to: `configured` when every
/// socket path in it fits `sun_path`, else a short private directory derived
/// from it (stable across restarts, so orphans are still adopted).
///
/// # Errors
/// When a directory cannot be created, or the short one exists but is not a
/// private directory of this user (another user could otherwise intercept
/// the agents' sockets).
#[cfg(unix)]
fn effective_socket_dir(configured: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
    std::fs::create_dir_all(configured)?;
    if fits_sun_path(configured) {
        return Ok(configured.to_path_buf());
    }
    let canonical = std::fs::canonicalize(configured)?;
    let short = Path::new(SHORT_SOCKET_BASE).join(format!("graphrt-gw-{}", short_hash(&canonical.to_string_lossy())));
    match std::fs::DirBuilder::new().mode(0o700).create(&short) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let owner = std::fs::metadata(configured)?.uid();
    let metadata = std::fs::symlink_metadata(&short)?;
    if !metadata.is_dir() || metadata.uid() != owner || metadata.mode() & 0o077 != 0 {
        return Err(std::io::Error::other(format!(
            "socket_dir {} is too long for Unix socket paths ({MAX_SOCKET_PATH} bytes), and its short replacement {} \
             is not a private directory of this user; remove it or set a shorter socket_dir",
            configured.display(),
            short.display()
        )));
    }
    Ok(short)
}

/// On Windows the directory only holds pid files (pipes have no path length
/// limit): `configured`, created if missing.
///
/// # Errors
/// When the directory cannot be created.
#[cfg(not(unix))]
fn effective_socket_dir(configured: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(configured)?;
    Ok(configured.to_path_buf())
}

fn version_prefix(deployment: &DeploymentRef) -> String {
    format!("g{}", short_hash(&format!("{}/{}", deployment.name, deployment.version)))
}

/// Ends the agent `pid` and its children at once: `SIGKILL` to its process
/// group, `taskkill /F /T` on Windows.
async fn kill_agent(pid: u32) {
    #[cfg(unix)]
    let _best_effort = crate::os::signal_group(pid, "KILL").await;
    #[cfg(windows)]
    let _best_effort = crate::os::kill_tree(pid).await;
    #[cfg(not(any(unix, windows)))]
    let _best_effort = crate::os::kill(pid).await;
}

/// `--listen` value of an agent listening at `address`.
fn listen_argument(address: &EndpointAddress) -> String {
    match address {
        EndpointAddress::Pipe(name) => format!("pipe:{name}"),
        EndpointAddress::Unix(socket) => format!("unix:{}", socket.display()),
        EndpointAddress::Http(url) => url.clone(),
        _ => String::new(),
    }
}

/// Whether an agent accepts connections at `address` right now (blocking, but
/// immediate: a local socket or pipe answers or refuses at once).
fn is_listening(address: &EndpointAddress) -> bool {
    match address {
        #[cfg(unix)]
        EndpointAddress::Unix(socket) => std::os::unix::net::UnixStream::connect(socket).is_ok(),
        // Opening the pipe takes one of its instances (the agent makes the
        // next one before it serves this one); every instance busy means alive.
        EndpointAddress::Pipe(name) => match std::fs::OpenOptions::new().read(true).write(true).open(name) {
            Ok(_client) => true,
            Err(error) => error.raw_os_error() == Some(ERROR_PIPE_BUSY),
        },
        _ => false,
    }
}

/// How long a starting agent is probed at [`FAST_PROBE_INTERVAL`].
const FAST_PROBE_WINDOW: Duration = Duration::from_millis(50);
const FAST_PROBE_INTERVAL: Duration = Duration::from_micros(200);

/// Waits up to `window` for an agent to listen at `address`, probing every
/// [`FAST_PROBE_INTERVAL`] (blocking). Returns whether it does.
fn wait_listening(address: &EndpointAddress, window: Duration) -> bool {
    let deadline = std::time::Instant::now() + window;
    loop {
        if is_listening(address) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(FAST_PROBE_INTERVAL);
    }
}

/// `ERROR_PIPE_BUSY` (Windows).
const ERROR_PIPE_BUSY: i32 = 231;

/// 128 random bits as hex: the unguessable part of a pipe name.
fn pipe_nonce() -> Result<String, ProviderError> {
    use ring::rand::SecureRandom as _;
    let mut bytes = [0u8; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| ProviderError::transient("no randomness for the agent's pipe name"))?;
    Ok(crate::hex::encode(&bytes))
}

impl LocalProvider {
    /// A provider placing sockets in `socket_dir` (created if missing), or in
    /// a short private directory when paths in `socket_dir` would not fit
    /// `sun_path` (see [`Self::socket_dir`]).
    ///
    /// # Errors
    /// When a directory cannot be created or the short one is not private.
    pub fn new(socket_dir: &Path, grace: Duration) -> std::io::Result<Self> {
        Ok(Self {
            socket_dir: effective_socket_dir(socket_dir)?,
            grace,
            ready_timeout: Duration::from_secs(30),
            versions: Mutex::new(HashMap::new()),
            replicas: Mutex::new(HashMap::new()),
            wake_lock: tokio::sync::Mutex::new(()),
            next_socket: std::sync::atomic::AtomicU64::new(0),
            released: Mutex::new(std::collections::HashSet::new()),
        })
    }

    /// The directory the agents' sockets and pid files are in.
    #[must_use]
    pub fn socket_dir(&self) -> &Path {
        &self.socket_dir
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
        let pid_file = loop {
            let number = self.next_socket.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let candidate = self.socket_dir.join(format!("{prefix}-{number}.pid"));
            if !candidate.exists() && !candidate.with_extension("sock").exists() {
                break candidate;
            }
        };
        let address = if cfg!(windows) {
            let stem = pid_file.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
            EndpointAddress::Pipe(format!(r"\\.\pipe\graphrt-{stem}-{}", pipe_nonce()?))
        } else {
            EndpointAddress::Unix(pid_file.with_extension("sock"))
        };
        let mut command = Command::new(program);
        command
            .args(arguments)
            .arg("--listen")
            .arg(listen_argument(&address))
            .arg("--trust-gateway")
            .arg("--deployment")
            .arg(&deployment.name)
            .env("GRAPHRT_GATEWAY_KEYS", &version.keys)
            .envs(version.env.iter().map(|(name, value)| (name.as_str(), value.as_str())))
            .stdin(std::process::Stdio::null())
            .kill_on_drop(false);
        #[cfg(unix)]
        command.process_group(0);
        // Its own group: the console's Ctrl+C reaches the gateway, not the agents.
        #[cfg(windows)]
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
        let child = command.spawn().map_err(|error| ProviderError::fatal(format!("spawning the agent: {error}")))?;
        let pid = child.id().ok_or_else(|| ProviderError::transient("agent exited at once"))?;
        let mut record = format!("{pid}\n{}\n{}\n", deployment.name, deployment.version);
        if let EndpointAddress::Pipe(name) = &address {
            record.push_str(name);
            record.push('\n');
        }
        std::fs::write(&pid_file, record).map_err(|error| ProviderError::transient(error.to_string()))?;
        let replica = ReplicaRef { deployment: deployment.clone(), id: pid.to_string() };
        self.replicas.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(
            replica.clone(),
            LocalReplica {
                child: Some(child),
                pid,
                pid_file: pid_file.clone(),
                address: address.clone(),
                phase: ReplicaPhase::Starting,
            },
        );
        let deadline = tokio::time::Instant::now() + self.ready_timeout;
        // Probed every 200 us on a blocking thread while an agent usually comes
        // up (a few ms): the reactor's timer sleeps whole milliseconds, and a
        // 2 ms poll found a ready agent 1-2 ms late on every wake. Exits and the
        // ready timeout are checked by the slower loop below.
        let probed = address.clone();
        let mut ready = graphrt_core::task::spawn_blocking(move || wait_listening(&probed, FAST_PROBE_WINDOW))
            .await
            .unwrap_or(false);
        loop {
            if std::mem::take(&mut ready) || accepts(&address).await {
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
                kill_agent(pid).await;
                return Err(ProviderError::transient(match exited {
                    Some(status) => format!("agent exited during start-up ({status})"),
                    None => "agent did not listen in time".to_owned(),
                }));
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// Asks the agent at `address` to drain and exit (`POST /internal/v1/stop`,
    /// signed with the deployment's first key): the Windows stand-in for
    /// `SIGTERM`. Whether the agent accepted.
    #[cfg(not(unix))]
    async fn request_stop(&self, deployment: &DeploymentRef, address: &EndpointAddress) -> bool {
        let keys = self
            .versions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(deployment.name.clone(), deployment.version.clone()))
            .map(|version| version.keys.clone())
            .unwrap_or_default();
        let Some(key) = crate::signature::parse_keys(&keys).ok().and_then(|keys| keys.into_iter().next()) else {
            return false;
        };
        let client = crate::agent_client::AgentClient::default();
        tokio::time::timeout(self.grace, client.stop(address, &key, &deployment.name))
            .await
            .is_ok_and(|sent| sent.is_ok())
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
            let address = if cfg!(windows) {
                // The fourth line names the pipe; a pid file without one is not ours.
                let Some(name) = text.lines().nth(3).map(str::trim).filter(|name| !name.is_empty()) else { continue };
                EndpointAddress::Pipe(name.to_owned())
            } else {
                EndpointAddress::Unix(path.with_extension("sock"))
            };
            let mut replicas = self.replicas.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            replicas.entry(replica).or_insert_with(|| LocalReplica {
                child: None,
                pid,
                pid_file: path.clone(),
                address,
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
                None => !is_listening(&state.address),
            };
            if exited {
                state.remove_files();
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
                .map(|(replica, state)| Endpoint { replica: replica.clone(), address: state.address.clone() })
                .collect())
        })
    }

    fn release<'a>(&'a self, replica: &'a ReplicaRef, _mode: ReleaseMode) -> BoxFuture<'a, Result<(), ProviderError>> {
        Box::pin(async move {
            let Some(mut state) = self.forget(replica) else { return Ok(()) };
            self.released.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(state.pid);
            #[cfg(unix)]
            let asked = crate::os::signal_group(state.pid, "TERM").await;
            #[cfg(not(unix))]
            let asked = self.request_stop(&replica.deployment, &state.address).await;
            let grace = self.grace;
            graphrt_core::task::spawn(async move {
                let exited = match state.child.as_mut() {
                    Some(child) => tokio::time::timeout(grace, child.wait()).await.is_ok(),
                    // Adopted: on Windows it is gone once its pipe is; on Unix
                    // the grace is waited out as before.
                    None if cfg!(windows) && asked => {
                        let address = state.address.clone();
                        tokio::time::timeout(grace, async move {
                            while is_listening(&address) {
                                tokio::time::sleep(Duration::from_millis(50)).await;
                            }
                        })
                        .await
                        .is_ok()
                    }
                    None => {
                        tokio::time::sleep(grace).await;
                        false
                    }
                };
                if !exited {
                    kill_agent(state.pid).await;
                }
                if let Some(child) = state.child.as_mut() {
                    let _reaped = child.wait().await;
                }
                state.remove_files();
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
                    address: Some(state.address.clone()),
                    started_at_ms: None,
                })
                .collect())
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_starting_agent_is_found_within_a_fraction_of_a_millisecond() {
        let dir = std::env::temp_dir().join(format!("graphrt-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("a.sock");
        let _stale = std::fs::remove_file(&socket);
        let address = EndpointAddress::Unix(socket.clone());
        assert!(!wait_listening(&address, Duration::from_millis(5)), "nothing listens yet");
        let bound = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(3));
            let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
            (std::time::Instant::now(), listener)
        });
        assert!(wait_listening(&address, Duration::from_secs(5)));
        let found = std::time::Instant::now();
        let (listening, _listener) = bound.join().unwrap();
        assert!(found.duration_since(listening) < Duration::from_millis(1), "{:?}", found.duration_since(listening));
        let _cleanup = std::fs::remove_dir_all(&dir);
    }
}
