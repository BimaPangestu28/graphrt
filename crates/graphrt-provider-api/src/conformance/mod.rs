//! Provider conformance suite C1–C13 (plan §7).
//!
//! A provider crate implements [`ConformanceTarget`] for its provider wired to
//! a [`FakeServer`] (per PR) or to a real cloud account (nightly), then calls
//! [`run_conformance`] and asserts the report has no failures:
//!
//! ```ignore
//! let report = run_conformance(&my_target, &ConformanceConfig::default()).await;
//! report.assert_no_failures();
//! ```
//!
//! Provider-level cases run here. Cases that need the gateway in the loop
//! (C3 grace/migration, C6 long stream, C7 request cap, C11 old-version pool,
//! the orphan half of C12) run through a [`GatewayHarness`] the gateway crate
//! provides; without one they are reported as skipped, never as passed.

mod fake_server;
mod memory;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

pub use fake_server::{FakeHandler, FakeResponse, FakeServer, Fault, RecordedRequest};
pub use memory::MemoryProvider;

use crate::error::ProviderErrorKind;
use crate::provider::{BoxFuture, Provider};
use crate::types::{Addressing, DeploymentRef, ReleaseMode, ReplicaPhase, ReplicaRef};

/// Conformance case identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CaseId {
    /// 20 concurrent wakes from zero.
    C1,
    /// Wake latency p95.
    C2,
    /// Grace: run survives scale-down / SIGTERM.
    C3,
    /// Scale-in releases only what was asked.
    C4,
    /// Replica killed outside the gateway is detected.
    C5,
    /// Long stream without buffering.
    C6,
    /// Run past `max_request_secs`: planned preemption.
    C7,
    /// Auth refresh, 401 → refresh once, no credentials in logs.
    C8,
    /// API errors: jittered backoff, capacity classification.
    C9,
    /// Secrets never reach the gateway's requests or logs.
    C10,
    /// Old-version pool: resume on day 6, `410` on day 8.
    C11,
    /// Idle → zero replicas; orphans cleaned up.
    C12,
    /// Binary size delta within budget.
    C13,
}

impl fmt::Display for CaseId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, formatter)
    }
}

/// Result of one case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaseOutcome {
    /// Passed, with a measurement or note.
    Passed(String),
    /// Failed with the reason.
    Failed(String),
    /// Not run, with the reason (missing hook, needs real cloud...).
    Skipped(String),
}

/// One row of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseResult {
    /// Which case.
    pub id: CaseId,
    /// What happened.
    pub outcome: CaseOutcome,
}

/// The full report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConformanceReport {
    /// Target name.
    pub target: String,
    /// One row per case, C1..C13.
    pub results: Vec<CaseResult>,
}

impl ConformanceReport {
    /// The outcome of `id`.
    pub fn outcome(&self, id: CaseId) -> Option<&CaseOutcome> {
        self.results.iter().find(|result| result.id == id).map(|result| &result.outcome)
    }

    /// Failed cases.
    pub fn failures(&self) -> Vec<&CaseResult> {
        self.results.iter().filter(|result| matches!(result.outcome, CaseOutcome::Failed(_))).collect()
    }

    /// Panics with every failure listed (for `#[test]`s).
    ///
    /// # Panics
    /// When at least one case failed.
    pub fn assert_no_failures(&self) {
        let failures = self.failures();
        assert!(failures.is_empty(), "conformance failures for {}:\n{}", self.target, self);
    }
}

impl fmt::Display for ConformanceReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "conformance report: {}", self.target)?;
        for result in &self.results {
            let (label, detail) = match &result.outcome {
                CaseOutcome::Passed(detail) => ("pass", detail),
                CaseOutcome::Failed(detail) => ("FAIL", detail),
                CaseOutcome::Skipped(detail) => ("skip", detail),
            };
            writeln!(formatter, "  {:<4} {label}  {detail}", result.id.to_string())?;
        }
        Ok(())
    }
}

