//! Conformance of the local-process provider against real processes: a tiny
//! Python stand-in that listens on the Unix socket the provider passes
//! (`--listen unix:<path>`), or on Windows on the named pipe
//! (`--listen pipe:<name>`, stopping on `POST /internal/v1/stop`). Skipped
//! when `python3` is missing.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::print_stdout)]

use std::sync::Arc;
use std::time::Duration;

use graphrt_gateway::local::{LocalProvider, LocalVersion};
use graphrt_provider_api::conformance::{CaseId, CaseOutcome, ConformanceConfig, ConformanceTarget, run_conformance};
use graphrt_provider_api::{BoxFuture, DeploymentRef, Provider, ReplicaRef};

#[cfg(unix)]
const AGENT: &str = r#"
import os, signal, socket, sys
path = next(a[5:] for a in sys.argv if a.startswith("unix:"))
s = socket.socket(socket.AF_UNIX); s.bind(path); s.listen(16)
signal.signal(signal.SIGTERM, lambda *_: (os.unlink(path), sys.exit(0)))
while True:
    c, _ = s.accept()
    try:
        c.recv(65536); c.sendall(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}")
    except OSError:
        pass
    c.close()
"#;

/// The next pipe instance is made before the current client is served, so the
/// pipe never goes missing between two clients (the provider takes a missing
/// pipe for an agent that exited).
#[cfg(windows)]
const AGENT: &str = r#"
import _winapi, sys
name = next(a[5:] for a in sys.argv if a.startswith("pipe:"))
# PIPE_ACCESS_DUPLEX, FILE_FLAG_FIRST_PIPE_INSTANCE; byte mode, blocking, PIPE_REJECT_REMOTE_CLIENTS
def instance(first):
    flags = 0x3 | (0x80000 if first else 0)
    return _winapi.CreateNamedPipe(name, flags, 0x8, 255, 65536, 65536, 0, 0)
pipe = instance(True)
while True:
    try:
        _winapi.ConnectNamedPipe(pipe, False)
    except OSError:
        pass  # ERROR_PIPE_CONNECTED: the client came first
    current, pipe = pipe, instance(False)
    try:
        request, _ = _winapi.ReadFile(current, 65536)
        _winapi.WriteFile(current, b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}")
        if b"/internal/v1/stop" in request:
            sys.exit(0)
    except OSError:
        pass
    _winapi.CloseHandle(current)
"#;

fn python() -> Option<String> {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|_| "python3".to_owned())
}

struct LocalTarget {
    provider: Arc<LocalProvider>,
    deployment: DeploymentRef,
}

