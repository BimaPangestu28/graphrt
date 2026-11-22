//! Every provider in the loop: the gateway (supervisor, pool, SSE proxy)
//! drives the real provider implementation against its crate's fake cloud API
//! (K8s API server, Cloud Run Admin API + metadata server, ECS JSON 1.1, Fly
//! Machines, and a Lambda `InvokeWithResponseStream` fake defined here), and
//! the C1–C13 harness runs with the gateway cases C3, C6, C7, C11, C12.
//!
//! The fake platforms report private addresses (pod / task / 6PN IPs) the test
//! cannot reach, so [`Routed`] rewrites the HTTP endpoints the provider
//! returns to one loopback fake protocol-1 agent. Wake, list and release still
//! go through the provider and its fake API; Lambda needs no rewrite: its
//! requests go through `Provider::invoke` to the fake, which forwards them to
//! the agent and streams the reply back as `vnd.amazon.eventstream`.
//!
//! Run with `--features deploy-k8s,deploy-cloud-run,deploy-ecs,deploy-lambda,deploy-fly`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::print_stdout)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{AgentWorld, DEPLOYMENT, Options, TestGateway, serve_agent, start_gateway, version};
use graphrt_core::store::now_ms;
use graphrt_provider_api::conformance::{CaseId, CaseOutcome, ConformanceConfig, ConformanceTarget, GatewayHarness, HookResult, run_conformance};
use graphrt_provider_api::{
    Addressing, BoxFuture, Capabilities, DeploymentRef, Endpoint, EndpointAddress, InvokeRequest, InvokeResponse, Provider, ProviderError,
    ReleaseMode, ReplicaRef, ReplicaState,
};
use serde_json::json;

const DAY_MS: u64 = 24 * 3600 * 1000;

/// Rewrites HTTP endpoints of `inner` to `agent_url` (see the module docs).
struct Routed {
    inner: Arc<dyn Provider>,
    agent_url: String,
}

impl Routed {
    fn rewrite(&self, address: EndpointAddress) -> EndpointAddress {
        match address {
            EndpointAddress::Http(url) if !url.starts_with("lambda://") => EndpointAddress::Http(self.agent_url.clone()),
            other => other,
        }
    }
}

impl Provider for Routed {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn wake<'a>(&'a self, deployment: &'a DeploymentRef, replicas: u32) -> BoxFuture<'a, Result<Vec<Endpoint>, ProviderError>> {
        Box::pin(async move {
            let endpoints = self.inner.wake(deployment, replicas).await?;
            Ok(endpoints.into_iter().map(|endpoint| Endpoint { address: self.rewrite(endpoint.address), replica: endpoint.replica }).collect())
        })
    }
    fn release<'a>(&'a self, replica: &'a ReplicaRef, mode: ReleaseMode) -> BoxFuture<'a, Result<(), ProviderError>> {
        self.inner.release(replica, mode)
    }
    fn list<'a>(&'a self, deployment: &'a DeploymentRef) -> BoxFuture<'a, Result<Vec<ReplicaState>, ProviderError>> {
        Box::pin(async move {
            let rows = self.inner.list(deployment).await?;
            Ok(rows.into_iter().map(|row| ReplicaState { address: row.address.map(|address| self.rewrite(address)), ..row }).collect())
        })
    }
    fn invoke<'a>(&'a self, endpoint: &'a Endpoint, request: InvokeRequest) -> Option<BoxFuture<'a, Result<InvokeResponse, ProviderError>>> {
        self.inner.invoke(endpoint, request)
    }
}

fn runs() -> String {
    format!("/v1/deployments/{DEPLOYMENT}/runs")
}

/// The gateway-in-the-loop cases for one provider.
struct Harness {
    gateway: TestGateway,
    provider: Arc<dyn Provider>,
    reference: DeploymentRef,
    /// The platform ends requests itself at this many seconds (Lambda's cap in the fake).
    expects_platform_timeout: bool,
}

impl Harness {
    async fn grace(&self) -> HookResult {
        if !self.provider.capabilities().can_preempt() {
            return Ok("no grace on this platform: runs are never preempted (C7 covers the cap)".into());
        }
        let (_, record) = self.gateway.post(&runs(), json!({ "inputs": { "mode": "preempt_once", "events": 3 } })).await;
        if record["status"] != "succeeded" || record["outputs"]["executions"] != 2 {
            return Err(format!("drained run not continued: {record}"));
        }
        Ok("drained run re-dispatched through the provider and finished (2 executions, 0 lost)".into())
    }

