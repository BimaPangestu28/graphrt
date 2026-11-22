//! The harness against the in-memory reference provider, and against a small
//! REST provider talking to the fake server through `ProviderHttp`; the latter
//! is the template provider crates follow.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::print_stdout)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use graphrt_provider_api::conformance::{
    CaseId, CaseOutcome, ConformanceConfig, ConformanceTarget, FakeResponse, FakeServer, Fault, MemoryProvider,
    RecordedRequest, run_conformance,
};
use graphrt_provider_api::http::{
    JitterRng, ProviderHttp, RetryPolicy, SecretString, StaticToken, TokenBucket, TokenSource,
};
use graphrt_provider_api::{
    Addressing, BoxFuture, Capabilities, DeploymentRef, Endpoint, EndpointAddress, Provider, ProviderError,
    ProviderErrorKind, ReleaseMode, ReplicaPhase, ReplicaRef, ReplicaState, Scaler,
};

fn deployment() -> DeploymentRef {
    DeploymentRef::new("support-bot", "v1", "fake/support-bot")
}

struct MemoryTarget(Arc<MemoryProvider>);

impl ConformanceTarget for MemoryTarget {
    fn name(&self) -> String {
        "memory".to_owned()
    }
    fn provider(&self) -> Arc<dyn Provider> {
        self.0.clone()
    }
    fn deployment(&self) -> DeploymentRef {
        deployment()
    }
    fn kill_externally<'a>(&'a self, replica: &'a ReplicaRef) -> BoxFuture<'a, Result<(), String>> {
        self.0.kill_externally(replica);
        Box::pin(async { Ok(()) })
    }
}

fn fast_config() -> ConformanceConfig {
    ConformanceConfig {
        latency_samples: 10,
        poll_interval: Duration::from_millis(5),
        size_delta: Some(("gw-k8s".to_owned(), 90 * 1024)),
        ..ConformanceConfig::default()
    }
}

#[tokio::test]
async fn memory_provider_passes_provider_level_cases() {
    let target = MemoryTarget(Arc::new(MemoryProvider::new(Duration::from_millis(2))));
    let report = run_conformance(&target, &fast_config()).await;
    println!("{report}");
    report.assert_no_failures();
    for id in [CaseId::C1, CaseId::C2, CaseId::C4, CaseId::C5, CaseId::C12, CaseId::C13] {
        assert!(matches!(report.outcome(id), Some(CaseOutcome::Passed(_))), "{id} should pass");
    }
    for id in [CaseId::C3, CaseId::C6, CaseId::C7, CaseId::C11] {
        assert!(matches!(report.outcome(id), Some(CaseOutcome::Skipped(_))), "{id} needs the gateway");
    }
}

#[tokio::test]
async fn size_over_budget_fails_c13() {
    let target = MemoryTarget(Arc::new(MemoryProvider::new(Duration::from_millis(1))));
    let config = ConformanceConfig { size_delta: Some(("gw-fly".to_owned(), 200 * 1024)), ..fast_config() };
    let report = run_conformance(&target, &config).await;
    assert!(matches!(report.outcome(CaseId::C13), Some(CaseOutcome::Failed(_))));
}

/// The fake platform: a map of replica id → phase behind a tiny REST API
/// (`GET /replicas`, `POST /replicas` with a count, `DELETE /replicas/{id}`).
#[derive(Default)]
struct FakePlatform {
    replicas: BTreeMap<String, &'static str>,
    next: u32,
}

fn platform_handler(platform: Arc<Mutex<FakePlatform>>) -> Arc<dyn Fn(&RecordedRequest) -> FakeResponse + Send + Sync> {
    Arc::new(move |request: &RecordedRequest| {
        if request.header("authorization").is_none_or(|value| !value.starts_with("Bearer ")) {
            return FakeResponse::status(401);
        }
        let mut platform = platform.lock().unwrap();
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/replicas") => {}
            ("POST", "/replicas") => {
                let wanted: usize = request.body_text().trim().parse().unwrap_or(1);
                while platform.replicas.values().filter(|phase| **phase == "ready").count() < wanted {
                    platform.next += 1;
                    let id = format!("r{}", platform.next);
                    platform.replicas.insert(id, "ready");
                }
            }
            ("DELETE", path) if path.starts_with("/replicas/") => {
                platform.replicas.remove(&path["/replicas/".len()..]);
            }
            _ => return FakeResponse::status(404),
        }
        let listing: Vec<_> =
            platform.replicas.iter().map(|(id, phase)| serde_json::json!({"id": id, "phase": phase})).collect();
        FakeResponse::json(200, &serde_json::Value::Array(listing))
    })
}

/// A REST provider in the shape provider crates take.
struct RestProvider {
    http: ProviderHttp,
    base_url: String,
    token: StaticToken,
}

fn capacity_classifier(status: u16, body: &str) -> ProviderErrorKind {
    if status == 503 && body.contains("INSUFFICIENT_CAPACITY") {
        return ProviderErrorKind::Capacity;
    }
    ProviderError::kind_for_status(status)
}

