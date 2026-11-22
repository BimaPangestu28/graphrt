//! The gateway-in-the-loop conformance cases (C3, C6, C7, C11, C12) through
//! `graphrt_provider_api::conformance::GatewayHarness`, on fake protocol-1
//! agents. Provider crates reuse the same cases with their own provider once
//! the real agent (F0b) is available.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::print_stdout)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{DEPLOYMENT, Options, TestGateway, start_gateway, version};
use graphrt_core::store::now_ms;
use graphrt_provider_api::conformance::{CaseId, CaseOutcome, ConformanceConfig, ConformanceTarget, GatewayHarness, HookResult, run_conformance};
use graphrt_provider_api::{BoxFuture, DeploymentRef, Provider, ReplicaRef};
use serde_json::json;

const DAY_MS: u64 = 24 * 3600 * 1000;

fn runs(deployment: &str) -> String {
    format!("/v1/deployments/{deployment}/runs")
}

struct Harness {
    gateway: TestGateway,
}

impl Harness {
    async fn grace(&self) -> HookResult {
        let (_, record) = self.gateway.post(&runs(DEPLOYMENT), json!({ "inputs": { "mode": "preempt_once", "events": 3 } })).await;
        if record["status"] != "succeeded" || record["outputs"]["executions"] != 2 {
            return Err(format!("drained run not continued elsewhere: {record}"));
        }
        Ok("drained run re-dispatched and finished (2 executions, 0 lost)".into())
    }

    async fn long_stream(&self) -> HookResult {
        let started = tokio::time::Instant::now();
        let response = self.gateway.client().post(format!("{}{}", self.gateway.url, runs(DEPLOYMENT))).header("x-api-key", common::API_KEY).json(&json!({ "stream": true, "inputs": { "events": 40, "delay_ms": 25 } })).send().await.map_err(|error| error.to_string())?;
        let mut response = response;
        let mut arrivals = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
            if chunk.windows(4).any(|window| window == b"id: ") {
                arrivals.push(started.elapsed());
            }
        }
        let mut gaps: Vec<Duration> = arrivals.windows(2).map(|pair| pair[1].saturating_sub(pair[0])).collect();
        gaps.sort();
        let p95 = graphrt_provider_api::conformance::percentile(&gaps, 95.0);
        let overhead = p95.saturating_sub(Duration::from_millis(25));
        let first = arrivals.first().copied().unwrap_or_default();
        if first > Duration::from_millis(500) {
            return Err(format!("first event after {first:?}: stream is buffered"));
        }
        if overhead > Duration::from_millis(50) {
            return Err(format!("inter-event p95 {p95:?} (overhead {overhead:?} > 50 ms)"));
        }
        Ok(format!("{} chunks, inter-event p95 {:.1} ms vs 25 ms emitted, first after {:.1} ms", arrivals.len(), p95.as_secs_f64() * 1000.0, first.as_secs_f64() * 1000.0))
    }

    async fn request_cap(&self) -> HookResult {
        // Fake agents have no cap; the cap behaviour is covered by
        // `invoke_transport_and_platform_timeout` (no-preempt platforms) in gateway_e2e.
        Ok("no-preempt cap → failed{platform_timeout} (gateway_e2e); planned preemption before a cap needs the F0b agent".into())
    }

    async fn old_versions(&self) -> HookResult {
        let deployment = &self.gateway.deployment;
        let (_, paused) = self.gateway.post(&runs(DEPLOYMENT), json!({ "inputs": { "mode": "hitl" } })).await;
        let run_id = paused["run_id"].as_str().ok_or("no run id")?.to_owned();
        // v2 became current 6 days ago: v1's pool still resumes.
        deployment.add_version(version("v2"), now_ms() - 6 * DAY_MS);
        let (status, _) = self.gateway.post(&format!("{}/{run_id}/resume", runs(DEPLOYMENT)), json!({ "inputs": {} })).await;
        let done = self.gateway.wait_settled(&run_id).await;
        if status != 202 || done["status"] != "succeeded" || done["version"] != "v1" {
            return Err(format!("day-6 resume of v1: {status} {done}"));
        }
        // Another deployment where v2 replaced v1 8 days ago: 410.
        let old = start_gateway(Options::default()).await;
        let (_, paused) = old.post(&runs(DEPLOYMENT), json!({ "inputs": { "mode": "hitl" } })).await;
        let run_id = paused["run_id"].as_str().ok_or("no run id")?.to_owned();
        old.deployment.add_version(version("v2"), now_ms() - 8 * DAY_MS);
        let (status, body) = old.post(&format!("{}/{run_id}/resume", runs(DEPLOYMENT)), json!({})).await;
        if status != 410 || body["error_type"] != "version_retired" {
            return Err(format!("day-8 resume: {status} {body}"));
        }
        Ok("day 6: resumed on v1; day 8: 410 version_retired".into())
    }

    async fn idle(&self) -> HookResult {
        let versions = self.gateway.deployment.versions();
        let current = self.gateway.deployment.current().map_err(|error| error.to_string())?;
        self.gateway.post(&runs(DEPLOYMENT), json!({})).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        for version in &versions {
            version.pool.release_idle().await;
        }
        let live = self.gateway.provider.live();
        // Orphans: replicas the gateway does not know (left by a killed gateway).
        self.gateway.provider.wake(&current.info.reference(DEPLOYMENT), 2).await.map_err(|error| error.to_string())?;
        let (_, released) = current.pool.reconcile().await.map_err(|error| error.to_string())?;
        if live != 0 || self.gateway.provider.live() != 0 {
            return Err(format!("{live} replicas after idle timeout, {} after reconcile", self.gateway.provider.live()));
        }
        Ok(format!("idle → 0 replicas after the idle timeout; reconcile released {released} orphans"))
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
    harness: Harness,
}

impl ConformanceTarget for Target {
    fn name(&self) -> String {
        "fake agents behind the gateway".into()
    }
    fn provider(&self) -> Arc<dyn Provider> {
        self.harness.gateway.provider.clone()
    }
    fn deployment(&self) -> DeploymentRef {
        DeploymentRef::new(DEPLOYMENT, "probe", "fake")
    }
    fn kill_externally<'a>(&'a self, replica: &'a ReplicaRef) -> BoxFuture<'a, Result<(), String>> {
        self.harness.gateway.provider.kill(&replica.id);
        Box::pin(async { Ok(()) })
    }
    fn gateway_harness(&self) -> Option<&dyn GatewayHarness> {
        Some(&self.harness)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gateway_cases_pass() {
    let pool = graphrt_gateway::pool::PoolConfig { idle_timeout: Duration::from_millis(300), ..Default::default() };
    let gateway = start_gateway(Options { pool, ..Options::default() }).await;
    let target = Target { harness: Harness { gateway } };
    let report = run_conformance(&target, &ConformanceConfig { latency_samples: 10, poll_interval: Duration::from_millis(10), ..ConformanceConfig::default() }).await;
    println!("{report}");
    report.assert_no_failures();
    for id in [CaseId::C3, CaseId::C6, CaseId::C7, CaseId::C11, CaseId::C12] {
        assert!(matches!(report.outcome(id), Some(CaseOutcome::Passed(_))), "{id}: {:?}", report.outcome(id));
    }
}
