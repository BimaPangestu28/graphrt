//! Conformance C1–C13 of the Cloud Run provider against fake Google APIs and a
//! fake Cloud Run instance. What only real Cloud Run can show is listed in
//! `docs/gateway-cloud-run.md` ("Real-GCP verification").

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod support;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::StreamExt as _;
use graphrt_gcp::cloud_run::{
    CloudRunDeployDriver, CloudRunProvider, INVOKE_AUTH_HEADER, PLANNED_PREEMPT_MARGIN_SECS, URL_REPLICA_ID,
};
use graphrt_gcp::secret_manager::SecretManager;
use graphrt_provider_api::conformance::{FakeResponse, Fault};
use graphrt_provider_api::http::SecretString;
use graphrt_provider_api::{
    Addressing, DeployDriver, DeployErrorKind, DeploymentRef, EndpointAddress, Provider, ProviderErrorKind,
    ReleaseMode, ReplicaPhase, ReplicaRef, Scaler,
};
use support::{FakeGoogle, FakeInstance, service_resource, spec};

const AUDIENCE: &str = "graphrt/demo-project/europe-west1/graphrt-chat";

struct World {
    google: FakeGoogle,
    instance: FakeInstance,
    provider: CloudRunProvider,
    driver: CloudRunDeployDriver,
}

/// Fakes up, `chat` v1 deployed, provider with the gateway's run-time identity.
async fn world() -> World {
    let instance = FakeInstance::start().await;
    let google = FakeGoogle::start(&instance.url).await;
    *instance.state.expected_audience.lock().unwrap() = Some(AUDIENCE.to_owned());
    let driver = CloudRunDeployDriver::new(google.config(), &google.service_account_auth()).unwrap();
    driver.apply(&spec("chat", "v1")).await.unwrap();
    let provider = CloudRunProvider::new(google.config(), google.metadata_auth()).unwrap();
    World { google, instance, provider, driver }
}

fn reference(version: &str) -> DeploymentRef {
    DeploymentRef::new("chat", version, service_resource("chat"))
}

fn percentile(samples: &mut [Duration], percent: usize) -> Duration {
    samples.sort();
    samples[(samples.len() * percent / 100).min(samples.len() - 1)]
}

#[tokio::test]
async fn c01_twenty_concurrent_wakes_from_zero_share_one_cold_start() {
    let world = world().await;
    world.instance.state.cold_start_ms.store(300, Ordering::SeqCst);
    world.google.clear_requests();
    let deployment = reference("v1");
    let wakes = futures::future::join_all((0..20).map(|_| world.provider.wake(&deployment, 1))).await;
    for endpoints in &wakes {
        let endpoints = endpoints.as_ref().unwrap();
        assert_eq!(endpoints.len(), 1);
        assert_eq!(endpoints[0].replica.id, URL_REPLICA_ID);
        assert_eq!(endpoints[0].address, EndpointAddress::Http(world.instance.url.clone()));
    }
    assert_eq!(world.instance.state.healthz_calls.load(Ordering::SeqCst), 1, "one probe for 20 wakes");
    assert_eq!(world.google.requests_to("GET", "/v2/").len(), 1, "one services.get");
    let identity = world.google.requests_to("GET", "/computeMetadata/v1/instance/service-accounts/default/identity");
    assert_eq!(identity.len(), 1, "one ID token");
    assert!(identity[0].path.contains("audience=graphrt%2Fdemo-project%2Feurope-west1%2Fgraphrt-chat"));
}

