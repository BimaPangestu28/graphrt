//! The shared C1-C13 harness of `graphrt-provider-api` (`run_conformance`)
//! against the Container Apps provider, wired to the harness's own
//! `FakeServer` emulating the managed identity endpoint, ARM and the ingress.
//! Gateway-in-the-loop cases (C3, C6, C7, C11) are skipped until the gateway's
//! `GatewayHarness` exists; their provider halves are in `conformance.rs` and
//! `idle_timeout.rs`.
#![cfg(feature = "provider-aca")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;
use std::time::Duration;

use graphrt_provider_api::conformance::{
    CaseId, CaseOutcome, ConformanceConfig, ConformanceTarget, FakeResponse, FakeServer, RecordedRequest,
    run_conformance,
};
use graphrt_provider_api::http::{RetryPolicy, SecretString};
use graphrt_provider_api::{DeploymentRef, Provider};
use graphrt_provider_azure::aca::{AcaConfig, AcaProvider};
use graphrt_provider_azure::arm::{ArmClient, ArmConfig};
use graphrt_provider_azure::auth::{AzureCredential, TokenProvider};
use serde_json::json;

const IDENTITY_HEADER: &str = "identity-header-value-7f3c";
const ACCESS_TOKEN: &str = "eyJ0eXAi.fake-access-token.sig";
const FQDN: &str = "bot.internal.fake-env.westeurope.azurecontainerapps.io";

/// Managed identity endpoint, `GET app`, `GET …/revisions/bot--v1/replicas`, ingress `/healthz`.
fn handle(request: &RecordedRequest) -> FakeResponse {
    let path = request.path.split('?').next().unwrap_or("");
    if path == "/msi/token" {
        if request.header("x-identity-header") != Some(IDENTITY_HEADER) {
            return FakeResponse::status(400);
        }
        let expires_on = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 3600;
        return FakeResponse::json(200, &json!({"access_token": ACCESS_TOKEN, "expires_on": expires_on.to_string()}));
    }
    if path == "/healthz" {
        return FakeResponse::status(200);
    }
    if request.header("authorization") != Some(&format!("Bearer {ACCESS_TOKEN}")) {
        return FakeResponse::status(401);
    }
    let app = "/subscriptions/sub/resourceGroups/rg/providers/Microsoft.App/containerApps/bot";
    if path == app {
        return FakeResponse::json(
            200,
            &json!({"properties": {"latestRevisionName": "bot--v1", "configuration": {"ingress": {
                "fqdn": FQDN, "traffic": [{"revisionName": "bot--v1", "weight": 100, "label": "v1"}]}}}}),
        );
    }
    if path == format!("{app}/revisions/bot--v1/replicas") {
        return FakeResponse::json(
            200,
            &json!({"value": [{"name": "bot--v1-abc", "properties": {"runningState": "Running", "createdTime": "2026-11-22T00:00:00Z"}}]}),
        );
    }
    FakeResponse::status(404)
}

struct AcaTarget {
    server: FakeServer,
    provider: Arc<AcaProvider>,
}

impl ConformanceTarget for AcaTarget {
    fn name(&self) -> String {
        "azure-container-apps (fake)".into()
    }
    fn provider(&self) -> Arc<dyn Provider> {
        self.provider.clone()
    }
    fn deployment(&self) -> DeploymentRef {
        DeploymentRef::new("bot", "v1", "bot")
    }
    fn fake_server(&self) -> Option<&FakeServer> {
        Some(&self.server)
    }
    fn secrets(&self) -> Vec<String> {
        vec![IDENTITY_HEADER.into(), ACCESS_TOKEN.into()]
    }
    fn captured_logs(&self) -> Vec<String> {
        // The crate does not log; its Debug output is what would reach a log.
        vec![format!("{:?}", self.provider)]
    }
}

async fn target() -> AcaTarget {
    let server = FakeServer::start(Arc::new(handle)).await.unwrap();
    let credential = AzureCredential::AppServiceIdentity {
        endpoint: format!("{}/msi/token", server.base_url()),
        header: SecretString::new(IDENTITY_HEADER),
        client_id: None,
    };
    let tokens = Arc::new(TokenProvider::new(credential).unwrap());
    let retry =
        RetryPolicy { max_attempts: 4, base_delay: Duration::from_millis(5), max_delay: Duration::from_millis(20) };
    let arm = ArmClient::new(ArmConfig { endpoint: server.base_url(), retry, ..ArmConfig::default() }, tokens).unwrap();
    let mut config = AcaConfig::new("sub", "rg");
    config.invoke_scheme = "http".into();
    let address = server.base_url().trim_start_matches("http://").parse().unwrap();
    config.resolve_overrides = vec![(format!("bot---v1.{}", FQDN.split_once('.').unwrap().1), address)];
    AcaTarget { server, provider: Arc::new(AcaProvider::new(arm, config).unwrap()) }
}

#[tokio::test]
async fn shared_harness_passes_every_provider_level_case() {
    let target = target().await;
    // C13 from crates/graphrt-provider-azure/measure-size.sh (ACA alone, bytes), when exported.
    let size_delta = std::env::var("GRAPHRT_SIZE_DELTA_GW_ACA").ok().and_then(|bytes| bytes.parse().ok());
    let config = ConformanceConfig {
        latency_samples: 50,
        poll_interval: Duration::from_millis(5),
        size_delta: size_delta.map(|bytes| ("gw-aca".to_string(), bytes)),
        ..ConformanceConfig::default()
    };
    let report = run_conformance(&target, &config).await;
    eprintln!("{report}");
    report.assert_no_failures();
    for id in [CaseId::C1, CaseId::C2, CaseId::C4, CaseId::C8, CaseId::C9, CaseId::C10, CaseId::C12] {
        assert!(matches!(report.outcome(id), Some(CaseOutcome::Passed(_))), "{id} must pass: {report}");
    }
}