    async fn long_stream(&self) -> HookResult {
        let started = tokio::time::Instant::now();
        let mut response = self
            .gateway
            .client()
            .post(format!("{}{}", self.gateway.url, runs()))
            .header("x-api-key", common::API_KEY)
            .json(&json!({ "stream": true, "inputs": { "events": 30, "delay_ms": 25 } }))
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let mut arrivals = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
            if chunk.windows(4).any(|window| window == b"id: ") {
                arrivals.push(started.elapsed());
            }
        }
        let mut gaps: Vec<Duration> = arrivals.windows(2).map(|pair| pair[1].saturating_sub(pair[0])).collect();
        gaps.sort();
        let p95 = graphrt_provider_api::conformance::percentile(&gaps, 95.0);
        let first = arrivals.first().copied().unwrap_or_default();
        if arrivals.len() < 30 || first > Duration::from_millis(1500) || p95 > Duration::from_millis(75) {
            return Err(format!("{} chunks, first after {first:?}, inter-event p95 {p95:?}: buffered or slow", arrivals.len()));
        }
        Ok(format!("{} chunks, inter-event p95 {:.1} ms vs 25 ms emitted, first after {:.0} ms", arrivals.len(), p95.as_secs_f64() * 1000.0, first.as_secs_f64() * 1000.0))
    }

    async fn request_cap(&self) -> HookResult {
        let capabilities = self.provider.capabilities();
        match capabilities.max_request_secs {
            None => Ok("no request cap on this platform".into()),
            Some(_) if self.expects_platform_timeout => {
                let (_, record) = self.gateway.post(&runs(), json!({ "inputs": { "events": 5, "delay_ms": 400 } })).await;
                if record["error_type"] != "platform_timeout" {
                    return Err(format!("expected failed{{platform_timeout}}: {record}"));
                }
                Ok("run past the platform cap → failed{platform_timeout}, no silent kill, no re-dispatch".into())
            }
            Some(cap) if !capabilities.can_preempt() => Err(format!("cap {cap} s without grace and no platform timeout seen")),
            Some(cap) => Err(format!("planned preemption before the {cap} s request cap is not implemented in the gateway yet")),
        }
    }

    async fn old_versions(&self) -> HookResult {
        let deployment = &self.gateway.deployment;
        let (_, paused) = self.gateway.post(&runs(), json!({ "inputs": { "mode": "hitl" } })).await;
        let run_id = paused["run_id"].as_str().ok_or(format!("no run id: {paused}"))?.to_owned();
        let mut next = version("v2");
        next.target.clone_from(&self.reference.target);
        deployment.add_version(next, now_ms() - 6 * DAY_MS);
        let (status, _) = self.gateway.post(&format!("{}/{run_id}/resume", runs()), json!({ "inputs": {} })).await;
        let done = self.gateway.wait_settled(&run_id).await;
        if status != 202 || done["status"] != "succeeded" || done["version"] != "v1" {
            return Err(format!("day-6 resume on v1: {status} {done}"));
        }
        Ok("paused run of v1 resumed on v1's pool 6 days after v2 (410 after day 7: gateway_harness.rs)".into())
    }

    async fn idle(&self) -> HookResult {
        if self.provider.capabilities().addressing == Addressing::Url {
            return Ok("platform scales to zero itself (Url addressing)".into());
        }
        for version in self.gateway.deployment.versions() {
            version.pool.release_idle().await;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        for version in self.gateway.deployment.versions() {
            version.pool.release_idle().await;
            version.pool.reconcile().await.map_err(|error| error.to_string())?;
        }
        let live = |rows: Vec<ReplicaState>| rows.iter().filter(|row| matches!(row.phase, graphrt_provider_api::ReplicaPhase::Ready | graphrt_provider_api::ReplicaPhase::Starting)).count();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = live(self.provider.list(&self.reference).await.map_err(|error| error.to_string())?);
            if remaining == 0 {
                return Ok("idle → 0 replicas on the platform after the idle timeout".into());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!("{remaining} replicas still live after the idle timeout"));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

impl GatewayHarness for Harness {
    fn grace_migration(&self) -> BoxFuture<'_, HookResult> {
        Box::pin(self.grace())
    }
    fn long_stream(&self) -> BoxFuture<'_, HookResult> {
        Box::pin(self.long_stream())
    }
    fn request_cap(&self) -> BoxFuture<'_, HookResult> {
        Box::pin(self.request_cap())
    }
    fn old_version_pool(&self) -> BoxFuture<'_, HookResult> {
        Box::pin(self.old_versions())
    }
    fn idle_and_orphans(&self) -> BoxFuture<'_, HookResult> {
        Box::pin(self.idle())
    }
}

