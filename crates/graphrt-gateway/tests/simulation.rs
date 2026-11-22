//! Seeded simulation of the gateway with fake agents, and the invariant
//! checker of plan §8 (I1–I7; I8 belongs to the active/passive store of F3).
//!
//! Each seed derives a scenario (run modes, duplicate webhook / cron /
//! idempotent deliveries, client reconnect points) from a SplitMix64 stream;
//! the scenario is reproducible, the interleaving of tokio tasks is not.
//! `GRAPHRT_SIM_SEEDS` sets the number of seeds (default 1000 per PR; the
//! nightly runs 10000), `GRAPHRT_SIM_SEED_START` the first one.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::print_stdout)]

mod common;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use common::{API_KEY, DEPLOYMENT, Options, TestGateway, start_gateway};
use graphrt_core::store::{RunStore, StoredEvent};
use graphrt_gateway::cron::Schedule;
use graphrt_gateway::pool::PoolConfig;
use graphrt_gateway::quota::Quotas;
use graphrt_gateway::scheduler::{CronJob, fire};
use graphrt_gateway::webhook::{HookConfig, Scheme};
use graphrt_provider_api::http::JitterRng;
use serde_json::{Value as Json, json};

const SECRET: &str = "sk-sim-never-stored";
const MODES: [&str; 6] = ["ok", "ok", "hitl", "preempt_once", "break_once", "fail"];

#[derive(Debug, Default)]
struct Violations(Vec<String>);

impl Violations {
    fn check(&mut self, ok: bool, message: impl FnOnce() -> String) {
        if !ok {
            self.0.push(message());
        }
    }
}

fn runs() -> String {
    format!("/v1/deployments/{DEPLOYMENT}/runs")
}

fn engine_ids(frames: &[(Option<String>, String, Json)]) -> Vec<u64> {
    frames.iter().filter_map(|(id, _, _)| id.as_deref()).filter(|id| !id.contains('.')).map(|id| id.parse().unwrap()).collect()
}

/// I2: stored indices 0..event_count, no duplicates (append idempotency), one
/// `run_started` per execution segment.
fn check_events(violations: &mut Violations, run_id: &str, events: &[StoredEvent], event_count: u64) {
    violations.check(events.len() as u64 == event_count, || format!("I2 {run_id}: {} stored events, event_count {event_count}", events.len()));
    violations.check(events.iter().all(|event| !event.get().contains(SECRET)), || format!("I6 {run_id}: secret in stored events"));
}

async fn simulate_seed(gateway: &TestGateway, seed: u64, violations: &mut Violations) {
    let rng = JitterRng::seeded(seed);
    let mode = MODES[(rng.next_u64() % MODES.len() as u64) as usize];
    let stream = rng.next_u64() % 2 == 0;
    let events = rng.next_u64() % 6;
    if std::env::var_os("GRAPHRT_SIM_TRACE").is_some() {
        println!("  mode {mode} stream {stream} events {events}");
    }
    let body = json!({ "stream": stream, "inputs": { "mode": mode, "events": events, "api_token": SECRET } });
    // I5 on the live stream.
    let run_id = if stream {
        let frames = gateway.sse(&runs(), Some(body), None).await;
        let ids = engine_ids(&frames);
        let contiguous = ids.windows(2).all(|pair| pair[1] == pair[0] + 1) && ids.first().is_none_or(|first| *first == 0);
        violations.check(contiguous, || format!("I5 seed {seed}: live ids {ids:?}"));
        frames.iter().find_map(|(_, event, data)| (event == "run_started").then(|| data["run_id"].as_str().map(str::to_owned)).flatten())
    } else {
        let (status, record) = gateway.post(&runs(), body).await;
        violations.check(status == 200, || format!("I1 seed {seed}: status {status} {record}"));
        record["run_id"].as_str().map(str::to_owned)
    };
    let Some(run_id) = run_id else {
        violations.0.push(format!("seed {seed}: no run id"));
        return;
    };
    let trace = std::env::var_os("GRAPHRT_SIM_TRACE").is_some();
    if trace {
        println!("  started {run_id}");
    }
    let mut record = gateway.wait_settled(&run_id).await;
    if trace {
        println!("  settled {}", record["status"]);
    }
    if record["status"] == "paused" {
        let (status, _) = gateway.post(&format!("{}/{run_id}/resume", runs()), json!({ "inputs": { "ask": seed } })).await;
        violations.check(status == 202, || format!("seed {seed}: resume {status}"));
        record = gateway.wait_settled(&run_id).await;
    }
    // I1: settled in a final state.
    let expected = if mode == "fail" { "failed" } else { "succeeded" };
    violations.check(record["status"] == expected, || format!("I1 seed {seed} ({mode}): {record}"));
    let stored = gateway.gateway.store.get(&run_id).await.unwrap().unwrap();
    let events_stored = gateway.gateway.store.events(&run_id, 0).await.unwrap();
    check_events(violations, &run_id, &events_stored, stored.event_count);
    if trace {
        println!("  final {}", record["status"]);
    }
    // I5 across a reconnect at a seeded position.
    let cut = rng.next_u64() % stored.event_count.max(1);
    let replay = gateway.sse(&format!("{}/{run_id}/events", runs()), None, Some(&cut.to_string())).await;
    let replayed = engine_ids(&replay);
    violations.check(replayed == ((cut + 1)..stored.event_count).collect::<Vec<_>>(), || format!("I5 seed {seed}: after {cut} got {replayed:?} of {}", stored.event_count));
}

