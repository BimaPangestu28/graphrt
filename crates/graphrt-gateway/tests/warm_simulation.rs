//! Deterministic simulation of the F4 warm-up features on virtual time
//! (`start_paused`): wake-storm protection (`max_parallel_wakes` and the
//! prewarm jitter) and the predictive prewarm's hit rate against the plan's
//! target (≥ 70 %, plan §10 F4).
//!
//! Every input comes from a seeded SplitMix64 stream and tokio's paused clock,
//! so a seed replays exactly. `GRAPHRT_SIM_SEEDS` sets the number of seeds
//! (default 50), `GRAPHRT_SIM_SEED_START` the first one.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::print_stdout)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use graphrt_gateway::pool::{Pool, PoolConfig, PrewarmOutcome};
use graphrt_gateway::quota::Quotas;
use graphrt_gateway::warm::{ArrivalModel, PrewarmSource, WakeGate, WakeStormConfig};
use graphrt_provider_api::http::JitterRng;
use graphrt_provider_api::{
    Addressing, BoxFuture, Capabilities, DeploymentRef, Endpoint, EndpointAddress, Provider, ProviderError,
    ReleaseMode, ReplicaPhase, ReplicaRef, ReplicaState, Scaler,
};
use tokio::time::Instant;

/// A provider whose wakes take `latency` each and run fully in parallel, and
/// that records how many are in flight (the cloud API a storm would hit).
struct StormProvider {
    latency: Duration,
    replicas: Mutex<HashMap<String, Vec<ReplicaRef>>>,
    next: AtomicU64,
    in_flight: AtomicU32,
    peak: AtomicU32,
    starts: Mutex<Vec<Instant>>,
}

impl StormProvider {
    fn new(latency: Duration) -> Arc<Self> {
        Arc::new(Self {
            latency,
            replicas: Mutex::default(),
            next: AtomicU64::new(0),
            in_flight: AtomicU32::new(0),
            peak: AtomicU32::new(0),
            starts: Mutex::default(),
        })
    }

    fn endpoints(&self, deployment: &DeploymentRef) -> Vec<Endpoint> {
        self.replicas
            .lock()
            .unwrap()
            .get(&deployment.name)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|replica| {
                let address = EndpointAddress::Http(format!("mem://{}", replica.id));
                Endpoint { replica, address }
            })
            .collect()
    }
}

impl Provider for StormProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            addressing: Addressing::Replica,
            scaler: Scaler::Gateway,
            max_request_secs: None,
            idle_timeout_secs: None,
            max_grace_secs: 30,
            background_cpu: true,
            supports_suspend: false,
            wake_p95_target_ms: self.latency.as_millis() as u64,
        }
    }

    fn wake<'a>(
        &'a self,
        deployment: &'a DeploymentRef,
        replicas: u32,
    ) -> BoxFuture<'a, Result<Vec<Endpoint>, ProviderError>> {
        Box::pin(async move {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            self.starts.lock().unwrap().push(Instant::now());
            tokio::time::sleep(self.latency).await;
            {
                let mut all = self.replicas.lock().unwrap();
                let list = all.entry(deployment.name.clone()).or_default();
                while list.len() < replicas as usize {
                    let id = format!("r-{}", self.next.fetch_add(1, Ordering::SeqCst));
                    list.push(ReplicaRef { deployment: deployment.clone(), id });
                }
            }
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(self.endpoints(deployment))
        })
    }

    fn release<'a>(&'a self, replica: &'a ReplicaRef, _mode: ReleaseMode) -> BoxFuture<'a, Result<(), ProviderError>> {
        Box::pin(async move {
            if let Some(list) = self.replicas.lock().unwrap().get_mut(&replica.deployment.name) {
                list.retain(|known| known != replica);
            }
            Ok(())
        })
    }

    fn list<'a>(&'a self, deployment: &'a DeploymentRef) -> BoxFuture<'a, Result<Vec<ReplicaState>, ProviderError>> {
        Box::pin(async move {
            Ok(self
                .endpoints(deployment)
                .into_iter()
                .map(|endpoint| ReplicaState {
                    replica: endpoint.replica,
                    phase: ReplicaPhase::Ready,
                    address: Some(endpoint.address),
                    started_at_ms: None,
                })
                .collect())
        })
    }
}

fn seeds() -> std::ops::Range<u64> {
    let count = std::env::var("GRAPHRT_SIM_SEEDS").ok().and_then(|text| text.parse().ok()).unwrap_or(50);
    let start = std::env::var("GRAPHRT_SIM_SEED_START").ok().and_then(|text| text.parse().ok()).unwrap_or(0);
    start..start + count
}