#[tokio::test]
async fn c02_wake_overhead_p95_is_far_below_the_two_second_target() {
    let mut world = world().await;
    let mut config = world.google.config();
    config.warm_window = Duration::ZERO;
    world.provider = CloudRunProvider::new(config, world.google.metadata_auth()).unwrap();
    let target = Duration::from_millis(world.provider.capabilities().wake_p95_target_ms);
    let deployment = reference("v1");
    let mut samples = Vec::new();
    for _ in 0..50 {
        let started = Instant::now();
        world.provider.wake(&deployment, 1).await.unwrap();
        samples.push(started.elapsed());
    }
    let p95 = percentile(&mut samples, 95);
    eprintln!("C2 cloud-run (fake, warm instance): wake p95 = {p95:?}, p50 = {:?}", percentile(&mut samples, 50));
    assert!(p95 < target, "p95 {p95:?} ≥ target {target:?}");
    assert_eq!(world.instance.state.healthz_calls.load(Ordering::SeqCst), 50);
}

#[tokio::test]
async fn c03_ten_second_grace_means_preempt_on_drain() {
    let world = world().await;
    let capabilities = world.provider.capabilities();
    // Cloud Run's grace is fixed: the spec's 120 s cannot be honoured, so the
    // gateway must plan for 10 s, i.e. always preempt (decision 1).
    assert_eq!(capabilities.max_grace_secs, 10);
    assert!(!capabilities.background_cpu);
    let body = world.driver.render(&spec("chat", "v2")).await.unwrap();
    let text = body.to_string();
    assert!(!text.contains("120"), "no grace setting exists on Cloud Run: {text}");
}

#[tokio::test]
async fn c04_scale_in_is_the_platform_s_job_and_never_cuts_a_stream() {
    let world = world().await;
    let deployment = reference("v1");
    let endpoints = world.provider.wake(&deployment, 1).await.unwrap();
    let EndpointAddress::Http(url) = &endpoints[0].address else { panic!("url endpoint") };
    let (header, value) = world.provider.invoke_authorization(&deployment, false).await.unwrap().unwrap();
    let response = reqwest::Client::new()
        .post(format!("{url}/internal/v1/runs"))
        .header(header, value.expose())
        .body(r#"{"chunks":20,"interval_ms":10}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    world.google.clear_requests();
    world.provider.release(&endpoints[0].replica, ReleaseMode::ScaleIn).await.unwrap();
    world.provider.release(&endpoints[0].replica, ReleaseMode::Stop).await.unwrap();
    assert!(world.google.all_requests().is_empty(), "release makes no API call");
    let body = response.text().await.unwrap();
    assert_eq!(body.matches("\n\n").count(), 20, "stream completed after release");
}

#[tokio::test]
async fn c05_service_removed_out_of_band_is_detected() {
    let world = world().await;
    let deployment = reference("v1");
    let listed = world.provider.list(&deployment).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].phase, ReplicaPhase::Ready);
    world.google.state.lock().unwrap().services.clear();
    assert!(world.provider.list(&deployment).await.unwrap().is_empty());
    let error = world.provider.service_status(&deployment).await.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::NotFound);
}

