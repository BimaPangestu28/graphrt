//! The shared C1–C13 harness of `graphrt-provider-api`, run against the fake AWS
//! APIs. Cases the harness skips here (C8/C9 fault injection, the gateway-in-the-loop
//! cases) are covered by `conformance_ecs.rs`, `conformance_lambda.rs` and
//! `auth_chain.rs`, or need the gateway / real AWS (see `docs/gateway-aws.md`).
#![cfg(all(feature = "provider-ecs", feature = "provider-lambda"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::print_stdout)]

mod support;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use graphrt_provider_api::conformance::{CaseId, CaseOutcome, ConformanceConfig, ConformanceTarget, run_conformance};
use graphrt_provider_aws::api::{BoxFuture, DeploymentRef, Provider, ReplicaRef};
use graphrt_provider_aws::ecs::{EcsConfig, EcsProvider, EcsTaskTemplate};
use graphrt_provider_aws::lambda::{LambdaConfig, LambdaProvider};
use support::{FakeAws, fast_retry};

struct EcsTarget {
    fake: FakeAws,
    provider: Arc<EcsProvider>,
}

impl ConformanceTarget for EcsTarget {
    fn name(&self) -> String {
        "ecs-fargate (fake ECS API)".into()
    }
    fn provider(&self) -> Arc<dyn Provider> {
        Arc::clone(&self.provider) as Arc<dyn Provider>
    }
    fn deployment(&self) -> DeploymentRef {
        DeploymentRef::new("chat", "7", "")
    }
    fn kill_externally<'a>(&'a self, replica: &'a ReplicaRef) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let mut state = self.fake.state.lock().unwrap();
            let task = state.tasks.iter_mut().find(|t| t.arn == replica.id).ok_or("unknown task")?;
            task.stopped = true;
            task.stopped_reason = Some("Essential container in task exited".into());
            Ok(())
        })
    }
    fn secrets(&self) -> Vec<String> {
        vec![support::SECRET_KEY.into(), support::SESSION_TOKEN.into()]
    }
    fn captured_logs(&self) -> Vec<String> {
        // Everything the provider could print about itself or its config.
        vec![format!("{:?}", self.provider)]
    }
}

fn ecs_provider(fake: &FakeAws) -> EcsProvider {
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
        limits: fast_retry(),
        deployments: HashMap::from([("chat".to_owned(), template)]),
        wake_timeout: Duration::from_secs(5),
        poll_interval: Duration::from_millis(10),
    };
    EcsProvider::new(fake.chain(), config).unwrap()
}

fn config() -> ConformanceConfig {
    ConformanceConfig {
        // The fake provisions in 30 ms; the real 45 s target needs Fargate.
        wake_p95_target_ms: Some(500),
        release_settle: Duration::from_secs(5),
        poll_interval: Duration::from_millis(20),
        ..ConformanceConfig::default()
    }
}

fn assert_passed(report: &graphrt_provider_api::conformance::ConformanceReport, cases: &[CaseId]) {
    for case in cases {
        assert!(matches!(report.outcome(*case), Some(CaseOutcome::Passed(_))), "{case} did not pass:\n{report}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ecs_passes_the_shared_harness() {
    let fake = support::start().await;
    fake.state.lock().unwrap().provisioning = Duration::from_millis(30);
    let provider = Arc::new(ecs_provider(&fake));
    let target = EcsTarget { fake, provider };
    let report = run_conformance(&target, &config()).await;
    println!("{report}");
    report.assert_no_failures();
    assert_passed(&report, &[CaseId::C1, CaseId::C2, CaseId::C4, CaseId::C5, CaseId::C10, CaseId::C12]);
}

struct LambdaTarget {
    provider: Arc<LambdaProvider>,
}

impl ConformanceTarget for LambdaTarget {
    fn name(&self) -> String {
        "lambda (fake Lambda API)".into()
    }
    fn provider(&self) -> Arc<dyn Provider> {
        Arc::clone(&self.provider) as Arc<dyn Provider>
    }
    fn deployment(&self) -> DeploymentRef {
        DeploymentRef::new("chat", "7", "arn:aws:lambda:eu-west-1:123:function:graphrt-chat")
    }
    fn secrets(&self) -> Vec<String> {
        vec![support::SECRET_KEY.into(), support::SESSION_TOKEN.into()]
    }
    fn captured_logs(&self) -> Vec<String> {
        vec![format!("{:?}", self.provider)]
    }
}

#[tokio::test]
async fn lambda_passes_the_shared_harness() {
    let fake = support::start().await;
    let config =
        LambdaConfig { endpoint: fake.endpoint("lambda"), limits: fast_retry(), ..LambdaConfig::new("eu-west-1") };
    let provider = Arc::new(LambdaProvider::new(fake.chain(), config).unwrap());
    let report = run_conformance(&LambdaTarget { provider }, &config_for_url()).await;
    println!("{report}");
    report.assert_no_failures();
    assert_passed(&report, &[CaseId::C1, CaseId::C2, CaseId::C4, CaseId::C10, CaseId::C12]);
}

fn config_for_url() -> ConformanceConfig {
    ConformanceConfig { wake_p95_target_ms: Some(1_000), ..config() }
}