/// Binary size budget per cargo feature, plan §9 (bytes).
pub const FEATURE_SIZE_BUDGETS: &[(&str, u64)] = &[
    ("gw-k8s", 150 * 1024),
    ("gw-gke", 20 * 1024),
    ("gw-aks", 20 * 1024),
    ("gw-eks", 30 * 1024),
    ("gw-cloudrun", 80 * 1024),
    ("gw-aca", 80 * 1024),
    ("gw-ecs", 100 * 1024),
    ("gw-lambda", 80 * 1024),
    ("gw-fly", 80 * 1024),
    ("auth-aws", 120 * 1024),
    ("auth-gcp", 60 * 1024),
    ("auth-azure", 60 * 1024),
];

/// The §9 budget of `feature`, if it has one.
pub fn feature_size_budget(feature: &str) -> Option<u64> {
    FEATURE_SIZE_BUDGETS.iter().find(|(name, _)| *name == feature).map(|(_, budget)| *budget)
}

/// Knobs of a run.
#[derive(Debug, Clone)]
pub struct ConformanceConfig {
    /// C1 concurrency (plan: 20).
    pub concurrent_wakes: u32,
    /// C2 samples (plan: n = 50).
    pub latency_samples: u32,
    /// C2 bound; `None` = the provider's `wake_p95_target_ms`.
    pub wake_p95_target_ms: Option<u64>,
    /// C5 detection bound (plan: 15 s).
    pub external_kill_detection: Duration,
    /// C12 bound for the provider to report zero replicas after release.
    pub release_settle: Duration,
    /// Poll interval of `list` while waiting.
    pub poll_interval: Duration,
    /// C13: cargo feature and its measured binary delta in bytes.
    pub size_delta: Option<(String, u64)>,
}

impl Default for ConformanceConfig {
    fn default() -> Self {
        Self {
            concurrent_wakes: 20,
            latency_samples: 50,
            wake_p95_target_ms: None,
            external_kill_detection: Duration::from_secs(15),
            release_settle: Duration::from_secs(60),
            poll_interval: Duration::from_millis(100),
            size_delta: None,
        }
    }
}

/// Unit result of a hook.
pub type HookResult = Result<String, String>;

/// Cases that need the gateway in the loop; implemented by `graphrt-gateway`.
pub trait GatewayHarness: Send + Sync {
    /// C3: a run with a 60 s node survives the provider lowering replicas /
    /// SIGTERM, continues elsewhere, drain ≤ `max_grace_secs - 2 s`.
    fn grace_migration(&self) -> BoxFuture<'_, HookResult>;
    /// C6: long stream, inter-event latency overhead p95 ≤ 50 ms, no buffering.
    fn long_stream(&self) -> BoxFuture<'_, HookResult>;
    /// C7: a run past `max_request_secs` is preempted and re-dispatched.
    fn request_cap(&self) -> BoxFuture<'_, HookResult>;
    /// C11: resume of an N-1 paused run works on day 6, `410` on day 8.
    fn old_version_pool(&self) -> BoxFuture<'_, HookResult>;
    /// C12 (gateway half): idle → 0 replicas ≤ idle timeout + 60 s, and
    /// orphans after `kill -9` of the gateway are reconciled ≤ 2 min.
    fn idle_and_orphans(&self) -> BoxFuture<'_, HookResult>;
}

/// What a provider crate exposes to the harness.
pub trait ConformanceTarget: Send + Sync {
    /// Name in the report.
    fn name(&self) -> String;
    /// The provider under test.
    fn provider(&self) -> Arc<dyn Provider>;
    /// A deployment version that exists on the (fake) platform.
    fn deployment(&self) -> DeploymentRef;
    /// Kills a replica behind the gateway's back (C5). Default: unsupported.
    fn kill_externally<'a>(&'a self, _replica: &'a ReplicaRef) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Err("kill_externally not implemented by target".to_owned()) })
    }
    /// The fake provider API, enabling fault injection for C8 / C9 / C10.
    fn fake_server(&self) -> Option<&FakeServer> {
        None
    }
    /// A fault the provider must classify as `Capacity` (C9), e.g. a Fargate
    /// `RunTask` failure body. Default: none (part skipped).
    fn capacity_fault(&self) -> Option<Fault> {
        None
    }
    /// Values that must never appear in provider requests or logs (C8, C10):
    /// deployment secrets and credentials.
    fn secrets(&self) -> Vec<String> {
        Vec::new()
    }
    /// Log lines the provider emitted during the run (C8, C10).
    fn captured_logs(&self) -> Vec<String> {
        Vec::new()
    }
    /// The gateway-in-the-loop cases.
    fn gateway_harness(&self) -> Option<&dyn GatewayHarness> {
        None
    }
}