#[tokio::test]
async fn c06_long_stream_is_not_buffered() {
    let world = world().await;
    let deployment = reference("v1");
    let endpoints = world.provider.wake(&deployment, 1).await.unwrap();
    let EndpointAddress::Http(url) = &endpoints[0].address else { panic!("url endpoint") };
    let (header, value) = world.provider.invoke_authorization(&deployment, false).await.unwrap().unwrap();
    assert_eq!(header, INVOKE_AUTH_HEADER);
    let response = reqwest::Client::new()
        .post(format!("{url}/internal/v1/runs"))
        .header(header, value.expose())
        .body(r#"{"chunks":200,"interval_ms":10,"size":1000}"#)
        .send()
        .await
        .unwrap();
    let mut stream = response.bytes_stream();
    let mut pending = String::new();
    let mut lags = Vec::new();
    while let Some(chunk) = stream.next().await {
        pending.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
        while let Some(end) = pending.find("\n\n") {
            let event: String = pending.drain(..end + 2).collect();
            let sent_us: u128 = event.split("\"sent_us\":").nth(1).unwrap().split(',').next().unwrap().parse().unwrap();
            let now_us = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros();
            lags.push(Duration::from_micros(now_us.saturating_sub(sent_us) as u64));
        }
    }
    assert_eq!(lags.len(), 200);
    let p95 = percentile(&mut lags, 95);
    eprintln!("C6 cloud-run (fake): per-event delivery lag p95 = {p95:?}");
    assert!(p95 <= Duration::from_millis(50), "events buffered: p95 lag {p95:?}");
}

#[tokio::test]
async fn c07_request_cap_drives_planned_preemption() {
    let world = world().await;
    let capabilities = world.provider.capabilities();
    assert_eq!(capabilities.max_request_secs, Some(3_600));
    assert_eq!(capabilities.max_request_secs.unwrap() - PLANNED_PREEMPT_MARGIN_SECS, 3_540);
    let service = world.google.service(&service_resource("chat")).unwrap();
    assert_eq!(service["template"]["timeout"], "3600s");

    let mut config = world.google.config();
    config.request_timeout_secs = 900;
    let provider = CloudRunProvider::new(config.clone(), world.google.metadata_auth()).unwrap();
    assert_eq!(provider.capabilities().max_request_secs, Some(900));
    let driver = CloudRunDeployDriver::new(config, &world.google.service_account_auth()).unwrap();
    assert_eq!(driver.render(&spec("chat", "v2")).await.unwrap()["template"]["timeout"], "900s");
}

#[tokio::test]
async fn c08_invoker_token_refreshes_once_after_403() {
    let world = world().await;
    // The first ID token is for the wrong audience: Cloud Run answers 403.
    *world.instance.state.expected_audience.lock().unwrap() = Some("rotated-audience".to_owned());
    let deployment = reference("v1");
    let error = world.provider.wake(&deployment, 1).await.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Auth);
    assert!(error.message.contains("roles/run.invoker"), "{error}");
    let identity = world.google.requests_to("GET", "/computeMetadata/v1/instance/service-accounts/default/identity");
    assert_eq!(identity.len(), 2, "exactly one forced refresh");
    assert!(!format!("{error:?}").contains("eyJ"), "no token in the error");
}

#[tokio::test]
async fn c08_admin_api_401_refreshes_the_access_token_once() {
    let world = world().await;
    world.google.inject(Fault {
        path_prefix: "/v2/".into(),
        response: FakeResponse::json(401, &serde_json::json!({"error": {"status": "UNAUTHENTICATED"}})),
        count: 1,
    });
    let before = world.google.requests_to("GET", "/computeMetadata/v1/instance/service-accounts/default/token").len();
    world.provider.service_status(&reference("v1")).await.unwrap();
    let after = world.google.requests_to("GET", "/computeMetadata/v1/instance/service-accounts/default/token").len();
    assert_eq!(after - before, 2, "initial token + one forced refresh");
}

#[tokio::test]
async fn c09_transient_errors_back_off_and_capacity_is_not_retried() {
    let world = world().await;
    world.google.inject(Fault { path_prefix: "/v2/".into(), response: FakeResponse::status(503), count: 2 });
    world.google.clear_requests();
    let started = Instant::now();
    world.provider.service_status(&reference("v1")).await.unwrap();
    assert_eq!(world.google.requests_to("GET", "/v2/").len(), 3);
    // Equal jitter: ≥ 100 ms + 200 ms before the third attempt.
    assert!(started.elapsed() >= Duration::from_millis(300), "{:?}", started.elapsed());

    world.google.inject(Fault {
        path_prefix: "/v2/".into(),
        response: FakeResponse::status(429).with_header("Retry-After", "1"),
        count: 1,
    });
    let started = Instant::now();
    world.provider.service_status(&reference("v1")).await.unwrap();
    assert!(started.elapsed() >= Duration::from_secs(1), "Retry-After honoured");

    world.instance.state.healthz_status.store(429, Ordering::SeqCst);
    let error = world.provider.wake(&reference("v1"), 1).await.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Capacity);
    assert_eq!(world.instance.state.healthz_calls.load(Ordering::SeqCst), 1, "capacity is not retried");
}

