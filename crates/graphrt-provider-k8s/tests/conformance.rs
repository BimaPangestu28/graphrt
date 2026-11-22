//! Provider conformance (plan §7) of the Kubernetes provider against the
//! in-process fake API server.
//!
//! Provider-level checks run here: C1, C2, C4, C5, C8, C9, C10, C12. C3, C6,
//! C7 and C11 exercise the gateway's run supervisor (drain, streams, version
//! pool) and run in the shared gateway harness; C13 is the binary-size gate.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use graphrt_provider_k8s::auth::{AccessToken, ClusterConfig, SecretString, TokenFuture, TokenSource};
use graphrt_provider_k8s::fake::{FakeApiServer, FakeOptions};
use graphrt_provider_k8s::{
    ClientOptions, DeployDriver, DeploymentRef, DeploymentSpec, K8sDeployDriver, K8sProvider, K8sProviderConfig,
    KubeClient, Provider, ProviderError, ReleaseMode, ReplicaPhase, SecretRef,
};

const NAMESPACE: &str = "agents";

fn bot() -> DeploymentRef {
    DeploymentRef::new("support-bot", "v3")
}

fn fast_retries() -> ClientOptions {
    ClientOptions {
        backoff_base: Duration::from_millis(5),
        backoff_cap: Duration::from_millis(50),
        qps: 1_000.0,
        burst: 1_000,
        ..ClientOptions::default()
    }
}

async fn cluster(options: FakeOptions) -> FakeApiServer {
    let fake = FakeApiServer::start(options).await.unwrap();
    fake.add_deployment(NAMESPACE, "support-bot-v3", "support-bot", "v3", 8080);
    fake
}

fn provider_for(fake: &FakeApiServer, config: ClusterConfig) -> K8sProvider {
    let client = KubeClient::new(&config, fast_retries()).unwrap();
    let mut provider_config = K8sProviderConfig::new(NAMESPACE);
    provider_config.wake_timeout = Duration::from_secs(3);
    let _ = fake;
    K8sProvider::new(client, provider_config)
}