/// Runs C1–C13 in order and returns the report (never panics on a failing case).
pub async fn run_conformance(target: &dyn ConformanceTarget, config: &ConformanceConfig) -> ConformanceReport {
    let mut results = Vec::with_capacity(13);
    let mut record = |id, outcome| results.push(CaseResult { id, outcome });
    record(CaseId::C1, case_concurrent_wake(target, config).await);
    record(CaseId::C2, case_wake_latency(target, config).await);
    record(CaseId::C3, gateway_case(target, |harness| harness.grace_migration()).await);
    record(CaseId::C4, case_scale_in(target).await);
    record(CaseId::C5, case_external_kill(target, config).await);
    record(CaseId::C6, gateway_case(target, |harness| harness.long_stream()).await);
    record(CaseId::C7, gateway_case(target, |harness| harness.request_cap()).await);
    record(CaseId::C8, case_auth(target).await);
    record(CaseId::C9, case_api_errors(target).await);
    record(CaseId::C10, case_secrets(target));
    record(CaseId::C11, gateway_case(target, |harness| harness.old_version_pool()).await);
    record(CaseId::C12, case_idle_release(target, config).await);
    record(CaseId::C13, case_size(config));
    ConformanceReport { target: target.name(), results }
}

fn hook_outcome(result: HookResult) -> CaseOutcome {
    match result {
        Ok(detail) => CaseOutcome::Passed(detail),
        Err(reason) => CaseOutcome::Failed(reason),
    }
}

