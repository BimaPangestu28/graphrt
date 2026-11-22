//! Provider conformance (plan §7) of the Kubernetes provider.
//!
//! - `shared_harness_c1_to_c13`: the shared suite of `graphrt-provider-api`
//!   (`run_conformance`) against the emulated cluster behind the harness's
//!   scripted `FakeServer` (fault injection, request recording).
//! - The other tests go deeper on Kubernetes specifics against the streaming
//!   fake API server: scale CAS conflicts, victim choice with
//!   `pod-deletion-cost`, readiness watch with `410 Gone`, token rotation,
//!   capacity (unschedulable and ResourceQuota), deploy driver.
//!
//! C3, C6, C7, C11 need the gateway in the loop (`GatewayHarness` from
//! `graphrt-gateway`) and are reported as skipped until it exists. C13 is
//! measured separately (binary size).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use graphrt_provider_api::conformance::{
    CaseId, CaseOutcome, ConformanceConfig, ConformanceTarget, FakeServer, run_conformance,
};
use graphrt_provider_api::http::RetryPolicy;
use graphrt_provider_api::{
    BoxFuture, DeployDriver, DeploymentRef, DeploymentSpec, EndpointAddress, Provider, ProviderError,
    ProviderErrorKind, ReleaseMode, ReplicaPhase, ReplicaRef, Resources, SecretRef,
};
use graphrt_provider_k8s::auth::{ClusterConfig, SecretString, StaticToken, TokenSource};
use graphrt_provider_k8s::fake::{FakeApiServer, FakeOptions};
use graphrt_provider_k8s::{ClientOptions, K8sDeployDriver, K8sProvider, K8sProviderConfig, KubeClient};

const NAMESPACE: &str = "agents";
const BEARER: &str = "k8s-bearer-for-the-scan";

fn bot() -> DeploymentRef {
    DeploymentRef::new("support-bot", "v3", "agents/support-bot-v3")
}