impl ConformanceTarget for LocalTarget {
    fn name(&self) -> String {
        "local-process".into()
    }
    fn provider(&self) -> Arc<dyn Provider> {
        self.provider.clone()
    }
    fn deployment(&self) -> DeploymentRef {
        self.deployment.clone()
    }
    fn kill_externally<'a>(&'a self, replica: &'a ReplicaRef) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let mut command = if cfg!(windows) {
                let mut command = tokio::process::Command::new("taskkill");
                command.args(["/F", "/PID", &replica.id]);
                command
            } else {
                let mut command = tokio::process::Command::new("kill");
                command.args(["-KILL", &replica.id]);
                command
            };
            let status = command.status().await.map_err(|error| error.to_string())?;
            if status.success() { Ok(()) } else { Err("kill failed".into()) }
        })
    }
    fn secrets(&self) -> Vec<String> {
        vec!["k1:".to_owned() + &"ab".repeat(32)]
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_process_provider_conformance() {
    let Some(python) = python() else {
        println!("python3 missing: skipped");
        return;
    };
    let dir = std::env::temp_dir().join(format!("gwlp-{}", std::process::id()));
    let _cleanup = std::fs::remove_dir_all(&dir);
    let provider = Arc::new(LocalProvider::new(&dir, Duration::from_secs(2)).unwrap());
    let deployment = DeploymentRef::new("shop", "v1", "local");
    provider.register(
        &deployment,
        LocalVersion {
            command: vec![python, "-c".into(), AGENT.into()],
            keys: format!("k1:{}", "ab".repeat(32)),
            env: Vec::new(),
        },
    );
    let target = LocalTarget { provider: provider.clone(), deployment: deployment.clone() };
    // Python starts in tens of milliseconds, slower than the 15 ms target of a
    // real agent; C2 is measured against 1.5 s here (loaded CI machines) and reported.
    let config = ConformanceConfig {
        latency_samples: 10,
        wake_p95_target_ms: Some(1500),
        poll_interval: Duration::from_millis(20),
        ..ConformanceConfig::default()
    };
    let report = run_conformance(&target, &config).await;
    println!("{report}");
    report.assert_no_failures();
    for id in [CaseId::C1, CaseId::C2, CaseId::C4, CaseId::C5, CaseId::C12] {
        assert!(matches!(report.outcome(id), Some(CaseOutcome::Passed(_))), "{id}: {:?}", report.outcome(id));
    }
    // C12: a new provider (gateway restarted after kill -9) finds the replicas
    // the old one left behind through their pid files and can release them.
    provider.wake(&deployment, 1).await.unwrap();
    let restarted = LocalProvider::new(&dir, Duration::from_secs(2)).unwrap();
    let orphans = restarted.list(&deployment).await.unwrap();
    assert_eq!(orphans.len(), 1, "orphan adopted from its pid file");
    restarted.release(&orphans[0].replica, graphrt_provider_api::ReleaseMode::Stop).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(restarted.list(&deployment).await.unwrap().is_empty());
    let _cleanup = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn releasing_one_replica_keeps_the_other() {
    let Some(python) = python() else { return };
    let dir = std::env::temp_dir().join(format!("gwlq-{}", std::process::id()));
    let _cleanup = std::fs::remove_dir_all(&dir);
    let provider = LocalProvider::new(&dir, Duration::from_secs(2)).unwrap();
    let deployment = DeploymentRef::new("shop", "v1", "local");
    provider.register(
        &deployment,
        LocalVersion { command: vec![python, "-c".into(), AGENT.into()], keys: String::new(), env: Vec::new() },
    );
    for round in 0..5 {
        let endpoints = provider.wake(&deployment, 2).await.unwrap();
        assert_eq!(endpoints.len(), 2, "round {round}");
        provider.release(&endpoints[0].replica, graphrt_provider_api::ReleaseMode::ScaleIn).await.unwrap();
        let states = provider.list(&deployment).await.unwrap();
        assert_eq!(states.len(), 1, "round {round}: {states:?} (endpoints {endpoints:?})");
        assert_eq!(states[0].replica, endpoints[1].replica, "round {round}");
        for state in states {
            provider.release(&state.replica, graphrt_provider_api::ReleaseMode::Stop).await.unwrap();
        }
    }
    let _cleanup = std::fs::remove_dir_all(&dir);
}

/// Longest Unix socket path `bind` accepts: `sun_path` is 108 bytes on Linux,
/// 104 on macOS and the BSDs, including the terminating NUL.
#[cfg(unix)]
const MAX_SOCKET_PATH: usize = if cfg!(target_os = "linux") { 107 } else { 103 };

#[cfg(unix)]
fn socket_of(endpoint: &graphrt_provider_api::Endpoint) -> std::path::PathBuf {
    match &endpoint.address {
        graphrt_provider_api::EndpointAddress::Unix(path) => path.clone(),
        other => panic!("expected a Unix socket, got {other:?}"),
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_socket_dir_too_long_for_sun_path_still_starts_agents() {
    let Some(python) = python() else { return };
    let base = std::env::temp_dir().join(format!("gwll-{}", std::process::id()));
    let _cleanup = std::fs::remove_dir_all(&base);
    // Longer than sun_path on every platform: a socket inside it could never be bound.
    let dir = base.join("a".repeat(60)).join("b".repeat(60));
    assert!(dir.as_os_str().len() > MAX_SOCKET_PATH);
    let provider = LocalProvider::new(&dir, Duration::from_secs(2)).unwrap();
    let deployment = DeploymentRef::new("shop", "v1", "local");
    provider.register(
        &deployment,
        LocalVersion { command: vec![python, "-c".into(), AGENT.into()], keys: String::new(), env: Vec::new() },
    );

    let endpoints = provider.wake(&deployment, 1).await.expect("the agent starts despite the long socket_dir");

    let socket = socket_of(&endpoints[0]);
    assert!(socket.as_os_str().len() <= MAX_SOCKET_PATH, "{} is too long", socket.display());
    // A gateway restarted with the same socket_dir finds the same short directory and adopts the agent.
    let restarted = LocalProvider::new(&dir, Duration::from_secs(2)).unwrap();
    let orphans = restarted.list(&deployment).await.unwrap();
    assert_eq!(orphans.len(), 1, "orphan adopted from the short directory");
    restarted.release(&orphans[0].replica, graphrt_provider_api::ReleaseMode::Stop).await.unwrap();
    let _cleanup = std::fs::remove_dir_all(&base);
    let _cleanup = socket.parent().map(std::fs::remove_dir_all);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_short_socket_dir_is_used_as_configured() {
    let Some(python) = python() else { return };
    let dir = std::env::temp_dir().join(format!("gwls-{}", std::process::id()));
    let _cleanup = std::fs::remove_dir_all(&dir);
    if dir.as_os_str().len() + 40 > MAX_SOCKET_PATH {
        return; // this machine's temp dir is itself too long to tell the two cases apart
    }
    let provider = LocalProvider::new(&dir, Duration::from_secs(2)).unwrap();
    let deployment = DeploymentRef::new("shop", "v1", "local");
    provider.register(
        &deployment,
        LocalVersion { command: vec![python, "-c".into(), AGENT.into()], keys: String::new(), env: Vec::new() },
    );
    let endpoints = provider.wake(&deployment, 1).await.unwrap();
    assert_eq!(socket_of(&endpoints[0]).parent(), Some(dir.as_path()));
    provider.release(&endpoints[0].replica, graphrt_provider_api::ReleaseMode::Stop).await.unwrap();
    let _cleanup = std::fs::remove_dir_all(&dir);
}

/// Windows: agents listen on named pipes with an unguessable name, recorded in
/// the pid file next to nothing else (no socket file), and stop on the signed
/// `POST /internal/v1/stop` well within the grace.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn windows_agents_listen_on_private_pipes_and_stop_on_request() {
    let Some(python) = python() else { return };
    let dir = std::env::temp_dir().join(format!("gwlw-{}", std::process::id()));
    let _cleanup = std::fs::remove_dir_all(&dir);
    let provider = LocalProvider::new(&dir, Duration::from_secs(10)).unwrap();
    let deployment = DeploymentRef::new("shop", "v1", "local");
    provider.register(
        &deployment,
        LocalVersion {
            command: vec![python, "-c".into(), AGENT.into()],
            keys: format!("k1:{}", "ab".repeat(32)),
            env: Vec::new(),
        },
    );
    let endpoints = provider.wake(&deployment, 1).await.unwrap();
    let graphrt_provider_api::EndpointAddress::Pipe(name) = &endpoints[0].address else {
        panic!("expected a named pipe, got {:?}", endpoints[0].address)
    };
    let random = name.rsplit('-').next().unwrap();
    assert_eq!(random.len(), 32, "128 random bits in {name}");
    let files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|entry| entry.path()).collect();
    assert_eq!(files.len(), 1, "{files:?}");
    assert!(std::fs::read_to_string(&files[0]).unwrap().lines().nth(3) == Some(name.as_str()));
    let began = std::time::Instant::now();
    provider.release(&endpoints[0].replica, graphrt_provider_api::ReleaseMode::Stop).await.unwrap();
    for _ in 0..200 {
        if provider.list(&deployment).await.unwrap().is_empty() && !files[0].exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(provider.list(&deployment).await.unwrap().is_empty());
    assert!(began.elapsed() < Duration::from_secs(5), "stopped on request, not after the 10 s grace");
    let _cleanup = std::fs::remove_dir_all(&dir);
}

/// Process state letter of `pid` (`T`: stopped), Linux only.
#[cfg(target_os = "linux")]
fn process_state(pid: &str) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.get(stat.rfind(')')? + 2..)?.chars().next()
}

/// `pool.idle_mode: freeze`: a released (`Suspend`) replica is kept frozen
/// (stopped, listed as `Suspended`), the next wake thaws it instead of starting
/// a process, and stopping a frozen replica still ends it.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_frozen_replica_is_thawed_by_the_next_wake() {
    use graphrt_provider_api::{ReleaseMode, ReplicaPhase};
    let Some(python) = python() else { return };
    let dir = std::env::temp_dir().join(format!("gwlf-{}", std::process::id()));
    let _cleanup = std::fs::remove_dir_all(&dir);
    let provider = LocalProvider::new(&dir, Duration::from_secs(2)).unwrap();
    let deployment = DeploymentRef::new("shop", "v1", "local");
    provider.register(
        &deployment,
        LocalVersion { command: vec![python, "-c".into(), AGENT.into()], keys: String::new(), env: Vec::new() },
    );
    let first = provider.wake(&deployment, 1).await.unwrap();
    let replica = first[0].replica.clone();
    provider.release(&replica, ReleaseMode::Suspend).await.unwrap();
    let states = provider.list(&deployment).await.unwrap();
    assert_eq!(states.iter().map(|state| state.phase).collect::<Vec<_>>(), [ReplicaPhase::Suspended]);
    // SIGSTOP is delivered asynchronously.
    #[cfg(target_os = "linux")]
    {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while process_state(&replica.id) != Some('T') {
            assert!(std::time::Instant::now() < deadline, "not frozen: {:?}", process_state(&replica.id));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    let again = provider.wake(&deployment, 1).await.unwrap();
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].replica, replica, "the frozen process serves again, none started");
    #[cfg(target_os = "linux")]
    {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while process_state(&replica.id) == Some('T') {
            assert!(std::time::Instant::now() < deadline, "not thawed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    assert!(std::os::unix::net::UnixStream::connect(socket_of(&again[0])).is_ok());
    // Frozen again, then stopped: the stop thaws it so it can exit.
    provider.release(&replica, ReleaseMode::Suspend).await.unwrap();
    provider.release(&replica, ReleaseMode::Stop).await.unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while provider.list(&deployment).await.unwrap().iter().any(|state| state.replica == replica)
        || std::path::Path::new(&format!("/proc/{}", replica.id)).exists() && cfg!(target_os = "linux")
    {
        assert!(std::time::Instant::now() < deadline, "the frozen replica did not stop");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _cleanup = std::fs::remove_dir_all(&dir);
}

/// `max_frozen_agents`: beyond the limit the agent frozen longest ago is
/// stopped; a limit of 0 stops idle agents as without the freeze mode.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn frozen_agents_stay_within_the_limit() {
    use graphrt_provider_api::{ReleaseMode, ReplicaPhase};
    let Some(python) = python() else { return };
    let dir = std::env::temp_dir().join(format!("gwlm-{}", std::process::id()));
    let _cleanup = std::fs::remove_dir_all(&dir);
    let provider = LocalProvider::new(&dir, Duration::from_secs(2)).unwrap().with_freeze_limit(1);
    let (shop, blog) = (DeploymentRef::new("shop", "v1", "local"), DeploymentRef::new("blog", "v1", "local"));
    for deployment in [&shop, &blog] {
        provider.register(
            deployment,
            LocalVersion {
                command: vec![python.clone(), "-c".into(), AGENT.into()],
                keys: String::new(),
                env: Vec::new(),
            },
        );
    }
    let shop_replica = provider.wake(&shop, 1).await.unwrap()[0].replica.clone();
    let blog_replica = provider.wake(&blog, 1).await.unwrap()[0].replica.clone();
    provider.release(&shop_replica, ReleaseMode::Suspend).await.unwrap();
    assert_eq!(provider.frozen_stats().total, 1);
    // A second frozen agent: the first (frozen longest ago) is stopped.
    provider.release(&blog_replica, ReleaseMode::Suspend).await.unwrap();
    let stats = provider.frozen_stats();
    assert_eq!((stats.total, stats.limit), (1, 1));
    assert_eq!(stats.per_version, vec![("blog".to_owned(), "v1".to_owned(), 1)]);
    assert!(provider.list(&shop).await.unwrap().is_empty(), "the oldest frozen agent was stopped");
    let blog_states = provider.list(&blog).await.unwrap();
    assert_eq!(blog_states.iter().map(|state| state.phase).collect::<Vec<_>>(), [ReplicaPhase::Suspended]);
    provider.release(&blog_replica, ReleaseMode::Stop).await.unwrap();

    let never = LocalProvider::new(&dir.join("never"), Duration::from_secs(2)).unwrap().with_freeze_limit(0);
    never.register(
        &shop,
        LocalVersion { command: vec![python, "-c".into(), AGENT.into()], keys: String::new(), env: Vec::new() },
    );
    let replica = never.wake(&shop, 1).await.unwrap()[0].replica.clone();
    never.release(&replica, ReleaseMode::Suspend).await.unwrap();
    assert!(never.list(&shop).await.unwrap().is_empty(), "limit 0: stopped, not frozen");
    assert_eq!(never.frozen_stats().total, 0);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _cleanup = std::fs::remove_dir_all(&dir);
}