fn pools(provider: &Arc<StormProvider>, count: usize, gate: Option<Arc<WakeGate>>) -> Vec<Arc<Pool>> {
    (0..count)
        .map(|index| {
            let config = PoolConfig {
                queue_timeout: Duration::from_secs(120),
                quotas: Quotas { max_replicas: 1, ..Quotas::default() },
                wake_gate: gate.clone(),
                ..PoolConfig::default()
            };
            Pool::new(DeploymentRef::new(format!("d{index}"), "v1", "storm"), provider.clone(), config)
        })
        .collect()
}

const DEPLOYMENTS: usize = 48;
const MAX_PARALLEL: u32 = 4;
const WAKE: Duration = Duration::from_secs(2);

/// A cold burst on every deployment at once (a gateway restart, a launch):
/// at most `max_parallel_wakes` provider wakes in flight, every request
/// served, in FIFO waves of `WAKE`.
#[tokio::test(start_paused = true)]
async fn storm_of_cold_requests_is_bounded_by_max_parallel_wakes() {
    for seed in seeds() {
        let rng = JitterRng::seeded(seed);
        let provider = StormProvider::new(WAKE);
        let gate = Arc::new(WakeGate::seeded(
            WakeStormConfig { max_parallel_wakes: MAX_PARALLEL, prewarm_jitter_ms: 0 },
            JitterRng::seeded(seed),
        ));
        let pools = pools(&provider, DEPLOYMENTS, Some(gate.clone()));
        let started = Instant::now();
        // 1–3 concurrent requests per deployment, in a seeded order.
        let mut requests = Vec::new();
        for pool in &pools {
            for _ in 0..=(rng.next_u64() % 3) {
                requests.push(pool.clone());
            }
        }
        let leases = futures::future::join_all(requests.iter().map(|pool| pool.acquire())).await;
        assert!(leases.iter().all(Result::is_ok), "seed {seed}: every request is served");
        assert!(provider.peak.load(Ordering::SeqCst) <= MAX_PARALLEL, "seed {seed}: peak wakes");
        assert_eq!(gate.peak.load(Ordering::SeqCst), MAX_PARALLEL, "seed {seed}: the gate was saturated");
        assert_eq!(provider.starts.lock().unwrap().len(), DEPLOYMENTS, "seed {seed}: one wake per deployment");
        let waves = DEPLOYMENTS as u32 / MAX_PARALLEL;
        assert_eq!(started.elapsed(), WAKE * waves, "seed {seed}: FIFO waves, no idle gap");
    }
    // Control: without the gate every wake hits the provider at once.
    let provider = StormProvider::new(WAKE);
    let pools = pools(&provider, DEPLOYMENTS, None);
    let _leases = futures::future::join_all(pools.iter().map(|pool| pool.acquire())).await;
    assert_eq!(provider.peak.load(Ordering::SeqCst), DEPLOYMENTS as u32);
}

/// Every deployment prewarms for the same minute (crons at midnight): the
/// jitter spreads the wakes over the window and the gate still bounds them.
#[tokio::test(start_paused = true)]
async fn simultaneous_prewarms_are_spread_by_the_jitter() {
    let jitter = Duration::from_secs(10);
    for seed in seeds() {
        let provider = StormProvider::new(WAKE);
        let gate = Arc::new(WakeGate::seeded(
            WakeStormConfig { max_parallel_wakes: MAX_PARALLEL * 4, prewarm_jitter_ms: jitter.as_millis() as u64 },
            JitterRng::seeded(seed),
        ));
        let pools = pools(&provider, DEPLOYMENTS, Some(gate.clone()));
        let started = Instant::now();
        let tasks: Vec<_> = pools
            .iter()
            .map(|pool| {
                let (pool, delay) = (pool.clone(), gate.jitter(Duration::MAX));
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    pool.prewarm(1, PrewarmSource::Cron).await.unwrap()
                })
            })
            .collect();
        for task in tasks {
            assert_eq!(task.await.unwrap(), PrewarmOutcome::Started(1), "seed {seed}");
        }
        let starts = provider.starts.lock().unwrap().clone();
        let offsets: Vec<Duration> = starts.iter().map(|at| at.duration_since(started)).collect();
        let latest = offsets.iter().max().copied().unwrap();
        assert!(latest >= jitter / 2, "seed {seed}: spread over the window ({latest:?})");
        // In any one-second window at most a third of the deployments start.
        for window in 0..jitter.as_secs() {
            let from = Duration::from_secs(window);
            let in_window = offsets.iter().filter(|at| **at >= from && **at < from + Duration::from_secs(1)).count();
            assert!(in_window <= DEPLOYMENTS / 3, "seed {seed}: {in_window} wakes in second {window}");
        }
        assert!(provider.peak.load(Ordering::SeqCst) <= MAX_PARALLEL * 4, "seed {seed}");
        assert!(pools.iter().all(|pool| pool.stats().replicas == 1), "seed {seed}: all warm");
    }
}

