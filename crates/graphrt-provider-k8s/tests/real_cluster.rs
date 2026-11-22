//! The provider against a real cluster (kind / k3d / any), opt-in:
//!
//! ```sh
//! kind create cluster --name graphrt-k8s
//! kind load docker-image alpine:3.22 --name graphrt-k8s
//! GRAPHRT_K8S_E2E_CONTEXT=kind-graphrt-k8s cargo test -p graphrt-provider-k8s --test real_cluster -- --ignored --nocapture
//! ```
//!
//! It authenticates from the kubeconfig (kind uses a client certificate and an
//! inline CA), deploys a tiny HTTP server (a busybox `nc` loop in `alpine:3.22`
//! answering `200 ok`) with the deploy driver, then checks wake latency from
//! zero, scale-in victim selection with `pod-deletion-cost` and readiness
//! tracking. Namespace: `GRAPHRT_K8S_E2E_NAMESPACE` (default `default`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::time::{Duration, Instant};

use graphrt_provider_api::{DeployDriver, DeploymentRef, DeploymentSpec, Provider, ReleaseMode, ReplicaPhase, Resources};
use graphrt_provider_k8s::auth::{KubeconfigOptions, kubeconfig_from_env};
use graphrt_provider_k8s::{ClientOptions, K8sDeployDriver, K8sProvider, K8sProviderConfig, KubeClient};

fn spec() -> DeploymentSpec {
    DeploymentSpec {
        name: "graphrt-e2e".into(),
        version: "v1".into(),
        image: Some(std::env::var("GRAPHRT_K8S_E2E_IMAGE").unwrap_or_else(|_| "alpine:3.22".into())),
        command: vec![
            "sh".into(),
            "-c".into(),
            "trap 'exit 0' TERM; while true; do printf 'HTTP/1.0 200 OK\\r\\ncontent-length: 2\\r\\n\\r\\nok' | nc -l -p 8080 >/dev/null; done".into(),
        ],
        env: Default::default(),
        secrets: vec![],
        max_active_runs: 1,
        min_replicas: 0,
        max_replicas: 3,
        grace_secs: 5,
        resources: Resources { cpu_millis: 10, memory_mib: 16 },
        workflow_fingerprint: "e2e".into(),
        provider_settings: Default::default(),
    }
}

async fn wait_for_zero(provider: &K8sProvider, deployment: &DeploymentRef) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let live = provider.list(deployment).await.unwrap();
        if live.iter().all(|replica| replica.phase != ReplicaPhase::Ready && replica.phase != ReplicaPhase::Starting) {
            return;
        }
        assert!(Instant::now() < deadline, "replicas still live: {live:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real cluster: set GRAPHRT_K8S_E2E_CONTEXT"]
async fn real_cluster_wake_scale_in_and_readiness() {
    let Ok(context) = std::env::var("GRAPHRT_K8S_E2E_CONTEXT") else {
        eprintln!("GRAPHRT_K8S_E2E_CONTEXT not set, skipping");
        return;
    };
    let namespace = std::env::var("GRAPHRT_K8S_E2E_NAMESPACE").unwrap_or_else(|_| "default".into());
    let cluster = kubeconfig_from_env(&KubeconfigOptions { path: None, context: Some(context) }).unwrap();
    let client = KubeClient::new(&cluster, ClientOptions::default()).unwrap();
    let deployment: DeploymentRef = K8sDeployDriver::new(client.clone(), &namespace).apply(&spec()).await.unwrap();
    let mut config = K8sProviderConfig::new(&namespace);
    config.wake_timeout = Duration::from_secs(120);
    let provider = K8sProvider::new(client, config);

    // Wake from zero, repeatedly: the image is cached on the node (kind load).
    let rounds: usize = std::env::var("GRAPHRT_K8S_E2E_ROUNDS").ok().and_then(|value| value.parse().ok()).unwrap_or(10);
    let mut wakes = Vec::new();
    for _ in 0..rounds {
        wait_for_zero(&provider, &deployment).await;
        let started = Instant::now();
        let endpoints = provider.wake(&deployment, 1).await.unwrap();
        wakes.push(started.elapsed());
        assert_eq!(endpoints.len(), 1);
        provider.release(&endpoints[0].replica, ReleaseMode::ScaleIn).await.unwrap();
    }
    wakes.sort();
    let p50 = wakes[wakes.len() / 2];
    let p95 = wakes[(wakes.len() * 95 / 100).min(wakes.len() - 1)];
    eprintln!("real cluster wake 0->1 (n={rounds}): min {:?}, p50 {p50:?}, p95 {p95:?}, max {:?}", wakes[0], wakes[wakes.len() - 1]);

    // Scale-in picks the released replica, not the oldest or newest.
    wait_for_zero(&provider, &deployment).await;
    let endpoints = provider.wake(&deployment, 3).await.unwrap();
    assert_eq!(endpoints.len(), 3);
    let victim = endpoints[1].replica.clone();
    provider.release(&victim, ReleaseMode::ScaleIn).await.unwrap();
    let mut tracker = provider.readiness(&deployment).unwrap();
    let gone = tracker
        .wait_until(tokio::time::Instant::now() + Duration::from_secs(30), |ready| {
            ready.len() == 2 && ready.iter().all(|address| address.pod != victim.id)
        })
        .await
        .unwrap();
    assert!(gone, "victim {} still ready: {:?}", victim.id, tracker.ready());
    let survivors: Vec<String> = tracker.ready().into_iter().map(|address| address.pod).collect();
    assert!(survivors.contains(&endpoints[0].replica.id) && survivors.contains(&endpoints[2].replica.id), "{survivors:?}");

    for endpoint in [&endpoints[0], &endpoints[2]] {
        provider.release(&endpoint.replica, ReleaseMode::Stop).await.unwrap();
    }
    wait_for_zero(&provider, &deployment).await;
}