fn provider(fake: &FakeApiServer) -> K8sProvider {
    provider_for(fake, ClusterConfig::new(fake.url()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn c1_concurrent_wakes_from_zero_scale_exactly_once() {
    let fake = cluster(FakeOptions::default()).await;
    let provider = Arc::new(provider(&fake));
    let wakes: Vec<_> = (0..20)
        .map(|_| {
            let provider = provider.clone();
            tokio::spawn(async move { provider.wake(&bot(), 1).await })
        })
        .collect();
    for wake in wakes {
        let endpoints = wake.await.unwrap().unwrap();
        assert_eq!(endpoints.len(), 1);
        assert_eq!(endpoints[0].address.port(), 8080);
    }
    assert_eq!(fake.replicas(NAMESPACE, "support-bot-v3"), Some(1));
    assert_eq!(fake.pods(NAMESPACE, "support-bot-v3").len(), 1);
    assert_eq!(fake.scale_patches(), 1, "singleflight: one scale patch for 20 wakes");
    // Asking for more scales up once more, to exactly that many.
    assert_eq!(provider.wake(&bot(), 3).await.unwrap().len(), 3);
    assert_eq!(fake.replicas(NAMESPACE, "support-bot-v3"), Some(3));
    assert_eq!(fake.scale_patches(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c2_wake_latency_overhead_over_readiness() {
    let ready_delay = Duration::from_millis(20);
    let fake = cluster(FakeOptions { ready_delay, ..FakeOptions::default() }).await;
    let provider = provider(&fake);
    let mut overheads = Vec::new();
    for _ in 0..50 {
        let started = Instant::now();
        let endpoints = provider.wake(&bot(), 1).await.unwrap();
        overheads.push(started.elapsed().saturating_sub(ready_delay));
        provider.release(&endpoints[0].replica, ReleaseMode::ScaleIn).await.unwrap();
        assert_eq!(fake.replicas(NAMESPACE, "support-bot-v3"), Some(0));
    }
    overheads.sort();
    let p50 = overheads[overheads.len() / 2];
    let p95 = overheads[overheads.len() * 95 / 100];
    eprintln!("C2 provider overhead over pod readiness (fake API, n=50): p50 {p50:?}, p95 {p95:?}");
    // The §5 target (3 s) is for a real cluster; against the fake, the provider's own share must be tiny.
    assert!(p95 < Duration::from_millis(250), "p95 overhead {p95:?}");
}

#[tokio::test]
async fn c4_scale_in_removes_only_the_released_replica() {
    let fake = cluster(FakeOptions::default()).await;
    let provider = provider(&fake);
    let endpoints = provider.wake(&bot(), 3).await.unwrap();
    // The middle pod is neither the newest nor the oldest: only its deletion cost singles it out.
    let victim = endpoints[1].replica.clone();
    provider.release(&victim, ReleaseMode::ScaleIn).await.unwrap();
    let pods = fake.pods(NAMESPACE, "support-bot-v3");
    assert_eq!(pods.len(), 2);
    assert!(!pods.contains(&victim.id), "victim {} still running: {pods:?}", victim.id);
    for survivor in [&endpoints[0], &endpoints[2]] {
        assert!(pods.contains(&survivor.replica.id), "busy replica {} was removed", survivor.replica.id);
    }
    // Idempotent: releasing a replica that is gone changes nothing.
    provider.release(&victim, ReleaseMode::ScaleIn).await.unwrap();
    assert_eq!(fake.replicas(NAMESPACE, "support-bot-v3"), Some(2));
    // Every release mode is a scale-in on Kubernetes; down to zero.
    provider.release(&endpoints[0].replica, ReleaseMode::Suspend).await.unwrap();
    provider.release(&endpoints[2].replica, ReleaseMode::Stop).await.unwrap();
    assert_eq!(fake.replicas(NAMESPACE, "support-bot-v3"), Some(0));
    assert!(fake.pods(NAMESPACE, "support-bot-v3").is_empty());
}

#[tokio::test]
async fn c4_scale_survives_concurrent_writers() {
    let fake = cluster(FakeOptions::default()).await;
    let provider = provider(&fake);
    fake.inject_scale_conflicts(3);
    assert_eq!(provider.wake(&bot(), 2).await.unwrap().len(), 2);
    assert_eq!(fake.replicas(NAMESPACE, "support-bot-v3"), Some(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c5_replica_killed_outside_the_gateway_is_detected() {
    let fake = cluster(FakeOptions::default()).await;
    let provider = provider(&fake);
    let endpoints = provider.wake(&bot(), 2).await.unwrap();
    let (sender, mut changes) = tokio::sync::mpsc::unbounded_channel::<Vec<String>>();
    let mut tracker = provider.readiness(&bot());
    let follower = tokio::spawn(async move {
        tracker
            .follow(|ready| {
                let _closed = sender.send(ready.iter().map(|address| address.pod.clone()).collect());
            })
            .await
    });
    let initial = changes.recv().await.unwrap();
    assert_eq!(initial.len(), 2);
    let killed = endpoints[0].replica.id.clone();
    let killed_at = Instant::now();
    fake.kill_pod(NAMESPACE, &killed);
    let detected = loop {
        let ready = tokio::time::timeout(Duration::from_secs(15), changes.recv()).await.unwrap().unwrap();
        if !ready.contains(&killed) {
            break killed_at.elapsed();
        }
    };
    eprintln!("C5 killed replica detected after {detected:?}");
    assert!(detected < Duration::from_secs(15));
    // The controller replaces it; the replacement shows up once ready.
    let replaced = loop {
        let ready = tokio::time::timeout(Duration::from_secs(15), changes.recv()).await.unwrap().unwrap();
        if ready.len() == 2 {
            break ready;
        }
    };
    assert!(!replaced.contains(&killed));
    // History compaction (410 Gone) is survived by a relist.
    fake.compact();
    fake.kill_pod(NAMESPACE, &replaced[0]);
    loop {
        let ready = tokio::time::timeout(Duration::from_secs(15), changes.recv()).await.unwrap().unwrap();
        if !ready.contains(&replaced[0]) {
            break;
        }
    }
    follower.abort();
}

#[derive(Debug)]
struct RotatingToken {
    fetches: AtomicUsize,
}

impl TokenSource for RotatingToken {
    fn fetch(&self) -> TokenFuture<'_> {
        let fetch = self.fetches.fetch_add(1, Ordering::SeqCst);
        let token = if fetch == 0 { "token-old" } else { "token-new" };
        Box::pin(async move {
            Ok(AccessToken {
                token: SecretString::new(token),
                expires_at: Some(std::time::SystemTime::now() + Duration::from_secs(3600)),
            })
        })
    }
}

#[tokio::test]
async fn c8_rejected_token_is_refreshed_once_and_never_leaks() {
    let fake = cluster(FakeOptions {
        accepted_tokens: Some(HashSet::from(["token-new".to_owned()])),
        ..FakeOptions::default()
    })
    .await;
    let source = Arc::new(RotatingToken { fetches: AtomicUsize::new(0) });
    let provider = provider_for(&fake, ClusterConfig::new(fake.url()).with_token_source(source.clone()));
    assert_eq!(provider.wake(&bot(), 1).await.unwrap().len(), 1);
    assert_eq!(source.fetches.load(Ordering::SeqCst), 2, "one refresh after the 401");
    // The fresh token is reused (cached until exp - 5 min).
    provider.list(&bot()).await.unwrap();
    assert_eq!(source.fetches.load(Ordering::SeqCst), 2);
    let bearers: Vec<_> = fake.requests().into_iter().filter_map(|request| request.bearer).collect();
    assert_eq!(bearers.iter().filter(|bearer| *bearer == "token-old").count(), 1);

    // Credentials revoked: one refresh, then a typed Auth error without the token in it.
    fake.set_accepted_tokens(Some(HashSet::new()));
    let error = provider.list(&bot()).await.unwrap_err();
    assert!(matches!(error, ProviderError::Auth(_)), "{error:?}");
    let printed = format!("{error} {error:?} {provider:?}");
    assert!(!printed.contains("token-new") && !printed.contains("token-old"), "{printed}");
}

#[tokio::test]
async fn c9_transient_errors_back_off_and_classify() {
    let fake = cluster(FakeOptions::default()).await;
    let provider = provider(&fake);
    fake.inject_faults([(503, None), (429, Some(0)), (500, None)]);
    assert_eq!(provider.wake(&bot(), 1).await.unwrap().len(), 1);

    // Four attempts (ClientOptions::max_attempts), all failing: the error is classified.
    fake.inject_faults([(500, None); 4]);
    assert!(matches!(provider.list(&bot()).await, Err(ProviderError::Transient(_))));
    fake.inject_faults([(429, Some(0)); 4]);
    assert!(matches!(provider.list(&bot()).await, Err(ProviderError::RateLimited { .. })));
    fake.inject_faults([(403, None)]);
    assert!(matches!(provider.list(&bot()).await, Err(ProviderError::Auth(_))), "403 is not retried");
    let missing = DeploymentRef::new("nope", "v1");
    assert!(matches!(provider.wake(&missing, 1).await, Err(ProviderError::NotFound(_))));
}

#[tokio::test]
async fn c9_unschedulable_replicas_are_a_capacity_error() {
    let fake = cluster(FakeOptions { capacity: Some(1), ..FakeOptions::default() }).await;
    let mut config = K8sProviderConfig::new(NAMESPACE);
    config.wake_timeout = Duration::from_millis(300);
    let provider = K8sProvider::new(KubeClient::new(&ClusterConfig::new(fake.url()), fast_retries()).unwrap(), config);
    let error = provider.wake(&bot(), 2).await.unwrap_err();
    assert!(matches!(&error, ProviderError::Capacity(message) if message.contains("insufficient cpu")), "{error:?}");
    let over_limit = provider.wake(&bot(), 1_000).await.unwrap_err();
    assert!(matches!(over_limit, ProviderError::Capacity(_)));
}

fn spec() -> DeploymentSpec {
    DeploymentSpec {
        deployment: DeploymentRef::new("helpdesk", "v1"),
        image: "ghcr.io/bimapangestu28/graphrt:0.5.0".into(),
        args: vec!["serve".into(), "--trust-gateway".into(), "/app/workflow.yaml".into()],
        port: 8080,
        env: vec![("RUST_LOG".into(), "info".into())],
        secrets: vec![SecretRef { name: "helpdesk-llm".into(), key: None, env: None }],
        grace_secs: 120,
        cpu: None,
        memory: Some("128Mi".into()),
    }
}

#[tokio::test]
async fn c10_deploy_references_secrets_and_never_resets_scale() {
    let fake = FakeApiServer::start(FakeOptions::default()).await.unwrap();
    let client = KubeClient::new(&ClusterConfig::new(fake.url()), fast_retries()).unwrap();
    let driver = K8sDeployDriver::new(client.clone(), NAMESPACE);
    driver
        .apply_secret("helpdesk-llm", &[("OPENAI_API_KEY".into(), SecretString::new("sk-live-should-not-leak"))])
        .await
        .unwrap();
    let deployed = driver.apply(&spec()).await.unwrap();
    assert_eq!(deployed, spec().deployment);
    let manifest = fake.applied(NAMESPACE, "deployments", "helpdesk-v1").unwrap();
    assert_eq!(manifest["spec"]["replicas"], 0, "new versions start scaled to zero");
    let text = manifest.to_string();
    assert!(!text.contains("sk-live") && text.contains("helpdesk-llm"), "{text}");
    assert!(fake.applied(NAMESPACE, "services", "helpdesk-v1").is_some());
    assert!(!format!("{:?}", spec()).contains("info"), "spec Debug prints env names only");

    // The gateway scales the version; a re-deploy keeps its replica count.
    let provider = K8sProvider::new(client, K8sProviderConfig::new(NAMESPACE));
    assert_eq!(provider.wake(&deployed, 2).await.unwrap().len(), 2);
    driver.apply(&spec()).await.unwrap();
    assert_eq!(fake.replicas(NAMESPACE, "helpdesk-v1"), Some(2));
}

#[tokio::test]
async fn c12_list_reports_replicas_for_reconciliation() {
    let fake = cluster(FakeOptions::default()).await;
    let provider = provider(&fake);
    assert!(provider.list(&bot()).await.unwrap().is_empty());
    let endpoints = provider.wake(&bot(), 2).await.unwrap();
    let replicas = provider.list(&bot()).await.unwrap();
    assert_eq!(replicas.len(), 2);
    assert!(replicas.iter().all(|replica| replica.phase == ReplicaPhase::Ready && replica.address.is_some()));
    // Orphans after a gateway crash are released from the list alone.
    for replica in &replicas {
        provider.release(&replica.replica, ReleaseMode::Stop).await.unwrap();
    }
    assert!(provider.list(&bot()).await.unwrap().is_empty());
    assert_eq!(fake.replicas(NAMESPACE, "support-bot-v3"), Some(0));
    drop(endpoints);
}