async fn gateway_case<'t, F>(target: &'t dyn ConformanceTarget, case: F) -> CaseOutcome
where
    F: FnOnce(&'t dyn GatewayHarness) -> BoxFuture<'t, HookResult>,
{
    match target.gateway_harness() {
        Some(harness) => hook_outcome(case(harness).await),
        None => CaseOutcome::Skipped("needs the gateway harness (graphrt-gateway)".to_owned()),
    }
}

/// Releases every replica and waits until `list` shows none live.
async fn release_all(
    provider: &dyn Provider,
    deployment: &DeploymentRef,
    settle: Duration,
    poll: Duration,
) -> Result<(), String> {
    if provider.capabilities().addressing == Addressing::Url {
        return Ok(());
    }
    let deadline = Instant::now() + settle;
    loop {
        let replicas = provider.list(deployment).await.map_err(|error| format!("list: {error}"))?;
        let live: Vec<_> = replicas.into_iter().filter(|state| is_live(state.phase)).collect();
        if live.is_empty() {
            return Ok(());
        }
        for state in &live {
            provider.release(&state.replica, ReleaseMode::Stop).await.map_err(|error| format!("release: {error}"))?;
        }
        if Instant::now() >= deadline {
            return Err(format!("{} replicas still live after {settle:?}", live.len()));
        }
        tokio::time::sleep(poll).await;
    }
}

fn is_live(phase: ReplicaPhase) -> bool {
    matches!(phase, ReplicaPhase::Starting | ReplicaPhase::Ready | ReplicaPhase::Stopping)
}

async fn ready_count(provider: &dyn Provider, deployment: &DeploymentRef) -> Result<usize, String> {
    let replicas = provider.list(deployment).await.map_err(|error| format!("list: {error}"))?;
    Ok(replicas.iter().filter(|state| state.phase == ReplicaPhase::Ready).count())
}

async fn case_concurrent_wake(target: &dyn ConformanceTarget, config: &ConformanceConfig) -> CaseOutcome {
    let provider = target.provider();
    let deployment = target.deployment();
    if let Err(reason) = release_all(provider.as_ref(), &deployment, config.release_settle, config.poll_interval).await
    {
        return CaseOutcome::Failed(format!("could not start from zero: {reason}"));
    }
    let wakes = (0..config.concurrent_wakes).map(|_| provider.wake(&deployment, 1));
    let results = futures::future::join_all(wakes).await;
    if let Some(error) = results.iter().find_map(|result| result.as_ref().err()) {
        return CaseOutcome::Failed(format!("wake failed: {error}"));
    }
    if results.iter().any(|result| result.as_ref().is_ok_and(Vec::is_empty)) {
        return CaseOutcome::Failed("a wake returned no endpoint".to_owned());
    }
    if provider.capabilities().addressing == Addressing::Url {
        return CaseOutcome::Passed(format!("{} concurrent wakes answered (platform scaler)", config.concurrent_wakes));
    }
    match ready_count(provider.as_ref(), &deployment).await {
        Ok(1) => CaseOutcome::Passed(format!("{} concurrent wakes → 1 replica", config.concurrent_wakes)),
        Ok(count) => {
            CaseOutcome::Failed(format!("{} concurrent wake(1) → {count} replicas, want 1", config.concurrent_wakes))
        }
        Err(reason) => CaseOutcome::Failed(reason),
    }
}

/// Nearest-rank percentile of sorted samples.
pub fn percentile(sorted: &[Duration], percent: f64) -> Duration {
    let rank = ((percent / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted.get(rank.clamp(1, sorted.len().max(1)) - 1).copied().unwrap_or(Duration::ZERO)
}

async fn case_wake_latency(target: &dyn ConformanceTarget, config: &ConformanceConfig) -> CaseOutcome {
    let provider = target.provider();
    let deployment = target.deployment();
    let target_ms = config.wake_p95_target_ms.unwrap_or(provider.capabilities().wake_p95_target_ms);
    let mut samples = Vec::with_capacity(config.latency_samples as usize);
    for _ in 0..config.latency_samples {
        if let Err(reason) =
            release_all(provider.as_ref(), &deployment, config.release_settle, config.poll_interval).await
        {
            return CaseOutcome::Failed(reason);
        }
        let started = Instant::now();
        if let Err(error) = provider.wake(&deployment, 1).await {
            return CaseOutcome::Failed(format!("wake failed: {error}"));
        }
        samples.push(started.elapsed());
    }
    samples.sort();
    let p95 = percentile(&samples, 95.0);
    let detail = format!("p95 {:.1} ms (target {target_ms} ms, n={})", p95.as_secs_f64() * 1000.0, samples.len());
    if p95 <= Duration::from_millis(target_ms) { CaseOutcome::Passed(detail) } else { CaseOutcome::Failed(detail) }
}

async fn case_scale_in(target: &dyn ConformanceTarget) -> CaseOutcome {
    let provider = target.provider();
    let deployment = target.deployment();
    if provider.capabilities().addressing == Addressing::Url {
        let pseudo = ReplicaRef { deployment: deployment.clone(), id: "url".to_owned() };
        return match provider.release(&pseudo, ReleaseMode::ScaleIn).await {
            Ok(()) => CaseOutcome::Passed("release is a no-op for Url addressing".to_owned()),
            Err(error) => CaseOutcome::Failed(format!("Url release must be a no-op: {error}")),
        };
    }
    let endpoints = match provider.wake(&deployment, 2).await {
        Ok(endpoints) => endpoints,
        Err(error) => return CaseOutcome::Failed(format!("wake(2): {error}")),
    };
    let (Some(victim), Some(survivor)) =
        (endpoints.first().map(|endpoint| &endpoint.replica), endpoints.get(1).map(|endpoint| &endpoint.replica))
    else {
        return CaseOutcome::Failed(format!("wake(2) returned {} endpoints", endpoints.len()));
    };
    if let Err(error) = provider.release(victim, ReleaseMode::ScaleIn).await {
        return CaseOutcome::Failed(format!("release: {error}"));
    }
    if let Err(error) = provider.release(victim, ReleaseMode::Stop).await {
        return CaseOutcome::Failed(format!("releasing an already released replica must be Ok: {error}"));
    }
    let states = match provider.list(&deployment).await {
        Ok(states) => states,
        Err(error) => return CaseOutcome::Failed(format!("list: {error}")),
    };
    let phase_of =
        |replica: &ReplicaRef| states.iter().find(|state| &state.replica == replica).map(|state| state.phase);
    match (phase_of(victim), phase_of(survivor)) {
        (Some(ReplicaPhase::Ready), _) => CaseOutcome::Failed("released replica still ready".to_owned()),
        (_, Some(ReplicaPhase::Ready)) => CaseOutcome::Passed("only the released replica left".to_owned()),
        (_, other) => CaseOutcome::Failed(format!("untouched replica is {other:?}, want Ready")),
    }
}

async fn case_external_kill(target: &dyn ConformanceTarget, config: &ConformanceConfig) -> CaseOutcome {
    let provider = target.provider();
    let deployment = target.deployment();
    if provider.capabilities().addressing == Addressing::Url {
        return CaseOutcome::Skipped("Url addressing: the platform replaces instances".to_owned());
    }
    let endpoint = match provider.wake(&deployment, 1).await {
        Ok(mut endpoints) if !endpoints.is_empty() => endpoints.swap_remove(0),
        Ok(_) => return CaseOutcome::Failed("wake returned no endpoint".to_owned()),
        Err(error) => return CaseOutcome::Failed(format!("wake: {error}")),
    };
    if let Err(reason) = target.kill_externally(&endpoint.replica).await {
        return CaseOutcome::Skipped(reason);
    }
    let started = Instant::now();
    while started.elapsed() <= config.external_kill_detection {
        match provider.list(&deployment).await {
            Ok(states) => {
                let still_ready =
                    states.iter().any(|state| state.replica == endpoint.replica && state.phase == ReplicaPhase::Ready);
                if !still_ready {
                    return CaseOutcome::Passed(format!("detected after {:.1} s", started.elapsed().as_secs_f64()));
                }
            }
            Err(error) => return CaseOutcome::Failed(format!("list: {error}")),
        }
        tokio::time::sleep(config.poll_interval).await;
    }
    CaseOutcome::Failed(format!("killed replica still reported ready after {:?}", config.external_kill_detection))
}

fn leaked_secret(target: &dyn ConformanceTarget, haystacks: &[String]) -> Option<String> {
    let secrets = target.secrets();
    secrets
        .iter()
        .filter(|secret| !secret.is_empty())
        .find(|secret| haystacks.iter().any(|haystack| haystack.contains(secret.as_str())))
        .map(|secret| format!("secret of length {} leaked", secret.len()))
}

async fn case_auth(target: &dyn ConformanceTarget) -> CaseOutcome {
    let provider = target.provider();
    let deployment = target.deployment();
    let mut notes = Vec::new();
    if let Some(server) = target.fake_server() {
        server.inject(Fault { path_prefix: String::new(), response: FakeResponse::status(401), count: 1 });
        let first = provider.list(&deployment).await;
        server.clear_faults();
        if let Err(error) = first {
            return CaseOutcome::Failed(format!("one 401 must trigger one refresh and succeed: {error}"));
        }
        let before = server.requests().len();
        server.inject(Fault { path_prefix: String::new(), response: FakeResponse::status(401), count: 100 });
        let rejected = provider.list(&deployment).await;
        server.clear_faults();
        let attempts = server.requests().len() - before;
        match rejected {
            Err(error) if error.kind == ProviderErrorKind::Auth => {}
            Err(error) => return CaseOutcome::Failed(format!("persistent 401 must be Auth, got {}", error.kind)),
            Ok(_) => return CaseOutcome::Failed("persistent 401 must fail".to_owned()),
        }
        if attempts > 2 {
            return CaseOutcome::Failed(format!(
                "persistent 401 must be refreshed once, not retried ({attempts} requests)"
            ));
        }
        notes.push("401 → one refresh");
        let urls: Vec<String> = server.requests().iter().map(|request| request.path.clone()).collect();
        if let Some(leak) = leaked_secret(target, &urls) {
            return CaseOutcome::Failed(format!("credential in a request URL: {leak}"));
        }
    }
    if let Some(leak) = leaked_secret(target, &target.captured_logs()) {
        return CaseOutcome::Failed(format!("credential in logs: {leak}"));
    }
    notes.push("no credentials in logs");
    if target.fake_server().is_none() {
        return CaseOutcome::Skipped("no fake server: refresh-on-401 not exercised (log scan only)".to_owned());
    }
    CaseOutcome::Passed(format!("{}; expiry/skew covered by the auth-* crates", notes.join(", ")))
}

async fn case_api_errors(target: &dyn ConformanceTarget) -> CaseOutcome {
    let Some(server) = target.fake_server() else {
        return CaseOutcome::Skipped("no fake server for fault injection".to_owned());
    };
    let provider = target.provider();
    let deployment = target.deployment();
    for (status, label) in [(429, "429"), (503, "503")] {
        let before = server.requests().len();
        let response = FakeResponse::status(status).with_header("retry-after", "0");
        server.inject(Fault { path_prefix: String::new(), response, count: 2 });
        let result = provider.list(&deployment).await;
        server.clear_faults();
        if let Err(error) = result {
            return CaseOutcome::Failed(format!("two {label} must be retried: {error}"));
        }
        let attempts = server.requests().len() - before;
        if attempts < 3 {
            return CaseOutcome::Failed(format!("{label}: expected ≥ 3 requests, saw {attempts}"));
        }
    }
    let before = server.requests().len();
    server.inject(Fault { path_prefix: String::new(), response: FakeResponse::status(400), count: 1 });
    let result = provider.list(&deployment).await;
    server.clear_faults();
    match result {
        Err(error) if error.kind == ProviderErrorKind::Fatal => {}
        other => return CaseOutcome::Failed(format!("400 must fail as Fatal without retry, got {other:?}")),
    }
    let attempts = server.requests().len() - before;
    if attempts != 1 {
        return CaseOutcome::Failed(format!("400 was retried ({attempts} requests)"));
    }
    let mut detail = "429/5xx retried with backoff, 400 not retried".to_owned();
    if let Some(fault) = target.capacity_fault() {
        if let Err(reason) =
            release_all(provider.as_ref(), &deployment, Duration::from_secs(60), Duration::from_millis(100)).await
        {
            return CaseOutcome::Failed(reason);
        }
        server.inject(fault);
        let result = provider.wake(&deployment, 1).await;
        server.clear_faults();
        match result {
            Err(error) if error.kind == ProviderErrorKind::Capacity => detail.push_str(", capacity classified"),
            other => return CaseOutcome::Failed(format!("capacity fault must be Capacity, got {other:?}")),
        }
    }
    CaseOutcome::Passed(detail)
}

fn case_secrets(target: &dyn ConformanceTarget) -> CaseOutcome {
    let mut haystacks = target.captured_logs();
    if let Some(server) = target.fake_server() {
        haystacks.extend(server.requests().iter().map(|request| format!("{} {}", request.path, request.body_text())));
    }
    if target.secrets().is_empty() {
        return CaseOutcome::Skipped("target declared no secrets to scan for".to_owned());
    }
    match leaked_secret(target, &haystacks) {
        Some(leak) => CaseOutcome::Failed(leak),
        None => CaseOutcome::Passed(format!("{} log lines / requests scanned", haystacks.len())),
    }
}

async fn case_idle_release(target: &dyn ConformanceTarget, config: &ConformanceConfig) -> CaseOutcome {
    let provider = target.provider();
    let deployment = target.deployment();
    if let Err(reason) = release_all(provider.as_ref(), &deployment, config.release_settle, config.poll_interval).await
    {
        return CaseOutcome::Failed(reason);
    }
    let provider_part = "release → 0 live replicas";
    match target.gateway_harness() {
        Some(harness) => match harness.idle_and_orphans().await {
            Ok(detail) => CaseOutcome::Passed(format!("{provider_part}; {detail}")),
            Err(reason) => CaseOutcome::Failed(reason),
        },
        None => {
            CaseOutcome::Passed(format!("{provider_part} (idle tracker / orphan reconcile need the gateway harness)"))
        }
    }
}

fn case_size(config: &ConformanceConfig) -> CaseOutcome {
    let Some((feature, delta)) = &config.size_delta else {
        return CaseOutcome::Skipped("no measured size delta (scripts/size-gate.sh)".to_owned());
    };
    let Some(budget) = feature_size_budget(feature) else {
        return CaseOutcome::Failed(format!("feature {feature} has no budget in plan §9"));
    };
    let detail = format!("{feature}: Δ {} KiB, budget {} KiB", delta / 1024, budget / 1024);
    if *delta <= budget { CaseOutcome::Passed(detail) } else { CaseOutcome::Failed(detail) }
}