impl RestProvider {
    async fn replicas(
        &self,
        method: reqwest::Method,
        path: &str,
        body: String,
    ) -> Result<Vec<ReplicaState>, ProviderError> {
        let url = format!("{}{path}", self.base_url);
        let token: &dyn TokenSource = &self.token;
        let response =
            self.http.send(|client| client.request(method.clone(), &url).body(body.clone()), Some(token)).await?;
        let bytes = response.bytes().await.map_err(|error| ProviderError::transient(error.to_string()))?;
        let listing: Vec<serde_json::Value> =
            serde_json::from_slice(&bytes).map_err(|error| ProviderError::fatal(error.to_string()))?;
        Ok(listing
            .iter()
            .map(|row| {
                let id = row["id"].as_str().unwrap_or_default().to_owned();
                ReplicaState {
                    address: Some(EndpointAddress::Http(format!("http://{id}.internal"))),
                    replica: ReplicaRef { deployment: deployment(), id },
                    phase: if row["phase"] == "ready" { ReplicaPhase::Ready } else { ReplicaPhase::Failed },
                    started_at_ms: None,
                }
            })
            .collect())
    }
}

impl Provider for RestProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            addressing: Addressing::Replica,
            scaler: Scaler::Gateway,
            max_request_secs: None,
            idle_timeout_secs: None,
            max_grace_secs: 120,
            background_cpu: true,
            supports_suspend: false,
            wake_p95_target_ms: 1000,
        }
    }
    fn wake<'a>(
        &'a self,
        _deployment: &'a DeploymentRef,
        replicas: u32,
    ) -> BoxFuture<'a, Result<Vec<Endpoint>, ProviderError>> {
        Box::pin(async move {
            let states = self.replicas(reqwest::Method::POST, "/replicas", replicas.to_string()).await?;
            Ok(states
                .into_iter()
                .filter(|state| state.phase == ReplicaPhase::Ready)
                .filter_map(|state| Some(Endpoint { address: state.address?, replica: state.replica }))
                .collect())
        })
    }
    fn release<'a>(&'a self, replica: &'a ReplicaRef, _mode: ReleaseMode) -> BoxFuture<'a, Result<(), ProviderError>> {
        Box::pin(async move {
            self.replicas(reqwest::Method::DELETE, &format!("/replicas/{}", replica.id), String::new()).await?;
            Ok(())
        })
    }
    fn list<'a>(&'a self, _deployment: &'a DeploymentRef) -> BoxFuture<'a, Result<Vec<ReplicaState>, ProviderError>> {
        Box::pin(self.replicas(reqwest::Method::GET, "/replicas", String::new()))
    }
}

struct RestTarget {
    server: FakeServer,
    platform: Arc<Mutex<FakePlatform>>,
    provider: Arc<RestProvider>,
}

impl ConformanceTarget for RestTarget {
    fn name(&self) -> String {
        "rest-fake".to_owned()
    }
    fn provider(&self) -> Arc<dyn Provider> {
        self.provider.clone()
    }
    fn deployment(&self) -> DeploymentRef {
        deployment()
    }
    fn kill_externally<'a>(&'a self, replica: &'a ReplicaRef) -> BoxFuture<'a, Result<(), String>> {
        self.platform.lock().unwrap().replicas.insert(replica.id.clone(), "failed");
        Box::pin(async { Ok(()) })
    }
    fn fake_server(&self) -> Option<&FakeServer> {
        Some(&self.server)
    }
    fn capacity_fault(&self) -> Option<Fault> {
        let response = FakeResponse::json(503, &serde_json::json!({"error": "INSUFFICIENT_CAPACITY"}));
        // Answered for every retry attempt, so it must be classified, not retried away.
        Some(Fault { path_prefix: "/replicas".to_owned(), response, count: 10 })
    }
    fn secrets(&self) -> Vec<String> {
        vec!["sk-llm-key-never-seen-by-gateway".to_owned(), "deploy-token-123".to_owned()]
    }
}

#[tokio::test]
async fn rest_provider_on_fake_server_passes() {
    // The binary installs the crypto provider; tests do it themselves.
    let _already_installed = rustls::crypto::ring::default_provider().install_default();
    let platform = Arc::new(Mutex::new(FakePlatform::default()));
    let server = FakeServer::start(platform_handler(platform.clone())).await.unwrap();
    let http = ProviderHttp::new(reqwest::Client::new(), Arc::new(TokenBucket::new(500.0, 50)))
        .with_policy(RetryPolicy {
            max_attempts: 4,
            base_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(20),
        })
        .with_rng(Arc::new(JitterRng::seeded(9)))
        .with_classifier(capacity_classifier);
    let provider = Arc::new(RestProvider {
        http,
        base_url: server.base_url(),
        token: StaticToken(SecretString::new("deploy-token-123")),
    });
    let target = RestTarget { server, platform, provider };
    let report = run_conformance(&target, &fast_config()).await;
    println!("{report}");
    report.assert_no_failures();
    for id in [CaseId::C1, CaseId::C4, CaseId::C5, CaseId::C8, CaseId::C9, CaseId::C10] {
        assert!(matches!(report.outcome(id), Some(CaseOutcome::Passed(_))), "{id}: {:?}", report.outcome(id));
    }
}