struct Target {
    name: String,
    harness: Harness,
}

impl ConformanceTarget for Target {
    fn name(&self) -> String {
        self.name.clone()
    }
    fn provider(&self) -> Arc<dyn Provider> {
        self.harness.provider.clone()
    }
    fn deployment(&self) -> DeploymentRef {
        self.harness.reference.clone()
    }
    fn gateway_harness(&self) -> Option<&dyn GatewayHarness> {
        Some(&self.harness)
    }
}

/// Runs the suite for `provider` behind a gateway and checks the gateway cases.
async fn check(name: &str, world: Arc<AgentWorld>, provider: Arc<dyn Provider>, target: &str, expects_platform_timeout: bool, failing_by_design: &[CaseId]) {
    let pool = graphrt_gateway::pool::PoolConfig { idle_timeout: Duration::from_millis(300), max_active_runs: 16, ..Default::default() };
    let gateway = start_gateway(Options { pool, world, provider: Some(provider.clone()), target: target.to_owned(), ..Options::default() }).await;
    let reference = DeploymentRef::new(DEPLOYMENT, "v1", target);
    let target = Target { name: name.to_owned(), harness: Harness { gateway, provider, reference, expects_platform_timeout } };
    let config = ConformanceConfig { latency_samples: 5, poll_interval: Duration::from_millis(20), ..ConformanceConfig::default() };
    let report = run_conformance(&target, &config).await;
    println!("{report}");
    for result in &report.results {
        let expected_failure = failing_by_design.contains(&result.id);
        match (&result.outcome, expected_failure) {
            (CaseOutcome::Failed(reason), false) => panic!("{name} {}: {reason}\n{report}", result.id),
            (CaseOutcome::Failed(_), true) => {}
            (_, true) => panic!("{name} {}: expected to fail (known gap), got {:?}", result.id, result.outcome),
            _ => {}
        }
    }
    for id in [CaseId::C3, CaseId::C6, CaseId::C11, CaseId::C12] {
        assert!(matches!(report.outcome(id), Some(CaseOutcome::Passed(_))), "{name} {id}: {:?}", report.outcome(id));
    }
}

async fn agent() -> (Arc<AgentWorld>, String, tokio::task::JoinHandle<()>) {
    graphrt_gateway::install_crypto_provider();
    let world = Arc::new(AgentWorld::default());
    let (address, task) = serve_agent(world.clone(), "agent-1").await;
    (world, format!("http://{address}"), task)
}

#[cfg(feature = "gw-k8s")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kubernetes_behind_the_gateway() {
    use graphrt_provider_k8s::auth::ClusterConfig;
    use graphrt_provider_k8s::fake::{FakeApiServer, FakeOptions};
    use graphrt_provider_k8s::{ClientOptions, K8sProvider, K8sProviderConfig, KubeClient};
    let (world, agent_url, _agent) = agent().await;
    let fake = FakeApiServer::start(FakeOptions::default()).await.unwrap();
    let reference = DeploymentRef::new(DEPLOYMENT, "v1", "");
    let object = graphrt_provider_k8s::names::object_name(&reference);
    fake.add_deployment("agents", &object, DEPLOYMENT, "v1", 8080);
    fake.add_deployment("agents", &graphrt_provider_k8s::names::object_name(&DeploymentRef::new(DEPLOYMENT, "v2", "")), DEPLOYMENT, "v2", 8080);
    let mut config = K8sProviderConfig::new("agents");
    config.wake_timeout = Duration::from_secs(3);
    let client = KubeClient::new(&ClusterConfig::new(fake.url()), ClientOptions::default()).unwrap();
    let inner: Arc<dyn Provider> = Arc::new(K8sProvider::new(client, config));
    let provider: Arc<dyn Provider> = Arc::new(Routed { inner, agent_url });
    check("k8s (fake API server)", world, provider, "", false, &[]).await;
    assert!(fake.scale_patches() > 0, "wakes went through the scale subresource");
}

#[cfg(feature = "deploy-cloud-run")]
#[allow(dead_code, unused_imports, clippy::all, clippy::pedantic)]
#[path = "../../graphrt-gcp/tests/support/mod.rs"]
mod gcp_support;

