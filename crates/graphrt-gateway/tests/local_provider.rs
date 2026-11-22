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