#[tokio::test]
async fn c09_admin_api_quota_is_respected() {
    let world = world().await;
    let mut config = world.google.config();
    config.admin_requests_per_second = 4.0;
    config.admin_burst = 1;
    let provider = CloudRunProvider::new(config, world.google.metadata_auth()).unwrap();
    let started = Instant::now();
    for _ in 0..5 {
        provider.service_status(&reference("v1")).await.unwrap();
    }
    assert!(started.elapsed() >= Duration::from_millis(950), "5 calls at 4/s: {:?}", started.elapsed());
}

#[tokio::test]
async fn c10_spec_carries_secret_references_only() {
    let world = world().await;
    let secrets = SecretManager::new(&world.google.config(), &world.google.service_account_auth()).unwrap();
    let value = SecretString::new("sk-live-do-not-leak-1234");
    let references = secrets.store_secrets("chat-", &[("OPENAI_API_KEY".to_owned(), value.clone())]).await.unwrap();
    assert_eq!(references[0].reference, "projects/demo-project/secrets/chat-openai-api-key/versions/1");
    // Storing again adds a version (409 ALREADY_EXISTS on create is success).
    let again = secrets.store_secrets("chat-", &[("OPENAI_API_KEY".to_owned(), value.clone())]).await.unwrap();
    assert_eq!(again[0].reference, "projects/demo-project/secrets/chat-openai-api-key/versions/2");

    let mut with_secret = spec("chat", "v2");
    with_secret.secrets = references;
    world.driver.apply(&with_secret).await.unwrap();
    let service = world.google.service(&service_resource("chat")).unwrap();
    let env = &service["template"]["containers"][0]["env"];
    let secret_env = env.as_array().unwrap().iter().find(|entry| entry["name"] == "OPENAI_API_KEY").unwrap();
    assert_eq!(
        secret_env["valueSource"]["secretKeyRef"]["secret"],
        "projects/demo-project/secrets/chat-openai-api-key"
    );
    assert_eq!(secret_env["valueSource"]["secretKeyRef"]["version"], "1");
    for request in world.google.requests_to("PATCH", "/v2/") {
        assert!(!request.body_text().contains("sk-live"), "secret value sent to the Admin API");
    }

    let mut plaintext = spec("chat", "v3");
    plaintext.secrets[0].reference = "sk-live-do-not-leak-1234".to_owned();
    let error = world.driver.apply(&plaintext).await.unwrap_err();
    assert_eq!(error.kind, DeployErrorKind::Invalid);
    assert!(!error.message.contains("sk-live"), "{error}");
}

#[tokio::test]
async fn c11_old_version_stays_wakeable_until_retired() {
    let world = world().await;
    world.driver.apply(&spec("chat", "v2")).await.unwrap();
    let service = world.google.service(&service_resource("chat")).unwrap();
    let traffic = service["traffic"].as_array().unwrap();
    assert_eq!(traffic[0]["tag"], "v2");
    assert_eq!(traffic[0]["percent"], 100);
    assert_eq!(
        traffic[1],
        serde_json::json!({
            "type": "TRAFFIC_TARGET_ALLOCATION_TYPE_REVISION", "revision": "graphrt-chat-v1", "percent": 0, "tag": "v1"
        })
    );
    // "Day 6": a run paused on v1 resumes on v1's revision.
    world.provider.wake(&reference("v1"), 1).await.unwrap();
    world.provider.wake(&reference("v2"), 1).await.unwrap();
    // "Day 8": the gateway retires v1; its wake is NotFound → 410 version_retired.
    world.driver.retire_versions(&reference("v2"), &["v1".to_owned()]).await.unwrap();
    let mut config = world.google.config();
    config.url_cache_ttl = Duration::ZERO;
    let provider = CloudRunProvider::new(config, world.google.metadata_auth()).unwrap();
    let error = provider.wake(&reference("v1"), 1).await.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::NotFound);
    assert!(error.message.contains("retired"), "{error}");
    provider.wake(&reference("v2"), 1).await.unwrap();
    let error = world.driver.retire_versions(&reference("v2"), &["v2".to_owned()]).await.unwrap_err();
    assert_eq!(error.kind, DeployErrorKind::Invalid, "the serving version cannot be retired");
}

