//! Conformance suite (plan §7) for `provider-ecs` against the fake ECS API.
//!
//! Items that need the gateway or real AWS are listed in `docs/gateway-aws.md`.
#![cfg(all(feature = "provider-ecs", feature = "provider-lambda"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::print_stdout)]

mod support;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use graphrt_provider_aws::api::{DeploymentRef, Provider, ProviderErrorKind, ReleaseMode, ReplicaPhase};
use graphrt_provider_aws::ecs::{EcsConfig, EcsProvider, EcsTaskTemplate};
use support::{FakeAws, fast_retry, quantile};

fn template() -> EcsTaskTemplate {
    EcsTaskTemplate {
        cluster: "graphrt".into(),
        task_definition_family: "graphrt-agent".into(),
        subnets: vec!["subnet-1".into()],
        security_groups: vec!["sg-1".into()],
        assign_public_ip: false,
        container_port: 8080,
        capacity_provider: None,
        platform_version: None,
    }
}

fn provider(fake: &FakeAws) -> EcsProvider {
    let config = EcsConfig {
        endpoint: fake.endpoint("ecs"),
        limits: fast_retry(),
        deployments: HashMap::from([("chat".to_owned(), template())]),
        wake_timeout: Duration::from_secs(5),
        poll_interval: Duration::from_millis(10),
    };
    EcsProvider::new(fake.chain(), config).unwrap()
}

fn deployment(version: &str) -> DeploymentRef {
    DeploymentRef::new("chat", version, "")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn c1_twenty_concurrent_wakes_start_exactly_the_requested_replicas() {
    let fake = support::start().await;
    fake.state.lock().unwrap().provisioning = Duration::from_millis(50);
    let provider = Arc::new(provider(&fake));
    let d = deployment("7");
    let wakes = (0..20).map(|_| {
        let provider = Arc::clone(&provider);
        let d = d.clone();
        tokio::spawn(async move { provider.wake(&d, 2).await })
    });
    let results = futures::future::join_all(wakes).await;
    let mut urls = HashSet::new();
    for result in results {
        let endpoints = result.unwrap().unwrap();
        assert_eq!(endpoints.len(), 2);
        urls.extend(endpoints.iter().map(|e| support::url_of(&e.address)));
    }
    assert_eq!(urls.len(), 2, "all wakes share the same two tasks: {urls:?}");
    assert_eq!(fake.live_tasks(), 2);
    assert!(urls.iter().all(|u| u.starts_with("http://10.0.") && u.ends_with(":8080")));
    assert_eq!(fake.state.lock().unwrap().signature_failures, 0);
}

#[tokio::test]
async fn c2_wake_latency_n50() {
    let fake = support::start().await;
    let provisioning = Duration::from_millis(30);
    fake.state.lock().unwrap().provisioning = provisioning;
    let provider = provider(&fake);
    let d = deployment("7");
    let mut samples = Vec::new();
    for _ in 0..50 {
        let start = Instant::now();
        let endpoints = provider.wake(&d, 1).await.unwrap();
        samples.push(start.elapsed());
        provider.release(&endpoints[0].replica, ReleaseMode::ScaleIn).await.unwrap();
    }
    let p50 = quantile(samples.clone(), 0.5);
    let p95 = quantile(samples, 0.95);
    println!("C2 ecs fake: provisioning {provisioning:?}, wake p50 {p50:?}, p95 {p95:?}");
    // Provider overhead on top of the platform's provisioning time (poll interval + 4–5 API
    // round trips ≈ 15 ms alone; generous because the other suites run in parallel).
    // The real §5 target (p95 ≤ 45 s) needs real Fargate.
    assert!(p95 < provisioning + Duration::from_millis(250), "p95 {p95:?}");
}

#[tokio::test]
async fn c3_stop_uses_stop_task_with_reason_and_stop_timeout_is_capped() {
    let fake = support::start().await;
    let provider = provider(&fake);
    let endpoints = provider.wake(&deployment("7"), 1).await.unwrap();
    provider.release(&endpoints[0].replica, ReleaseMode::Stop).await.unwrap();
    let reasons = fake.state.lock().unwrap().stop_reasons.clone();
    assert_eq!(reasons, vec![(endpoints[0].replica.id.clone(), "graphrt gateway: stop".to_owned())]);
    assert_eq!(provider.capabilities().max_grace_secs, 120);
    assert!(graphrt_provider_aws::ecs::validate_stop_timeout(121, 60).is_err());
}

#[tokio::test]
async fn c4_scale_in_releases_only_the_chosen_replica() {
    let fake = support::start().await;
    let provider = provider(&fake);
    let d = deployment("7");
    let endpoints = provider.wake(&d, 3).await.unwrap();
    assert_eq!(endpoints.len(), 3);
    provider.release(&endpoints[1].replica, ReleaseMode::ScaleIn).await.unwrap();
    let live: Vec<String> = provider.list(&d).await.unwrap().into_iter().map(|s| s.replica.id).collect();
    assert_eq!(live.len(), 2);
    assert!(!live.contains(&endpoints[1].replica.id));
    // Releasing again (or an unknown task) is idempotent.
    provider.release(&endpoints[1].replica, ReleaseMode::ScaleIn).await.unwrap();
}

#[tokio::test]
async fn c5_task_killed_outside_the_gateway_is_detected_and_replaced() {
    let fake = support::start().await;
    let provider = provider(&fake);
    let d = deployment("7");
    let endpoints = provider.wake(&d, 2).await.unwrap();
    {
        let mut state = fake.state.lock().unwrap();
        let task = state.tasks.iter_mut().find(|t| t.arn == endpoints[0].replica.id).unwrap();
        task.stopped = true;
        task.stopped_reason = Some("Essential container in task exited".into());
    }
    let detected = Instant::now();
    let live = provider.list(&d).await.unwrap();
    assert_eq!(live.len(), 1, "a single reconcile pass sees the dead task");
    assert!(detected.elapsed() < Duration::from_secs(1));
    let replaced = provider.wake(&d, 2).await.unwrap();
    assert_eq!(replaced.len(), 2);
    assert!(!replaced.iter().any(|e| e.replica.id == endpoints[0].replica.id));
}

#[tokio::test]
async fn c8_expired_token_refreshes_once_and_clock_skew_is_corrected() {
    let fake = support::start().await;
    let provider = provider(&fake);
    fake.state.lock().unwrap().expired_token_next = 1;
    provider.list(&deployment("7")).await.unwrap();
    assert_eq!(fake.state.lock().unwrap().cred_hits, 2, "credentials refetched exactly once after 403");

    // The fake's clock runs 7 minutes ahead: the first request is rejected, the
    // client learns the offset from `Date` and the retry succeeds.
    fake.state.lock().unwrap().server_clock_offset = 420;
    provider.list(&deployment("7")).await.unwrap();
    fake.state.lock().unwrap().server_clock_offset = -330;
    provider.list(&deployment("7")).await.unwrap();

    // Two expired-token answers in a row: one refresh only, then a clean Auth error.
    fake.state.lock().unwrap().expired_token_next = 2;
    let error = provider.list(&deployment("7")).await.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Auth, "{error:?}");
    assert!(!format!("{error} {provider:?}").contains(support::SECRET_KEY));
    assert!(!format!("{error} {provider:?}").contains(support::SESSION_TOKEN));
}