#[cfg(feature = "deploy-cloud-run")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cloud_run_behind_the_gateway() {
    use graphrt_gcp::cloud_run::{CloudRunDeployDriver, CloudRunProvider};
    use graphrt_provider_api::DeployDriver as _;
    let (world, agent_url, _agent) = agent().await;
    // The fake service URL is the fake agent: Cloud Run needs no address rewrite.
    let google = gcp_support::FakeGoogle::start(&agent_url).await;
    let mut spec = gcp_support::spec(DEPLOYMENT, "v1");
    spec.secrets.clear();
    CloudRunDeployDriver::new(google.config(), &google.service_account_auth()).unwrap().apply(&spec).await.unwrap();
    let mut config = google.config();
    config.warm_window = Duration::ZERO;
    let inner = CloudRunProvider::new(config, google.metadata_auth()).unwrap();
    let provider: Arc<dyn Provider> = Arc::new(graphrt_gateway::cloud_run::CloudRunGatewayProvider::new(inner).unwrap());
    let target = gcp_support::service_resource(DEPLOYMENT);
    // C7 fails by design: planned preemption before Cloud Run's 3600 s cap is a known gap.
    check("cloud run (fake Google APIs)", world.clone(), provider, &target, false, &[CaseId::C7]).await;
    let invoked = world.bodies.lock().unwrap().len();
    assert!(invoked > 0, "runs reached the agent through the Cloud Run invoke path");
}

#[cfg(any(feature = "gw-ecs", feature = "gw-lambda"))]
#[allow(dead_code, unused_imports, clippy::all, clippy::pedantic)]
#[path = "../../graphrt-provider-aws/tests/support/mod.rs"]
mod aws_support;

#[cfg(feature = "gw-ecs")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ecs_behind_the_gateway() {
    use std::collections::HashMap;

    use graphrt_provider_aws::ecs::{EcsConfig, EcsProvider, EcsTaskTemplate};
    let (world, agent_url, _agent) = agent().await;
    let fake = aws_support::start().await;
    let template = EcsTaskTemplate {
        cluster: "graphrt".into(),
        task_definition_family: "graphrt-agent".into(),
        subnets: vec!["subnet-1".into()],
        security_groups: vec!["sg-1".into()],
        assign_public_ip: false,
        container_port: 8080,
        capacity_provider: None,
        platform_version: None,
    };
    let config = EcsConfig {
        endpoint: fake.endpoint("ecs"),
        limits: aws_support::fast_retry(),
        deployments: HashMap::from([(DEPLOYMENT.to_owned(), template)]),
        wake_timeout: Duration::from_secs(5),
        poll_interval: Duration::from_millis(10),
    };
    let inner: Arc<dyn Provider> = Arc::new(EcsProvider::new(fake.chain(), config).unwrap());
    let provider: Arc<dyn Provider> = Arc::new(Routed { inner, agent_url });
    check("ecs fargate (fake ECS API)", world, provider, "", false, &[]).await;
    assert!(fake.calls("RunTask") > 0);
}

#[cfg(feature = "gw-fly")]
#[allow(dead_code, unused_imports, clippy::all, clippy::pedantic)]
#[path = "../../graphrt-provider-fly/tests/support/mod.rs"]
mod fly_support;

#[cfg(feature = "gw-fly")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fly_behind_the_gateway() {
    use graphrt_provider_fly::{FlyProvider, FlyProviderConfig, FlyToken, TokenOrigin};
    let (world, agent_url, _agent) = agent().await;
    let fake = fly_support::FakeFly::start().await;
    for version_id in ["v1", "v2"] {
        for index in 0..3 {
            fake.seed_machine(DEPLOYMENT, version_id, index, "created", 512);
        }
    }
    let mut config = FlyProviderConfig::single_app(fly_support::APP, TokenOrigin::Fixed(FlyToken::parse(fly_support::TOKEN).unwrap()));
    config.api_base_url = fake.server.base_url();
    config.max_machines_per_version = 3;
    config.start_timeout = Duration::from_secs(5);
    config.client_policy.requests_per_second = 10_000.0;
    config.client_policy.burst = 10_000;
    let inner: Arc<dyn Provider> = Arc::new(FlyProvider::new(config).unwrap());
    let provider: Arc<dyn Provider> = Arc::new(Routed { inner, agent_url });
    check("fly (fake Machines API)", world, provider, fly_support::APP, false, &[]).await;
}