#[tokio::test]
async fn c12_scale_to_zero_settings_and_drift() {
    let world = world().await;
    let resource = service_resource("chat");
    let service = world.google.service(&resource).unwrap();
    let template = &service["template"];
    assert_eq!(template["scaling"]["minInstanceCount"], 0);
    assert_eq!(template["scaling"]["maxInstanceCount"], 4);
    assert_eq!(template["maxInstanceRequestConcurrency"], 8);
    assert_eq!(template["containers"][0]["resources"]["cpuIdle"], true);
    assert_eq!(template["containers"][0]["resources"]["startupCpuBoost"], true);
    let status = world.provider.service_status(&reference("v1")).await.unwrap();
    assert!(status.drift_warnings(0).is_empty());

    // Someone sets min instances by hand: reconciliation reports the cost.
    world.google.edit_service(&resource, |service| service["template"]["scaling"]["minInstanceCount"] = 2.into());
    let warnings = world.provider.service_status(&reference("v1")).await.unwrap().drift_warnings(0);
    assert!(warnings.iter().any(|warning| warning.contains("minInstanceCount is 2")), "{warnings:?}");

    // After a gateway kill -9 there is nothing to clean: a fresh provider
    // (no state) lists the same single pseudo-replica, created by nobody.
    let restarted = CloudRunProvider::new(world.google.config(), world.google.metadata_auth()).unwrap();
    assert_eq!(restarted.list(&reference("v1")).await.unwrap().len(), 1);
    let capabilities = restarted.capabilities();
    assert_eq!((capabilities.addressing, capabilities.scaler), (Addressing::Url, Scaler::Platform));
}

#[tokio::test]
async fn deploy_is_idempotent_and_validates() {
    let world = world().await;
    let patches = world.google.requests_to("PATCH", "/v2/").len();
    let deployed = world.driver.apply(&spec("chat", "v1")).await.unwrap();
    assert_eq!(deployed.target, service_resource("chat"));
    assert_eq!(world.google.requests_to("PATCH", "/v2/").len(), patches, "re-applying v1 is a no-op");
    let create = &world.google.requests_to("PATCH", "/v2/")[0];
    assert!(create.path.contains("allowMissing=true"));
    assert!(create.header("authorization").unwrap().starts_with("Bearer ya29."));

    let mut bad = spec("chat", "v9");
    bad.resources.cpu_millis = 500;
    assert_eq!(world.driver.apply(&bad).await.unwrap_err().kind, DeployErrorKind::Invalid);
    let mut bad = spec("chat", "v9");
    bad.provider_settings.insert("admin_endpoint".into(), "http://169.254.169.254".into());
    assert_eq!(world.driver.apply(&bad).await.unwrap_err().kind, DeployErrorKind::Invalid);
    let mut bad = spec("chat", "v9");
    bad.image = None;
    assert_eq!(world.driver.apply(&bad).await.unwrap_err().kind, DeployErrorKind::Invalid);
}

#[tokio::test]
async fn targets_outside_the_configured_project_are_refused() {
    let world = world().await;
    let foreign = DeploymentRef::new("chat", "v1", "projects/other/locations/us-central1/services/x");
    let error = world.provider.wake(&foreign, 1).await.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Fatal);
    let replica = ReplicaRef { deployment: foreign, id: URL_REPLICA_ID.into() };
    world.provider.release(&replica, ReleaseMode::Stop).await.unwrap();
}
