//! End-to-end: client → gateway → fake agents (protocol 1) → store.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use common::{API_KEY, AgentWorld, DEPLOYMENT, Options, start_gateway};
use graphrt_core::store::{RunStatus, RunStore};
use graphrt_gateway::pool::PoolConfig;
use graphrt_gateway::quota::Quotas;
use graphrt_gateway::store::SqliteStore;
use graphrt_gateway::webhook::{HookConfig, Scheme};
use serde_json::{Value as Json, json};

fn runs() -> String {
    format!("/v1/deployments/{DEPLOYMENT}/runs")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sync_run_needs_a_key_and_returns_outputs() {
    let gateway = start_gateway(Options::default()).await;
    let unauthorized =
        gateway.client().post(format!("{}{}", gateway.url, runs())).json(&json!({})).send().await.unwrap();
    assert_eq!(unauthorized.status().as_u16(), 401);
    let (status, record) = gateway.post(&runs(), json!({ "inputs": { "text": "hi", "api_token": "s3cr3t" } })).await;
    assert_eq!(status, 200, "{record}");
    assert_eq!(record["status"], "succeeded");
    assert_eq!(record["outputs"]["echo"], "hi");
    assert_eq!(record["event_count"], 4);
    let run_id = record["run_id"].as_str().unwrap();
    // I6: the store never holds the request inputs (secrets).
    let stored = gateway.gateway.store.get(run_id).await.unwrap().unwrap();
    assert!(stored.inputs.is_none());
    let events = gateway.gateway.store.events(run_id, 0).await.unwrap();
    assert!(events.iter().all(|event| !event.get().contains("s3cr3t")));
    assert_eq!(events.len(), 4);
    // The request is stored for a restart without its declared secret input.
    let request = gateway.gateway.store.load_request(run_id).await.unwrap().unwrap();
    assert_eq!(request, r#"{"body":{"inputs":{"text":"hi"}},"needs_secrets":["api_token"]}"#);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streams_have_gapless_ids_across_reconnects() {
    let gateway = start_gateway(Options::default()).await;
    let frames = gateway.sse(&runs(), Some(json!({ "stream": true, "inputs": { "events": 5 } })), None).await;
    let ids: Vec<String> = frames.iter().filter_map(|(id, _, _)| id.clone()).collect();
    let engine: Vec<u64> = ids.iter().filter(|id| !id.contains('.')).map(|id| id.parse().unwrap()).collect();
    assert_eq!(engine, (0..7).collect::<Vec<_>>(), "{ids:?}");
    assert!(ids.contains(&"1.0".to_owned()), "response chunks keep N.k ids");
    let run_id =
        frames.iter().find(|(_, event, _)| event == "run_started").unwrap().2["run_id"].as_str().unwrap().to_owned();
    // Reconnect after event 3: events 4.. only, from the ring or the store (I5).
    let replay = gateway.sse(&format!("{}/{run_id}/events", runs()), None, Some("3")).await;
    let replay_ids: Vec<String> =
        replay.iter().filter_map(|(id, _, _)| id.clone()).filter(|id| !id.contains('.')).collect();
    assert_eq!(replay_ids, vec!["4", "5", "6"]);
    // Mid-chunk resume: `4.0` seen, event 4 not yet.
    let replay = gateway.sse(&format!("{}/{run_id}/events", runs()), None, Some("4.0")).await;
    assert_eq!(replay.first().and_then(|(id, _, _)| id.clone()).as_deref(), Some("4"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hitl_pause_then_resume() {
    let gateway = start_gateway(Options::default()).await;
    let (status, record) = gateway.post(&runs(), json!({ "inputs": { "mode": "hitl" } })).await;
    assert_eq!(status, 200);
    assert_eq!(record["status"], "paused");
    let run_id = record["run_id"].as_str().unwrap().to_owned();
    assert!(gateway.gateway.store.load_snapshot(&run_id).await.unwrap().is_some());
    let (status, _) = gateway.post(&format!("{}/{run_id}/resume", runs()), json!({ "inputs": { "ask": "yes" } })).await;
    assert_eq!(status, 202);
    let done = gateway.wait_settled(&run_id).await;
    assert_eq!(done["status"], "succeeded");
    assert_eq!(done["outputs"]["echo"]["ask"], "yes");
    assert_eq!(done["epoch"], 2);
    // A second resume of the same pause is refused (claim is CAS).
    let (status, _) = gateway.post(&format!("{}/{run_id}/resume", runs()), json!({})).await;
    assert_eq!(status, 409);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drained_runs_are_redispatched_with_their_inputs() {
    let gateway = start_gateway(Options::default()).await;
    let (_, record) =
        gateway.post(&runs(), json!({ "inputs": { "mode": "preempt_once", "user_jwt": "eyJ.secret.jwt" } })).await;
    assert_eq!(record["status"], "succeeded", "{record}");
    assert_eq!(record["outputs"]["executions"], 2);
    assert_eq!(record["epoch"], 2);
    let bodies = gateway.provider.world.bodies.lock().unwrap().clone();
    let resumed = bodies.iter().find(|body| body.get("resume").is_some()).unwrap();
    assert_eq!(resumed["resume"]["secrets"]["inputs.user_jwt"], "eyJ.secret.jwt", "in-memory inputs re-supplied");
    assert_eq!(resumed["event_index"], record["event_count"].as_u64().unwrap() - 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lost_stream_is_reclaimed_from_the_orphan_report() {
    let gateway = start_gateway(Options::default()).await;
    let (_, record) = gateway.post(&runs(), json!({ "inputs": { "mode": "break_once" } })).await;
    assert_eq!(record["status"], "succeeded", "{record}");
    assert_eq!(record["outputs"]["executions"], 2);
    assert_eq!(gateway.gateway.metrics.redispatches.load(Ordering::Relaxed), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vanished_executor_fails_with_owner_lost() {
    let gateway = start_gateway(Options { reclaim_grace: Duration::from_millis(300), ..Options::default() }).await;
    let (_, record) = gateway.post(&runs(), json!({ "inputs": { "mode": "vanish" } })).await;
    assert_eq!(record["status"], "failed");
    assert_eq!(record["error_type"], "owner_lost");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_cold_requests_wake_one_replica() {
    let pool = PoolConfig { max_active_runs: 32, ..PoolConfig::default() };
    let gateway = Arc::new(start_gateway(Options { pool, ..Options::default() }).await);
    let requests = (0..20).map(|_| {
        let gateway = gateway.clone();
        async move { gateway.post(&runs(), json!({ "inputs": { "delay_ms": 20 } })).await }
    });
    let results = futures::future::join_all(requests).await;
    assert!(results.iter().all(|(status, record)| *status == 200 && record["status"] == "succeeded"));
    assert_eq!(gateway.provider.wakes.load(Ordering::SeqCst), 1, "singleflight");
    assert_eq!(gateway.provider.live(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_queue_answers_503_with_retry_after() {
    let pool = PoolConfig {
        max_active_runs: 1,
        queue_timeout: Duration::from_millis(100),
        quotas: Quotas { max_replicas: 1, max_queued: 8, ..Quotas::default() },
        ..PoolConfig::default()
    };
    let gateway = Arc::new(start_gateway(Options { pool, ..Options::default() }).await);
    let slow = {
        let gateway = gateway.clone();
        tokio::spawn(async move { gateway.post(&runs(), json!({ "inputs": { "delay_ms": 300 } })).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let response = gateway
        .client()
        .post(format!("{}{}", gateway.url, runs()))
        .header("x-api-key", API_KEY)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 503);
    assert!(response.headers().get("retry-after").is_some());
    assert_eq!(slow.await.unwrap().1["status"], "succeeded");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idempotency_keys_and_webhooks_dedupe() {
    let hook = HookConfig {
        id: "orders".into(),
        scheme: Scheme::Github,
        secret_env: None,
        idempotency_header: Some("x-delivery".into()),
        input: None,
        callback_url: None,
    };
    let options = Options {
        configure_app: Some(Box::new(move |app| {
            app.hooks.insert((DEPLOYMENT.into(), "orders".into()), (hook, Some(b"hook-secret".to_vec())));
        })),
        ..Options::default()
    };
    let gateway = start_gateway(options).await;
    let first = gateway
        .client()
        .post(format!("{}{}", gateway.url, runs()))
        .header("x-api-key", API_KEY)
        .header("idempotency-key", "k-1")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let first: Json = first.json().await.unwrap();
    let second: Json = gateway
        .client()
        .post(format!("{}{}", gateway.url, runs()))
        .header("x-api-key", API_KEY)
        .header("idempotency-key", "k-1")
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first["run_id"], second["run_id"]);
    assert_eq!(second["duplicate"], true);

    let body = br#"{"order":7}"#;
    let mac = graphrt_gateway::hex::encode(&graphrt_gateway::hex::hmac_sha256(b"hook-secret", &[body]));
    let deliver = || {
        gateway
            .client()
            .post(format!("{}/v1/deployments/{DEPLOYMENT}/hooks/orders", gateway.url))
            .header("x-hub-signature-256", format!("sha256={mac}"))
            .header("x-delivery", "evt-1")
            .body(body.to_vec())
            .send()
    };
    let accepted = deliver().await.unwrap();
    assert_eq!(accepted.status().as_u16(), 202);
    let accepted: Json = accepted.json().await.unwrap();
    let again: Json = deliver().await.unwrap().json().await.unwrap();
    assert_eq!(again["duplicate"], true);
    assert_eq!(again["run_id"], accepted["run_id"]);
    let done = gateway.wait_settled(accepted["run_id"].as_str().unwrap()).await;
    assert_eq!(done["status"], "succeeded");
    let forged = gateway
        .client()
        .post(format!("{}/v1/deployments/{DEPLOYMENT}/hooks/orders", gateway.url))
        .header("x-hub-signature-256", "sha256=00")
        .body(body.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status().as_u16(), 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gateway_restart_reclaims_running_runs() {
    let dir = std::env::temp_dir().join(format!("graphrt-gw-restart-{}", std::process::id()));
    let _cleanup = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("gateway.db");
    let world = Arc::new(AgentWorld::default());
    // The first gateway runs on its own runtime; dropping the runtime stops all of
    // its tasks at once, like `kill -9` (the store file survives).
    let (run_id, epoch) = {
        let (path, world) = (path.clone(), world.clone());
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
            let result = runtime.block_on(async move {
                let first =
                    start_gateway(Options { store: SqliteStore::open(&path).unwrap(), world, ..Options::default() })
                        .await;
                let (_, accepted) =
                    first.post(&runs(), json!({ "async": true, "inputs": { "events": 40, "delay_ms": 20 } })).await;
                let run_id = accepted["run_id"].as_str().unwrap().to_owned();
                tokio::time::sleep(Duration::from_millis(200)).await;
                let before = first.gateway.store.get(&run_id).await.unwrap().unwrap();
                assert_eq!(before.status, RunStatus::Running);
                std::mem::forget(first);
                (run_id, before.epoch)
            });
            runtime.shutdown_background();
            result
        })
        .join()
        .unwrap()
    };
    let restarted = tokio::time::Instant::now();
    let second =
        start_gateway(Options { store: SqliteStore::open(&path).unwrap(), world: world.clone(), ..Options::default() })
            .await;
    let (queued, running) = second.gateway.recover().await.unwrap();
    assert_eq!((queued, running), (0, 1));
    assert!(restarted.elapsed() < Duration::from_secs(2), "restart to serving < 2 s");
    // The agent noticed the broken stream and reports the orphan to the new gateway.
    let terminal = json!({ "type": "run_paused", "reasons": [{ "frame": "", "node_id": null, "kind": "external", "data": "gateway_disconnected" }],
        "snapshot": { "format": 1, "run_id": run_id, "graph_fingerprint": "", "elapsed_ms": 0, "steps": 1, "usage": { "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0 }, "handled_errors": 0, "root": { "pool": {}, "nodes": {}, "edges": {}, "outputs": {} } } });
    assert!(second.gateway.accept_orphan(&run_id, json!({ "run_id": run_id, "epoch": epoch, "terminal": terminal })));
    let done = second.wait_settled(&run_id).await;
    assert_eq!(done["status"], "succeeded", "{done}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metrics_and_health() {
    let gateway = start_gateway(Options::default()).await;
    gateway.post(&runs(), json!({})).await;
    let text = gateway.client().get(format!("{}/metrics", gateway.url)).send().await.unwrap().text().await.unwrap();
    assert!(text.contains("graphrt_gateway_runs_started_total 1"), "{text}");
    assert!(text.contains("graphrt_gateway_replicas{deployment=\"shop\",version=\"v1\"} 1"));
    let health: Json =
        gateway.client().get(format!("{}/healthz", gateway.url)).send().await.unwrap().json().await.unwrap();
    assert_eq!(health["status"], "ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streamed_hitl_run_ends_its_stream_at_the_pause() {
    let gateway = start_gateway(Options::default()).await;
    let frames = tokio::time::timeout(
        Duration::from_secs(5),
        gateway.sse(&runs(), Some(json!({ "stream": true, "inputs": { "mode": "hitl", "events": 5 } })), None),
    )
    .await
    .expect("stream ends");
    assert_eq!(frames.last().unwrap().1, "run_paused");
    let run_id =
        frames.iter().find(|(_, event, _)| event == "run_started").unwrap().2["run_id"].as_str().unwrap().to_owned();
    let (status, _) = gateway.post(&format!("{}/{run_id}/resume", runs()), json!({ "inputs": { "ask": 1 } })).await;
    assert_eq!(status, 202);
    let done = gateway.wait_settled(&run_id).await;
    assert_eq!(done["status"], "succeeded");
    let replay = tokio::time::timeout(
        Duration::from_secs(5),
        gateway.sse(&format!("{}/{run_id}/events", runs()), None, Some("2")),
    )
    .await
    .expect("replay ends");
    let ids: Vec<String> = replay.iter().filter_map(|(id, _, _)| id.clone()).filter(|id| !id.contains('.')).collect();
    assert_eq!(ids.first().map(String::as_str), Some("3"), "{ids:?}");
}

/// A Lambda-like provider: no HTTP address for the gateway, requests go
/// through `invoke`; no grace (no preemption) and a request cap.
struct InvokeOnly(Arc<common::FakeAgentProvider>, std::sync::atomic::AtomicU32);

impl graphrt_provider_api::Provider for InvokeOnly {
    fn capabilities(&self) -> graphrt_provider_api::Capabilities {
        graphrt_provider_api::Capabilities { max_grace_secs: 0, max_request_secs: Some(1), ..self.0.capabilities() }
    }
    fn wake<'a>(
        &'a self,
        deployment: &'a graphrt_provider_api::DeploymentRef,
        replicas: u32,
    ) -> graphrt_provider_api::BoxFuture<
        'a,
        Result<Vec<graphrt_provider_api::Endpoint>, graphrt_provider_api::ProviderError>,
    > {
        self.0.wake(deployment, replicas)
    }
    fn release<'a>(
        &'a self,
        replica: &'a graphrt_provider_api::ReplicaRef,
        mode: graphrt_provider_api::ReleaseMode,
    ) -> graphrt_provider_api::BoxFuture<'a, Result<(), graphrt_provider_api::ProviderError>> {
        self.0.release(replica, mode)
    }
    fn list<'a>(
        &'a self,
        deployment: &'a graphrt_provider_api::DeploymentRef,
    ) -> graphrt_provider_api::BoxFuture<
        'a,
        Result<Vec<graphrt_provider_api::ReplicaState>, graphrt_provider_api::ProviderError>,
    > {
        self.0.list(deployment)
    }
    fn invoke<'a>(
        &'a self,
        endpoint: &'a graphrt_provider_api::Endpoint,
        request: graphrt_provider_api::InvokeRequest,
    ) -> Option<
        graphrt_provider_api::BoxFuture<
            'a,
            Result<graphrt_provider_api::InvokeResponse, graphrt_provider_api::ProviderError>,
        >,
    > {
        self.1.fetch_add(1, Ordering::SeqCst);
        let graphrt_provider_api::EndpointAddress::Http(base) = &endpoint.address else { return None };
        let url = format!("{base}{}", request.path);
        Some(Box::pin(async move {
            let mut builder = reqwest::Client::new().post(url).body(request.body);
            for (name, value) in request.headers {
                builder = builder.header(name, value);
            }
            let response = builder
                .send()
                .await
                .map_err(|error| graphrt_provider_api::ProviderError::transient(error.to_string()))?;
            let status = response.status().as_u16();
            let body = futures::stream::unfold(response, |mut response| async move {
                match response.chunk().await {
                    Ok(Some(chunk)) => Some((Ok(chunk.to_vec()), response)),
                    Ok(None) => None,
                    Err(error) => {
                        Some((Err(graphrt_provider_api::ProviderError::transient(error.to_string())), response))
                    }
                }
            });
            Ok(graphrt_provider_api::InvokeResponse { status, body: Box::pin(body) })
        }))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invoke_transport_and_platform_timeout() {
    let base = start_gateway(Options::default()).await;
    let provider = Arc::new(InvokeOnly(base.provider.clone(), std::sync::atomic::AtomicU32::new(0)));
    let auth = graphrt_gateway::auth::DeploymentAuth::new(
        vec![graphrt_gateway::hex::sha256_hex(API_KEY.as_bytes())],
        None,
        &reqwest::Client::new(),
    )
    .unwrap();
    let lambda = Arc::new(graphrt_gateway::deployment::Deployment::new(
        "lambda",
        auth,
        vec![common::key()],
        provider.clone(),
        PoolConfig::default(),
    ));
    lambda.add_version(common::version("v1"), graphrt_core::store::now_ms());
    base.gateway.add_deployment(lambda);
    let (_, record) = base.post("/v1/deployments/lambda/runs", json!({ "inputs": { "text": "via invoke" } })).await;
    assert_eq!(record["outputs"]["echo"], "via invoke", "{record}");
    assert_eq!(provider.1.load(Ordering::SeqCst), 1, "the request went through Provider::invoke");
    let (_, record) =
        base.post("/v1/deployments/lambda/runs", json!({ "inputs": { "events": 5, "delay_ms": 400 } })).await;
    assert_eq!(record["status"], "failed");
    assert_eq!(record["error_type"], "platform_timeout");
}

/// Lambda's own timeout comes back from `invoke` as `PlatformTimeout`.
struct TimesOut(Arc<common::FakeAgentProvider>, std::sync::atomic::AtomicU32);

impl graphrt_provider_api::Provider for TimesOut {
    fn capabilities(&self) -> graphrt_provider_api::Capabilities {
        graphrt_provider_api::Capabilities { max_grace_secs: 0, ..self.0.capabilities() }
    }
    fn wake<'a>(
        &'a self,
        deployment: &'a graphrt_provider_api::DeploymentRef,
        replicas: u32,
    ) -> graphrt_provider_api::BoxFuture<
        'a,
        Result<Vec<graphrt_provider_api::Endpoint>, graphrt_provider_api::ProviderError>,
    > {
        self.0.wake(deployment, replicas)
    }
    fn release<'a>(
        &'a self,
        replica: &'a graphrt_provider_api::ReplicaRef,
        mode: graphrt_provider_api::ReleaseMode,
    ) -> graphrt_provider_api::BoxFuture<'a, Result<(), graphrt_provider_api::ProviderError>> {
        self.0.release(replica, mode)
    }
    fn list<'a>(
        &'a self,
        deployment: &'a graphrt_provider_api::DeploymentRef,
    ) -> graphrt_provider_api::BoxFuture<
        'a,
        Result<Vec<graphrt_provider_api::ReplicaState>, graphrt_provider_api::ProviderError>,
    > {
        self.0.list(deployment)
    }
    fn invoke<'a>(
        &'a self,
        _endpoint: &'a graphrt_provider_api::Endpoint,
        _request: graphrt_provider_api::InvokeRequest,
    ) -> Option<
        graphrt_provider_api::BoxFuture<
            'a,
            Result<graphrt_provider_api::InvokeResponse, graphrt_provider_api::ProviderError>,
        >,
    > {
        self.1.fetch_add(1, Ordering::SeqCst);
        Some(Box::pin(async { Err(graphrt_provider_api::ProviderError::platform_timeout("Sandbox.Timedout")) }))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn platform_timeout_from_invoke_fails_without_redispatch() {
    let base = start_gateway(Options::default()).await;
    let provider = Arc::new(TimesOut(base.provider.clone(), std::sync::atomic::AtomicU32::new(0)));
    let auth = graphrt_gateway::auth::DeploymentAuth::new(
        vec![graphrt_gateway::hex::sha256_hex(API_KEY.as_bytes())],
        None,
        &reqwest::Client::new(),
    )
    .unwrap();
    let lambda = Arc::new(graphrt_gateway::deployment::Deployment::new(
        "lambda",
        auth,
        vec![common::key()],
        provider.clone(),
        PoolConfig::default(),
    ));
    lambda.add_version(common::version("v1"), graphrt_core::store::now_ms());
    base.gateway.add_deployment(lambda);
    let (_, record) = base.post("/v1/deployments/lambda/runs", json!({})).await;
    assert_eq!(record["status"], "failed", "{record}");
    assert_eq!(record["error_type"], "platform_timeout");
    assert_eq!(provider.1.load(Ordering::SeqCst), 1, "not re-dispatched");
}

/// A platform driver that records the spec it got and returns a target name.
struct RecordingPlatform(Arc<std::sync::Mutex<Option<graphrt_provider_api::DeploymentSpec>>>);

impl graphrt_provider_api::DeployDriver for RecordingPlatform {
    fn apply<'a>(
        &'a self,
        spec: &'a graphrt_provider_api::DeploymentSpec,
    ) -> graphrt_provider_api::BoxFuture<
        'a,
        Result<graphrt_provider_api::DeploymentRef, graphrt_provider_api::DeployError>,
    > {
        *self.0.lock().unwrap() = Some(spec.clone());
        Box::pin(async move { Ok(graphrt_provider_api::DeploymentRef::new(&spec.name, &spec.version, "ns/shop-v2")) })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_deploy_applies_on_the_platform_then_registers_the_target() {
    use graphrt_provider_api::DeployDriver as _;
    let options = Options {
        configure_app: Some(Box::new(|app| app.admin_token_hash = Some(graphrt_gateway::hex::sha256_hex(b"admin")))),
        ..Options::default()
    };
    let gateway = start_gateway(options).await;
    let seen = Arc::new(std::sync::Mutex::new(None));
    let driver = graphrt_gateway::deploy::RegisteringDriver::new(
        Box::new(RecordingPlatform(seen.clone())),
        graphrt_gateway::deploy::LocalDeployDriver::new(&gateway.url, "admin"),
    );
    let spec: graphrt_provider_api::DeploymentSpec = serde_json::from_value(json!({
        "name": DEPLOYMENT, "version": "v2", "image": "ghcr.io/acme/shop:2", "command": [], "max_active_runs": 8,
        "min_replicas": 0, "max_replicas": 4, "grace_secs": 30, "workflow_fingerprint": "",
        "provider_settings": { "workflow": "agent", "port": 9000 }
    }))
    .unwrap();
    let deployed = driver.apply(&spec).await.unwrap();
    assert_eq!(deployed.target, "ns/shop-v2");
    let platform_spec = seen.lock().unwrap().clone().unwrap();
    assert!(!platform_spec.provider_settings.contains_key("workflow"), "gateway-only setting stripped");
    assert_eq!(platform_spec.provider_settings["port"], 9000);
    let current = gateway.deployment.current().unwrap();
    assert_eq!(
        (current.info.version.as_str(), current.info.target.as_str(), current.info.workflow.as_str()),
        ("v2", "ns/shop-v2", "agent")
    );
}

/// Records retirements; fails the first call when asked to.
struct RecordingRetirer {
    calls: std::sync::Mutex<Vec<Vec<String>>>,
    fail_first: std::sync::atomic::AtomicBool,
}

impl graphrt_gateway::retire::VersionRetirer for RecordingRetirer {
    fn retire<'a>(
        &'a self,
        versions: &'a [graphrt_provider_api::DeploymentRef],
    ) -> graphrt_provider_api::BoxFuture<'a, Result<String, String>> {
        self.calls.lock().unwrap().push(versions.iter().map(|version| version.version.clone()).collect());
        let fail = self.fail_first.swap(false, Ordering::SeqCst);
        Box::pin(async move { if fail { Err("platform unavailable".to_owned()) } else { Ok("ok".to_owned()) } })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expired_versions_are_retired_on_the_platform_with_retry() {
    let day = 24 * 3600 * 1000;
    let now = graphrt_core::store::now_ms();
    let world = Arc::new(AgentWorld::default());
    let provider = Arc::new(common::FakeAgentProvider::new(world));
    let auth = graphrt_gateway::auth::DeploymentAuth::default();
    let mut deployment = graphrt_gateway::deployment::Deployment::new(
        "shop",
        auth,
        vec![common::key()],
        provider,
        PoolConfig::default(),
    );
    let retirer =
        Arc::new(RecordingRetirer { calls: Default::default(), fail_first: std::sync::atomic::AtomicBool::new(true) });
    deployment.retirer = Some(retirer.clone());
    deployment.add_version(common::version("v1"), now - 9 * day);
    deployment.add_version(common::version("v2"), now - 8 * day);
    // Day 8 after v2: v1 is past its 7-day pool. The first platform call fails.
    graphrt_gateway::app::retire_expired(&deployment, now).await;
    assert!(deployment.version("v1", now).is_err(), "v1 left the gateway (resumes get 410)");
    assert_eq!(deployment.current().unwrap().info.version, "v2");
    // The failed retirement is retried on the next maintenance pass.
    graphrt_gateway::app::retire_expired(&deployment, now).await;
    assert_eq!(*retirer.calls.lock().unwrap(), vec![vec!["v1".to_owned()], vec!["v1".to_owned()]]);
    graphrt_gateway::app::retire_expired(&deployment, now).await;
    assert_eq!(retirer.calls.lock().unwrap().len(), 2, "nothing left to retire");
}

fn warm_path() -> String {
    format!("/v1/deployments/{DEPLOYMENT}/warm")
}

async fn metrics_text(gateway: &common::TestGateway) -> String {
    gateway.client().get(format!("{}/metrics", gateway.url)).send().await.unwrap().text().await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn warm_endpoint_prewarms_and_the_next_run_is_a_hit() {
    let gateway = start_gateway(Options::default()).await;
    let unauthorized = gateway.client().post(format!("{}{}", gateway.url, warm_path())).send().await.unwrap();
    assert_eq!(unauthorized.status().as_u16(), 401, "warm is authenticated like a run");
    let (status, body) = gateway.post(&warm_path(), json!({})).await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["status"], "warming");
    for _ in 0..200 {
        if gateway.provider.live() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(gateway.provider.live(), 1, "a replica is warm before any run");
    let (status, body) = gateway.post(&warm_path(), json!({ "replicas": 1 })).await;
    assert_eq!((status, body["status"].as_str()), (200, Some("warm")), "{body}");
    let (status, record) = gateway.post(&runs(), json!({ "inputs": { "text": "hi" } })).await;
    assert_eq!((status, record["status"].as_str()), (200, Some("succeeded")), "{record}");
    assert_eq!(gateway.provider.wakes.load(Ordering::SeqCst), 1, "the run used the prewarmed replica");
    let text = metrics_text(&gateway).await;
    let label = format!("deployment=\"{DEPLOYMENT}\",version=\"v1\",source=\"api\"");
    assert!(text.contains(&format!("graphrt_gateway_prewarm_started_total{{{label}}} 1")), "{text}");
    assert!(text.contains(&format!("graphrt_gateway_prewarm_hits_total{{{label}}} 1")), "{text}");
    let (status, _) = gateway.post(&warm_path(), json!({ "replicas": "many" })).await;
    assert_eq!(status, 400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spent_warm_budget_refuses_warm_but_never_a_run() {
    let pool =
        PoolConfig { warm_budget: Some(Arc::new(graphrt_gateway::warm::WarmBudget::new(0))), ..PoolConfig::default() };
    let gateway = start_gateway(Options { pool, ..Options::default() }).await;
    let response = gateway
        .client()
        .post(format!("{}{}", gateway.url, warm_path()))
        .header("x-api-key", API_KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 429);
    assert!(response.headers().get("retry-after").is_some());
    let body: Json = response.json().await.unwrap();
    assert_eq!(body["error_type"], "warm_budget_exhausted");
    assert_eq!(gateway.provider.wakes.load(Ordering::SeqCst), 0);
    let (status, record) = gateway.post(&runs(), json!({ "inputs": { "text": "hi" } })).await;
    assert_eq!((status, record["status"].as_str()), (200, Some("succeeded")), "{record}");
    let text = metrics_text(&gateway).await;
    assert!(text.contains("graphrt_gateway_warm_budget_replica_seconds{deployment=\"shop\"} 0"), "{text}");
}
