//! The shared C1–C13 harness of `graphrt-provider-api` run against the Cloud
//! Run provider wired to fake Google APIs. The Cloud Run-specific checks of the
//! same cases are in `cloud_run_conformance.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use graphrt_gcp::cloud_run::{CloudRunDeployDriver, CloudRunProvider};
use graphrt_gcp::secret_manager::SecretManager;
use graphrt_provider_api::conformance::{
    CaseId, CaseOutcome, ConformanceConfig, ConformanceTarget, FakeServer, run_conformance,
};
use graphrt_provider_api::http::SecretString;
use graphrt_provider_api::{DeployDriver, DeploymentRef, Provider};
use support::{FakeGoogle, FakeInstance, service_resource, spec};

const SECRET_VALUE: &str = "sk-live-harness-secret-0042";

struct CloudRunTarget {
    google: FakeGoogle,
    _instance: FakeInstance,
    provider: Arc<CloudRunProvider>,
}

impl ConformanceTarget for CloudRunTarget {
    fn name(&self) -> String {
        "cloud-run (fake Google APIs)".to_owned()
    }
    fn provider(&self) -> Arc<dyn Provider> {
        self.provider.clone()
    }
    fn deployment(&self) -> DeploymentRef {
        DeploymentRef::new("chat", "v1", service_resource("chat"))
    }
    fn fake_server(&self) -> Option<&FakeServer> {
        Some(&self.google.server)
    }
    fn secrets(&self) -> Vec<String> {
        vec![SECRET_VALUE.to_owned()]
    }
    fn captured_logs(&self) -> Vec<String> {
        vec![format!("{:?}", self.provider), format!("{:?}", self.google.metadata_auth())]
    }
}

async fn target() -> CloudRunTarget {
    let instance = FakeInstance::start().await;
    let google = FakeGoogle::start(&instance.url).await;
    let deploy_auth = google.service_account_auth();
    let secrets = SecretManager::new(&google.config(), &deploy_auth).unwrap();
    let references = secrets
        .store_secrets("chat-", &[("OPENAI_API_KEY".to_owned(), SecretString::new(SECRET_VALUE))])
        .await
        .unwrap();
    let mut deployment_spec = spec("chat", "v1");
    deployment_spec.secrets = references;
    CloudRunDeployDriver::new(google.config(), &deploy_auth).unwrap().apply(&deployment_spec).await.unwrap();
    let mut config = google.config();
    // Every C2 sample probes the instance instead of using the warm shortcut.
    config.warm_window = Duration::ZERO;
    let provider = Arc::new(CloudRunProvider::new(config, google.metadata_auth()).unwrap());
    CloudRunTarget { google, _instance: instance, provider }
}

#[tokio::test]
async fn shared_harness_passes_for_cloud_run() {
    let target = target().await;
    // C13: measured min-size delta of the gateway feature (see docs/gateway-cloud-run.md).
    let config =
        ConformanceConfig { size_delta: Some(("gw-cloudrun".to_owned(), 49_968)), ..ConformanceConfig::default() };
    let report = run_conformance(&target, &config).await;
    eprintln!("{report}");
    report.assert_no_failures();
    for id in [CaseId::C1, CaseId::C2, CaseId::C4, CaseId::C8, CaseId::C9, CaseId::C10, CaseId::C12, CaseId::C13] {
        assert!(matches!(report.outcome(id), Some(CaseOutcome::Passed(_))), "{id}: {:?}", report.outcome(id));
    }
}