/// Traffic of one simulated deployment: bursts at fixed times of weekdays
/// (± 3 minutes, each skipped on 15 % of the days), quiet weekends, and random
/// noise. `(unix secs, part of a scheduled burst)`.
fn traffic(rng: &JitterRng, days: i64) -> Vec<(i64, bool)> {
    const BURSTS: [i64; 3] = [9 * 3600, 12 * 3600 + 30 * 60, 18 * 3600];
    let mut arrivals = Vec::new();
    for day in 0..days {
        // Unix day 0 was a Thursday.
        let weekend = (day + 3) % 7 >= 5;
        if !weekend {
            for burst in BURSTS {
                if rng.next_unit() < 0.15 {
                    continue;
                }
                let start = day * 86_400 + burst + (rng.next_u64() % 360) as i64 - 180;
                for _ in 0..1 + rng.next_u64() % 4 {
                    arrivals.push((start + (rng.next_u64() % 120) as i64, true));
                }
            }
        }
        for _ in 0..rng.next_u64() % 3 {
            arrivals.push((day * 86_400 + (rng.next_u64() % 86_400) as i64, false));
        }
    }
    arrivals.sort_unstable();
    arrivals
}

#[derive(Debug, Default)]
struct Outcome {
    hits: u64,
    started: u64,
    burst_cold: u64,
    bursts: u64,
}

/// One deployment over `days` days: the real pool (idle release every 15 s,
/// `idle_timeout` 300 s, wakes of 2 s) and the real arrival model.
async fn simulate_days(seed: u64, days: i64, predictive: bool) -> Outcome {
    let rng = JitterRng::seeded(seed);
    let arrivals = traffic(&rng, days);
    let provider = StormProvider::new(WAKE);
    let pool = pools(&provider, 1, None).remove(0);
    let mut model = ArrivalModel::new(0, 0.3, 0.6);
    let origin = Instant::now();
    let mut next = 0;
    let mut last_arrival = i64::MIN;
    let mut outcome = Outcome::default();
    // Counted from day 7: the first week is learning (three weekdays of each slot).
    let measured_from = 7 * 86_400;
    let mut baseline = None;
    loop {
        let now = origin.elapsed().as_secs() as i64;
        if now >= days * 86_400 {
            break;
        }
        if now >= measured_from && baseline.is_none() {
            baseline = Some(pool.stats().prewarm);
        }
        while next < arrivals.len() && arrivals[next].0 <= now {
            let (at, scheduled) = arrivals[next];
            next += 1;
            model.record_arrival(at);
            // First request of a scheduled burst (no traffic for 30 min): did it find a warm replica?
            if scheduled && at >= measured_from && at - last_arrival > 1800 {
                outcome.bursts += 1;
                if pool.stats().replicas == 0 {
                    outcome.burst_cold += 1;
                }
            }
            last_arrival = at;
            let pool = pool.clone();
            tokio::spawn(async move {
                let lease = pool.acquire().await.unwrap();
                tokio::time::sleep(Duration::from_secs(20)).await;
                drop(lease);
            });
        }
        if predictive && model.should_prewarm(now, 60) {
            let pool = pool.clone();
            tokio::spawn(
                async move { pool.prewarm_holding(1, PrewarmSource::Predictive, ArrivalModel::hold(60)).await },
            );
        }
        pool.release_idle().await;
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
    let (stats, baseline) = (pool.stats().prewarm, baseline.unwrap_or_default());
    outcome.hits = stats.hits[0] - baseline.hits[0];
    outcome.started = stats.started[0] - baseline.started[0];
    outcome
}

/// The plan's F4 target: hit rate ≥ 70 % on a periodic workload with noise and
/// quiet weekends, and fewer cold first requests of a burst than without it.
#[tokio::test(start_paused = true)]
async fn predictive_prewarm_hit_rate_meets_the_plan_target() {
    let days = 21;
    let (mut hits, mut started, mut cold_with, mut cold_without, mut bursts) = (0, 0, 0, 0, 0);
    for seed in seeds().take(4) {
        let with = simulate_days(seed, days, true).await;
        let without = simulate_days(seed, days, false).await;
        hits += with.hits;
        started += with.started;
        cold_with += with.burst_cold;
        cold_without += without.burst_cold;
        bursts += with.bursts;
        assert_eq!(without.started, 0);
    }
    let hit_rate = hits as f64 / started.max(1) as f64;
    println!(
        "predictive prewarm: hit rate {:.1} % ({hits}/{started}); cold first requests of scheduled bursts {cold_with}/{bursts} with, {cold_without}/{bursts} without",
        hit_rate * 100.0
    );
    assert!(hit_rate >= 0.7, "hit rate {hit_rate:.3} below the 70 % target");
    assert!(cold_with * 2 < cold_without, "prewarm at least halves cold bursts ({cold_with} vs {cold_without})");
}