/// A Lambda `InvokeWithResponseStream` endpoint that unwraps the Function URL
/// event, forwards it to the agent and streams its reply as eventstream frames
/// (prelude + 8 NUL, body chunks, `InvokeComplete`), like the Lambda Web Adapter.
/// `cap` makes it end the invocation with `Sandbox.Timedout` after that long.
#[cfg(feature = "gw-lambda")]
async fn fake_lambda(agent_url: String, cap: Duration) -> String {
    use axum::body::{Body, Bytes};
    use axum::extract::State;
    use axum::response::Response;
    use base64::Engine as _;
    use graphrt_provider_aws::lambda::eventstream::encode;

    fn frame(kind: &str, payload: &[u8]) -> Bytes {
        Bytes::from(encode(&[(":event-type", kind), (":message-type", "event"), (":content-type", "application/octet-stream")], payload))
    }

    async fn invoke(State((agent_url, cap)): State<(String, Duration)>, body: Bytes) -> Response {
        let event: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let path = event["rawPath"].as_str().unwrap().to_owned();
        let query = event["rawQueryString"].as_str().unwrap_or_default();
        let url = if query.is_empty() { format!("{agent_url}{path}") } else { format!("{agent_url}{path}?{query}") };
        let payload = base64::engine::general_purpose::STANDARD.decode(event["body"].as_str().unwrap_or_default()).unwrap();
        let method = reqwest::Method::from_bytes(event["requestContext"]["http"]["method"].as_str().unwrap().as_bytes()).unwrap();
        let mut request = reqwest::Client::new().request(method, url).body(payload);
        for (name, value) in event["headers"].as_object().unwrap() {
            request = request.header(name.as_str(), value.as_str().unwrap());
        }
        let mut reply = request.send().await.unwrap();
        let (sender, receiver) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(64);
        tokio::spawn(async move {
            let mut prelude = serde_json::to_vec(&json!({ "statusCode": reply.status().as_u16(), "headers": { "content-type": "text/event-stream" } })).unwrap();
            prelude.extend_from_slice(&[0u8; 8]);
            let _ = sender.send(Ok(frame("PayloadChunk", &prelude))).await;
            let deadline = tokio::time::Instant::now() + cap;
            loop {
                match tokio::time::timeout_at(deadline, reply.chunk()).await {
                    Ok(Ok(Some(chunk))) => {
                        if sender.send(Ok(frame("PayloadChunk", &chunk))).await.is_err() {
                            return;
                        }
                    }
                    Ok(_) => break,
                    Err(_) => {
                        let timeout = json!({ "ErrorCode": "Sandbox.Timedout", "ErrorDetails": "Task timed out", "LogResult": "" });
                        let _ = sender.send(Ok(frame("InvokeComplete", timeout.to_string().as_bytes()))).await;
                        return;
                    }
                }
            }
            let _ = sender.send(Ok(frame("InvokeComplete", b"{}"))).await;
        });
        let stream = futures::stream::unfold(receiver, |mut receiver| async move { receiver.recv().await.map(|item| (item, receiver)) });
        Response::builder().status(200).header("content-type", "application/vnd.amazon.eventstream").body(Body::from_stream(stream)).unwrap()
    }

    let app = axum::Router::new().route("/2021-11-15/functions/{function}/response-streaming-invocations", axum::routing::post(invoke)).with_state((agent_url, cap));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{address}")
}

#[cfg(feature = "gw-lambda")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lambda_behind_the_gateway() {
    use std::collections::HashMap;

    use graphrt_provider_aws::lambda::{LambdaConfig, LambdaFunction, LambdaProvider};
    let (world, agent_url, _agent) = agent().await;
    // The platform ends an invocation after 1.5 s here (Lambda's 15 min cap).
    let lambda_url = fake_lambda(agent_url, Duration::from_millis(1500)).await;
    let credentials = aws_support::start().await;
    let config = LambdaConfig {
        endpoint: graphrt_provider_aws::rest::ServiceEndpoint { base_url: lambda_url, region: "eu-west-1".into(), service: "lambda".into() },
        limits: aws_support::fast_retry(),
        functions: HashMap::from([(DEPLOYMENT.to_owned(), LambdaFunction { function_name: "graphrt-shop".into() })]),
        max_run_secs: 840,
        buffering_threshold: Duration::from_secs(20),
    };
    let provider: Arc<dyn Provider> = Arc::new(LambdaProvider::new(credentials.chain(), config).unwrap());
    check("lambda (fake InvokeWithResponseStream)", world.clone(), provider, "", true, &[]).await;
    assert!(!world.bodies.lock().unwrap().is_empty(), "runs reached the agent through Provider::invoke");
}