#[tokio::test]
async fn c9_throttling_and_5xx_back_off_then_succeed_or_surface() {
    let fake = support::start().await;
    let provider = provider(&fake);
    let d = deployment("7");
    fake.state.lock().unwrap().throttle_next = 2;
    fake.state.lock().unwrap().server_error_next = 1;
    provider.wake(&d, 1).await.unwrap();

    fake.state.lock().unwrap().throttle_next = 10;
    let error = provider.list(&d).await.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::RateLimited, "{error:?}");
    assert!(error.retry_after.is_some_and(|d| !d.is_zero()));
    fake.state.lock().unwrap().throttle_next = 0;

    fake.state.lock().unwrap().capacity_failure = true;
    let error = provider.wake(&d, 3).await.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Capacity, "{error:?}");
}

#[tokio::test]
async fn c9_run_task_retried_after_5xx_does_not_double_tasks() {
    let fake = support::start().await;
    let provider = provider(&fake);
    let d = deployment("7");
    // RunTask starts the tasks but the response is lost (500); the retry carries the
    // same client token and gets the same tasks back instead of starting two more.
    fake.state.lock().unwrap().lose_next_run_task_response = true;
    provider.wake(&d, 2).await.unwrap();
    assert_eq!(fake.calls("RunTask"), 2);
    assert_eq!(fake.state.lock().unwrap().client_tokens.len(), 1, "one RunTask client token per wake");
    assert_eq!(fake.live_tasks(), 2);
}

#[tokio::test]
async fn c11_old_version_wakes_its_own_task_definition_revision() {
    let fake = support::start().await;
    let provider = provider(&fake);
    provider.wake(&deployment("6"), 1).await.unwrap();
    provider.wake(&deployment("7"), 1).await.unwrap();
    let definitions: HashSet<String> =
        fake.state.lock().unwrap().tasks.iter().map(|t| t.task_definition.clone()).collect();
    assert_eq!(definitions, HashSet::from(["graphrt-agent:6".to_owned(), "graphrt-agent:7".to_owned()]));
    assert_eq!(provider.list(&deployment("6")).await.unwrap().len(), 1);
}

#[tokio::test]
async fn c12_reconcile_finds_and_stops_orphans_after_gateway_kill() {
    let fake = support::start().await;
    let d = deployment("7");
    {
        // A gateway that starts tasks and is then killed (-9): nothing is released.
        let crashed = provider(&fake);
        crashed.wake(&d, 3).await.unwrap();
    }
    let restarted = provider(&fake);
    let started = Instant::now();
    let orphans = restarted.list(&d).await.unwrap();
    assert_eq!(orphans.len(), 3);
    assert!(orphans.iter().all(|o| o.phase == ReplicaPhase::Ready));
    for orphan in &orphans {
        restarted.release(&orphan.replica, ReleaseMode::Stop).await.unwrap();
    }
    assert_eq!(fake.live_tasks(), 0);
    assert!(started.elapsed() < Duration::from_secs(2));
}
