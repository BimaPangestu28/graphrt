//! Provider conformance C1–C13 (plan §7) of the Fly provider against the fake
//! Machines API. Gateway-in-the-loop cases (C3, C6, C7, C11) are reported as
//! skipped here and run in `graphrt-gateway` with its harness.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::print_stdout)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use graphrt_provider_api::conformance::{
    CaseId, CaseOutcome, ConformanceConfig, ConformanceTarget, FakeResponse, FakeServer, Fault, run_conformance,
};
use graphrt_provider_api::{BoxFuture, DeploymentRef, Provider, ReplicaRef};
use graphrt_provider_fly::{FlyProvider, FlyProviderConfig, FlyToken, TokenOrigin};
use support::{APP, FakeFly, TOKEN};

const POOL_SIZE: u32 = 3;

struct FlyTarget {
    fake: FakeFly,
    provider: Arc<FlyProvider>,
}

impl FlyTarget {
    async fn start() -> Self {
        support::install_crypto_provider();
        let fake = FakeFly::start().await;
        for index in 0..POOL_SIZE {
            fake.seed_machine("chat", "v1", index, "created", 512);
        }
        let mut config = FlyProviderConfig::single_app(APP, TokenOrigin::Fixed(FlyToken::parse(TOKEN).unwrap()));
        config.api_base_url = fake.server.base_url();
        // No pool growth: C9's capacity fault must exhaust the pool.
        config.max_machines_per_version = POOL_SIZE;
        config.start_timeout = Duration::from_secs(5);
        // C2 measures the provider, not the production rate limit (20/s) that
        // ~400 calls of the suite would otherwise wait on.
        config.client_policy.requests_per_second = 10_000.0;
        config.client_policy.burst = 10_000;
        let provider = Arc::new(FlyProvider::new(config).unwrap());
        Self { fake, provider }
    }
}

impl ConformanceTarget for FlyTarget {
    fn name(&self) -> String {
        "fly (fake Machines API)".to_owned()
    }

    fn provider(&self) -> Arc<dyn Provider> {
        self.provider.clone()
    }

    fn deployment(&self) -> DeploymentRef {
        DeploymentRef::new("chat", "v1", APP)
    }

    fn kill_externally<'a>(&'a self, replica: &'a ReplicaRef) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let mut state = self.fake.state();
            let machine =
                state.machines.iter_mut().find(|machine| machine.id == replica.id).ok_or("unknown machine")?;
            // OOM kill / host failure with `restart.policy = no`: Fly reports it stopped.
            machine.state = "stopped".to_owned();
            Ok(())
        })
    }

    fn fake_server(&self) -> Option<&FakeServer> {
        Some(&self.fake.server)
    }

    fn capacity_fault(&self) -> Option<Fault> {
        let body = serde_json::json!({"error": "could not reserve resource for machine: insufficient memory available to fulfill request"});
        // Every start of the (exhausted) pool hits a full host.
        Some(Fault {
            path_prefix: format!("/v1/apps/{APP}/machines/"),
            response: FakeResponse::json(409, &body),
            count: POOL_SIZE,
        })
    }

    fn secrets(&self) -> Vec<String> {
        vec![TOKEN.to_owned()]
    }
}

fn measured_size_delta() -> Option<(String, u64)> {
    let bytes = std::env::var("GRAPHRT_FLY_SIZE_DELTA_BYTES").ok()?.parse().ok()?;
    Some(("gw-fly".to_owned(), bytes))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fly_provider_passes_conformance_against_the_fake_api() {
    let target = FlyTarget::start().await;
    let config = ConformanceConfig {
        external_kill_detection: Duration::from_secs(15),
        release_settle: Duration::from_secs(10),
        poll_interval: Duration::from_millis(20),
        size_delta: measured_size_delta(),
        ..ConformanceConfig::default()
    };
    let report = run_conformance(&target, &config).await;
    println!("{report}");
    report.assert_no_failures();
    for case in [CaseId::C1, CaseId::C2, CaseId::C4, CaseId::C5, CaseId::C8, CaseId::C9, CaseId::C10, CaseId::C12] {
        assert!(
            matches!(report.outcome(case), Some(CaseOutcome::Passed(_))),
            "{case} must run and pass: {:?}",
            report.outcome(case)
        );
    }
}
