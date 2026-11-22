//! Conformance of the local-process provider against real processes: a tiny
//! Python stand-in that listens on the Unix socket the provider passes
//! (`--listen unix:<path>`). Skipped when `python3` is missing.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::print_stdout)]

use std::sync::Arc;
use std::time::Duration;

use graphrt_gateway::local::{LocalProvider, LocalVersion};
use graphrt_provider_api::conformance::{CaseId, CaseOutcome, ConformanceConfig, ConformanceTarget, run_conformance};
use graphrt_provider_api::{BoxFuture, DeploymentRef, Provider, ReplicaRef};

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
            let status = tokio::process::Command::new("kill")
                .arg("-KILL")
                .arg(&replica.id)
                .status()
                .await
                .map_err(|error| error.to_string())?;
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
const MAX_SOCKET_PATH: usize = if cfg!(target_os = "linux") { 107 } else { 103 };

fn socket_of(endpoint: &graphrt_provider_api::Endpoint) -> std::path::PathBuf {
    match &endpoint.address {
        graphrt_provider_api::EndpointAddress::Unix(path) => path.clone(),
        other => panic!("expected a Unix socket, got {other:?}"),
    }
}

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