async fn simulate_duplicates(gateway: &Arc<TestGateway>, seed: u64, violations: &mut Violations) {
    // I4: concurrent duplicate deliveries of one webhook → one run.
    let body = format!("{{\"seed\":{seed}}}");
    let mac = graphrt_gateway::hex::encode(&graphrt_gateway::hex::hmac_sha256(b"sim-hook", &[body.as_bytes()]));
    let deliveries = (0..3).map(|_| {
        let gateway = gateway.clone();
        let (body, mac) = (body.clone(), mac.clone());
        async move {
            let response = gateway
                .client()
                .post(format!("{}/v1/deployments/{DEPLOYMENT}/hooks/sim", gateway.url))
                .header("x-hub-signature-256", format!("sha256={mac}"))
                .header("x-delivery", format!("d-{seed}"))
                .body(body)
                .send()
                .await
                .unwrap();
            let json: Json = response.json().await.unwrap();
            json["run_id"].as_str().unwrap().to_owned()
        }
    });
    if std::env::var_os("GRAPHRT_SIM_TRACE").is_some() {
        println!("  duplicates");
    }
    let ids: HashSet<String> = futures::future::join_all(deliveries).await.into_iter().collect();
    violations.check(ids.len() == 1, || format!("I4 seed {seed}: {} runs for one delivery", ids.len()));
    // I3: the same fire twice → one run.
    let job = CronJob { schedule: Schedule::new("sim", "* * * * *", "UTC").unwrap(), body: serde_json::Map::new() };
    let at = 1_800_000_000 + (seed as i64) * 60;
    let (first, second) = tokio::join!(fire(&gateway.gateway, &gateway.deployment, &job, at), fire(&gateway.gateway, &gateway.deployment, &job, at));
    violations.check(u8::from(first) + u8::from(second) == 1, || format!("I3 seed {seed}: {} runs for one fire", u8::from(first) + u8::from(second)));
    for run_id in ids.iter().cloned().chain([graphrt_gateway::cron::fire_run_id(DEPLOYMENT, "sim", at)]) {
        let record = gateway.wait_settled(&run_id).await;
        violations.check(record["status"] == "succeeded", || format!("I1 seed {seed}: {run_id} {record}"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn invariants_hold_over_seeds() {
    let seeds: u64 = std::env::var("GRAPHRT_SIM_SEEDS").ok().and_then(|value| value.parse().ok()).unwrap_or(1000);
    let start: u64 = std::env::var("GRAPHRT_SIM_SEED_START").ok().and_then(|value| value.parse().ok()).unwrap_or(0);
    let hook = HookConfig { id: "sim".into(), scheme: Scheme::Github, secret_env: None, idempotency_header: Some("x-delivery".into()), input: None, callback_url: None };
    let options = Options {
        // The wake quota is unit-tested; thousands of seeds in seconds would exhaust it.
        pool: PoolConfig { max_active_runs: 16, idle_timeout: Duration::from_millis(300), quotas: Quotas { max_wakes_per_hour: u32::MAX, ..Quotas::default() }, ..PoolConfig::default() },
        configure_app: Some(Box::new(move |app| {
            app.hooks.insert((DEPLOYMENT.into(), "sim".into()), (hook, Some(b"sim-hook".to_vec())));
        })),
        ..Options::default()
    };
    let gateway = Arc::new(start_gateway(options).await);
    let lanes: u64 = std::env::var("GRAPHRT_SIM_LANES").ok().and_then(|value| value.parse().ok()).unwrap_or(16);
    let tasks = (0..lanes).map(|lane| {
        let gateway = gateway.clone();
        tokio::spawn(async move {
            let mut violations = Violations::default();
            let mut seed = start + lane;
            while seed < start + seeds {
                if std::env::var_os("GRAPHRT_SIM_TRACE").is_some() {
                    println!("seed {seed}");
                }
                simulate_seed(&gateway, seed, &mut violations).await;
                if seed % 5 == 0 {
                    simulate_duplicates(&gateway, seed, &mut violations).await;
                }
                seed += lanes;
            }
            violations.0
        })
    });
    let mut violations: Vec<String> = futures::future::join_all(tasks).await.into_iter().flat_map(Result::unwrap).collect();

    // I6: no secret in records or metrics.
    let page = gateway.gateway.store.list(Default::default()).await.unwrap();
    let records = serde_json::to_string(&page.runs).unwrap();
    if records.contains(SECRET) {
        violations.push("I6: secret in a run record".into());
    }
    let metrics = gateway.client().get(format!("{}/metrics", gateway.url)).header("x-api-key", API_KEY).send().await.unwrap().text().await.unwrap();
    if metrics.contains(SECRET) {
        violations.push("I6: secret in metrics".into());
    }
    // I7: an idle deployment writes nothing and releases its replicas.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let writes = gateway.gateway.store.write_count();
    let version = gateway.deployment.current().unwrap();
    version.pool.release_idle().await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    version.pool.release_idle().await;
    if gateway.gateway.store.write_count() != writes {
        violations.push(format!("I7: {} store writes while idle", gateway.gateway.store.write_count() - writes));
    }
    if version.pool.stats().replicas != 0 || gateway.provider.live() != 0 {
        violations.push(format!("I7: {} replicas after idle timeout", gateway.provider.live()));
    }
    println!("simulation: {seeds} seeds from {start}, {} violations", violations.len());
    assert!(violations.is_empty(), "{} invariant violations:\n{}", violations.len(), violations.iter().take(30).cloned().collect::<Vec<_>>().join("\n"));
}