fn fast_retries() -> ClientOptions {
    ClientOptions {
        retry: RetryPolicy { max_attempts: 4, base_delay: Duration::from_millis(5), max_delay: Duration::from_millis(50) },
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

fn provider_at(url: String, token: Option<Arc<dyn TokenSource>>, wake_timeout: Duration) -> K8sProvider {
    let mut cluster = ClusterConfig::new(url);
    cluster.token_source = token;
    let mut config = K8sProviderConfig::new(NAMESPACE);
    config.wake_timeout = wake_timeout;
    K8sProvider::new(KubeClient::new(&cluster, fast_retries()).unwrap(), config)
}

fn provider(fake: &FakeApiServer) -> K8sProvider {
    provider_at(fake.url(), None, Duration::from_secs(3))
}

fn port_of(address: &EndpointAddress) -> u16 {
    match address {
        EndpointAddress::Http(url) => url.rsplit(':').next().unwrap().parse().unwrap(),
        EndpointAddress::Unix(path) => panic!("unexpected unix endpoint {path:?}"),
    }
}

struct KindTarget {
    cluster: FakeApiServer,
    server: FakeServer,
    provider: Arc<K8sProvider>,
}

impl ConformanceTarget for KindTarget {
    fn name(&self) -> String {
        "kubernetes (emulated API server)".into()
    }
    fn provider(&self) -> Arc<dyn Provider> {
        self.provider.clone()
    }
    fn deployment(&self) -> DeploymentRef {
        bot()
    }
    fn kill_externally<'a>(&'a self, replica: &'a ReplicaRef) -> BoxFuture<'a, Result<(), String>> {
        self.cluster.kill_pod(NAMESPACE, &replica.id);
        Box::pin(async { Ok(()) })
    }
    fn fake_server(&self) -> Option<&FakeServer> {
        Some(&self.server)
    }
    fn secrets(&self) -> Vec<String> {
        vec![BEARER.into()]
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_harness_c1_to_c13() {
    let cluster = cluster(FakeOptions::default()).await;
    let server = FakeServer::start(cluster.scripted_handler()).await.unwrap();
    let token: Arc<dyn TokenSource> = Arc::new(StaticToken(SecretString::new(BEARER)));
    let provider = Arc::new(provider_at(server.base_url(), Some(token), Duration::from_secs(5)));
    let target = KindTarget { cluster, server, provider };
    let config = ConformanceConfig { release_settle: Duration::from_secs(10), ..ConformanceConfig::default() };
    let report = run_conformance(&target, &config).await;
    eprintln!("{report}");
    report.assert_no_failures();
    for case in [CaseId::C1, CaseId::C2, CaseId::C4, CaseId::C5, CaseId::C8, CaseId::C9, CaseId::C10, CaseId::C12] {
        assert!(matches!(report.outcome(case), Some(CaseOutcome::Passed(_))), "{case} not passed:\n{report}");
    }
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
        assert_eq!(port_of(&endpoints[0].address), 8080);
    }
    assert_eq!(fake.replicas(NAMESPACE, "support-bot-v3"), Some(1));
    assert_eq!(fake.pods(NAMESPACE, "support-bot-v3").len(), 1);
    assert_eq!(fake.scale_patches(), 1, "singleflight: one scale patch for 20 wakes");
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
    provider.release(&victim, ReleaseMode::ScaleIn).await.unwrap();
    assert_eq!(fake.replicas(NAMESPACE, "support-bot-v3"), Some(2));
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

#[tokio::test]
async fn targets_outside_the_namespace_are_refused() {
    let fake = cluster(FakeOptions::default()).await;
    let provider = provider(&fake);
    let foreign = DeploymentRef::new("support-bot", "v3", "kube-system/coredns");
    assert_eq!(provider.wake(&foreign, 1).await.unwrap_err().kind, ProviderErrorKind::Fatal);
    assert!(fake.requests().is_empty(), "nothing may be sent for a foreign target");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c5_replica_killed_outside_the_gateway_is_detected() {
    let fake = cluster(FakeOptions::default()).await;
    let provider = provider(&fake);
    let endpoints = provider.wake(&bot(), 2).await.unwrap();
    let (sender, mut changes) = tokio::sync::mpsc::unbounded_channel::<Vec<String>>();
    let mut tracker = provider.readiness(&bot()).unwrap();
    let follower = tokio::spawn(async move {
        tracker
            .follow(|ready| {
                let _closed = sender.send(ready.iter().map(|address| address.pod.clone()).collect());
            })
            .await
    });
    assert_eq!(changes.recv().await.unwrap().len(), 2);
    let killed = endpoints[0].replica.id.clone();
    let killed_at = Instant::now();
    fake.kill_pod(NAMESPACE, &killed);
    let detected = loop {
        let ready = tokio::time::timeout(Duration::from_secs(15), changes.recv()).await.unwrap().unwrap();
        if !ready.contains(&killed) {
            break killed_at.elapsed();
        }
    };
    eprintln!("C5 killed replica detected by the watch after {detected:?}");
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

/// First token is stale, every refresh returns the current one.
#[derive(Debug)]
struct RotatingToken {
    fetches: AtomicUsize,
}

impl TokenSource for RotatingToken {
    fn token(&self, force_refresh: bool) -> BoxFuture<'_, Result<SecretString, ProviderError>> {
        let first = self.fetches.load(Ordering::SeqCst) == 0;
        if first || force_refresh {
            self.fetches.fetch_add(1, Ordering::SeqCst);
        }
        let token = if first { "token-old" } else { "token-new" };
        Box::pin(async move { Ok(SecretString::new(token)) })
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
    let provider = provider_at(fake.url(), Some(source.clone()), Duration::from_secs(3));
    assert_eq!(provider.wake(&bot(), 1).await.unwrap().len(), 1);
    assert_eq!(source.fetches.load(Ordering::SeqCst), 2, "one refresh after the 401");
    let bearers: Vec<_> = fake.requests().into_iter().filter_map(|request| request.bearer).collect();
    assert_eq!(bearers.iter().filter(|bearer| *bearer == "token-old").count(), 1);

    fake.set_accepted_tokens(Some(HashSet::new()));
    let error = provider.list(&bot()).await.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Auth, "{error:?}");
    let printed = format!("{error} {error:?} {provider:?}");
    assert!(!printed.contains("token-new") && !printed.contains("token-old"), "{printed}");
}

#[tokio::test]
async fn c9_transient_errors_back_off_and_classify() {
    let fake = cluster(FakeOptions::default()).await;
    let provider = provider(&fake);
    fake.inject_faults([(503, None), (429, Some(0)), (500, None)]);
    assert_eq!(provider.wake(&bot(), 1).await.unwrap().len(), 1);
    fake.inject_faults([(500, None); 4]);
    assert_eq!(provider.list(&bot()).await.unwrap_err().kind, ProviderErrorKind::Transient);
    fake.inject_faults([(429, Some(0)); 4]);
    assert_eq!(provider.list(&bot()).await.unwrap_err().kind, ProviderErrorKind::RateLimited);
    fake.inject_faults([(403, None)]);
    assert_eq!(provider.list(&bot()).await.unwrap_err().kind, ProviderErrorKind::Auth, "403 is not retried");
    let missing = DeploymentRef::new("nope", "v1", "agents/nope-v1");
    assert_eq!(provider.wake(&missing, 1).await.unwrap_err().kind, ProviderErrorKind::NotFound);
}

#[tokio::test]
async fn c9_unschedulable_and_quota_are_capacity_errors() {
    let unschedulable = cluster(FakeOptions { capacity: Some(1), ..FakeOptions::default() }).await;
    let provider = provider_at(unschedulable.url(), None, Duration::from_millis(300));
    let error = provider.wake(&bot(), 2).await.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Capacity, "{error:?}");
    assert!(error.message.contains("insufficient cpu"), "{error:?}");
    assert_eq!(provider.wake(&bot(), 1_000).await.unwrap_err().kind, ProviderErrorKind::Capacity);

    let quota = cluster(FakeOptions { quota: Some(1), ..FakeOptions::default() }).await;
    let provider = provider_at(quota.url(), None, Duration::from_millis(300));
    let error = provider.wake(&bot(), 2).await.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Capacity, "{error:?}");
    assert!(error.message.contains("exceeded quota"), "{error:?}");
}

fn spec() -> DeploymentSpec {
    DeploymentSpec {
        name: "helpdesk".into(),
        version: "v1".into(),
        image: Some("ghcr.io/bimapangestu28/graphrt:0.5.0".into()),
        command: vec!["graphrt".into(), "serve".into(), "--trust-gateway".into(), "/app/workflow.yaml".into()],
        env: [("RUST_LOG".to_owned(), "info".to_owned())].into(),
        secrets: vec![SecretRef { env: "OPENAI_API_KEY".into(), reference: "helpdesk-llm".into() }],
        max_active_runs: 8,
        min_replicas: 0,
        max_replicas: 10,
        grace_secs: 120,
        resources: Resources { cpu_millis: 0, memory_mib: 128 },
        workflow_fingerprint: "sha256:abc".into(),
        provider_settings: Default::default(),
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
    assert_eq!(deployed, DeploymentRef::new("helpdesk", "v1", "agents/helpdesk-v1"));
    let manifest = fake.applied(NAMESPACE, "deployments", "helpdesk-v1").unwrap();
    assert_eq!(manifest["spec"]["replicas"], 0, "new versions start at min_replicas");
    let text = manifest.to_string();
    assert!(!text.contains("sk-live") && text.contains("helpdesk-llm"), "{text}");
    assert!(fake.applied(NAMESPACE, "services", "helpdesk-v1").is_some());

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
    provider.wake(&bot(), 2).await.unwrap();
    let replicas = provider.list(&bot()).await.unwrap();
    assert_eq!(replicas.len(), 2);
    assert!(replicas.iter().all(|replica| replica.phase == ReplicaPhase::Ready && replica.address.is_some()));
    for replica in &replicas {
        provider.release(&replica.replica, ReleaseMode::Stop).await.unwrap();
    }
    assert!(provider.list(&bot()).await.unwrap().is_empty());
    assert_eq!(fake.replicas(NAMESPACE, "support-bot-v3"), Some(0));
}
