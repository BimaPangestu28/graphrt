//! Provider conformance C1-C13 (plan §7) for `azure-container-apps` against
//! the fake Azure in `common`. C6 (long streams through an idle-timeout
//! ingress) lives in `idle_timeout.rs`; C13 (binary size) is measured by
//! `scripts/measure-provider-size.sh`. What only real Azure can prove is
//! listed in docs/deploy-azure.md.
#![cfg(feature = "deploy-aca")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use common::FakeAzure;
use graphrt_provider_api::http::{RetryPolicy, SecretString};
use graphrt_provider_api::{
    Addressing, DeployDriver, DeployErrorKind, DeploymentRef, DeploymentSpec, EndpointAddress, Provider, ProviderError,
    ProviderErrorKind, ReleaseMode, ReplicaPhase, ReplicaRef, Resources, Scaler, SecretRef,
};
use graphrt_provider_azure::aca::revisions::parse_rfc3339;
use graphrt_provider_azure::aca::spec::{DEFAULT_GRACE_SECS, MAX_GRACE_SECS};
use graphrt_provider_azure::aca::{
    AcaAppSpec, AcaConfig, AcaDeployDriver, AcaProvider, IngressMode, heartbeat_keeps_stream_open,
    planned_preempt_after,
};
use graphrt_provider_azure::arm::{ArmClient, ArmConfig};
use graphrt_provider_azure::auth::{ARM_SCOPE, AuthEndpoints, AzureCredential, TokenProvider};
use graphrt_provider_azure::keyvault::{KeyVaultClient, KeyVaultConfig};

const APP: &str = "bot";
const DAY: u64 = 86_400;

fn secret_credential(fake: &FakeAzure, secret: &str) -> AzureCredential {
    AzureCredential::ClientSecret {
        authority_host: fake.base(),
        tenant_id: "tenant".into(),
        client_id: "client".into(),
        secret: SecretString::new(secret),
    }
}

fn arm_config(fake: &FakeAzure) -> ArmConfig {
    ArmConfig {
        endpoint: fake.base(),
        retry: RetryPolicy {
            max_attempts: 4,
            base_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(20),
        },
        poll_interval: Duration::from_millis(5),
        ..ArmConfig::default()
    }
}

fn aca_config(fake: &FakeAzure) -> AcaConfig {
    let mut config = AcaConfig::new("sub", "rg");
    config.invoke_scheme = "http".into();
    config.wake_timeout = Duration::from_secs(5);
    config.resolve_overrides = (1..=60)
        .map(|version| (format!("{APP}---v{version}.internal.fake-env.westeurope.azurecontainerapps.io"), fake.addr))
        .collect();
    config
}

struct Harness {
    fake: FakeAzure,
    tokens: Arc<TokenProvider>,
    arm: ArmClient,
    provider: AcaProvider,
}

async fn harness() -> Harness {
    let fake = FakeAzure::start().await;
    let tokens = Arc::new(TokenProvider::new(secret_credential(&fake, "right-secret")).unwrap());
    let arm = ArmClient::new(arm_config(&fake), tokens.clone()).unwrap();
    let provider = AcaProvider::new(arm.clone(), aca_config(&fake)).unwrap();
    Harness { fake, tokens, arm, provider }
}

fn deployment(version: u64) -> DeploymentRef {
    DeploymentRef::new(APP, format!("v{version}"), APP)
}

fn spec(version: u64) -> DeploymentSpec {
    let provider_settings = [
        ("location", serde_json::json!("westeurope")),
        (
            "environment_id",
            serde_json::json!("/subscriptions/sub/resourceGroups/rg/providers/Microsoft.App/managedEnvironments/env"),
        ),
        ("app_name", serde_json::json!(APP)),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value))
    .collect();
    DeploymentSpec {
        name: APP.into(),
        version: format!("v{version}"),
        image: Some(format!("ghcr.io/example/agent:{version}")),
        command: vec!["graphrt".into(), "serve".into(), "agent.yaml".into()],
        env: Default::default(),
        secrets: vec![SecretRef {
            env: "OPENAI_API_KEY".into(),
            reference: "https://kv1.vault.azure.net/secrets/openai-key".into(),
        }],
        max_active_runs: 8,
        min_replicas: 0,
        max_replicas: 10,
        grace_secs: 0,
        resources: Resources::default(),
        workflow_fingerprint: format!("sha256:{version}"),
        provider_settings,
    }
}

fn app_spec(version: u64) -> AcaAppSpec {
    AcaAppSpec::from_deployment_spec(&spec(version)).unwrap()
}

fn percentile(samples: &mut [Duration], fraction: f64) -> Duration {
    samples.sort();
    let index = ((samples.len() as f64 * fraction).ceil() as usize).saturating_sub(1);
    samples[index.min(samples.len() - 1)]
}

// C1: 20 concurrent wakes from zero produce one ARM lookup, one warm-up and one replica.
#[tokio::test]
async fn c01_concurrent_wakes_from_zero_are_single_flight() {
    let h = harness().await;
    h.fake.seed_app(APP, &[(3, "2026-11-01T00:00:00Z")]);
    h.fake.with(|state| state.cold_start = Duration::from_millis(100));
    let provider = Arc::new(h.provider);
    let wakes: Vec<_> = (0..20)
        .map(|_| {
            let provider = provider.clone();
            tokio::spawn(async move { provider.wake(&deployment(3), 1).await })
        })
        .collect();
    let mut urls = std::collections::HashSet::new();
    for wake in wakes {
        let endpoints = wake.await.unwrap().unwrap();
        let [endpoint] = endpoints.as_slice() else { panic!("expected one URL endpoint: {endpoints:?}") };
        let EndpointAddress::Http(url) = &endpoint.address else { panic!("expected an HTTP address") };
        assert_eq!(endpoint.replica.id, "url");
        urls.insert(url.clone());
    }
    assert_eq!(urls.len(), 1);
    assert!(urls.iter().all(|url| url.starts_with("http://bot---v3.internal.")));
    assert_eq!(h.fake.count("token"), 1, "one token fetch");
    assert_eq!(h.fake.count("arm GET app"), 1, "one ARM lookup");
    assert_eq!(h.fake.count("probe"), 1, "one warm-up request");
    assert_eq!(h.fake.count("scale from zero"), 1, "exactly one replica woken");
}

// C2: wake latency, n = 50 cold wakes. The fake's 20 ms cold start stands in
// for the platform; this measures the provider's own overhead. Real p95 needs Azure.
#[tokio::test]
async fn c02_wake_latency_p95_within_target() {
    let h = harness().await;
    let cold_start = Duration::from_millis(20);
    h.fake.with(|state| state.cold_start = cold_start);
    h.fake.seed_app(APP, &(1..=50).map(|version| (version, "2026-11-01T00:00:00Z")).collect::<Vec<_>>());
    let mut samples = Vec::new();
    for version in 1..=50 {
        let started = Instant::now();
        h.provider.wake(&deployment(version), 1).await.unwrap();
        samples.push(started.elapsed());
    }
    let p95 = percentile(&mut samples, 0.95);
    let target = Duration::from_millis(h.provider.capabilities().wake_p95_target_ms);
    eprintln!(
        "C2 fake wake p95 = {p95:?} (cold start {cold_start:?}, provider overhead ≈ {:?})",
        p95.saturating_sub(cold_start)
    );
    assert!(p95 <= target, "p95 {p95:?} > target {target:?}");
}

// C3: grace period. ACA sends SIGTERM and waits terminationGracePeriodSeconds;
// the agent drains within it (F0a). Here: the spec sets it and the gateway sees it.
#[tokio::test]
async fn c03_grace_is_configured_and_bounded() {
    let h = harness().await;
    assert_eq!(h.provider.capabilities().max_grace_secs, DEFAULT_GRACE_SECS);
    assert_eq!(app_spec(1).to_arm(Vec::new()).unwrap()["properties"]["template"]["terminationGracePeriodSeconds"], 120);
    let mut too_long = app_spec(1);
    too_long.grace_secs = MAX_GRACE_SECS + 1;
    assert!(
        matches!(too_long.validate(&KeyVaultConfig::default()), Err(error) if error.kind == DeployErrorKind::Invalid)
    );
}

// C4: Url addressing: release never touches ARM, so no active stream is cut.
#[tokio::test]
async fn c04_release_is_a_no_op() {
    let h = harness().await;
    h.fake.seed_app(APP, &[(1, "2026-11-01T00:00:00Z")]);
    let replica = ReplicaRef { deployment: deployment(1), id: "url".into() };
    for mode in [ReleaseMode::ScaleIn, ReleaseMode::Stop, ReleaseMode::Suspend] {
        h.provider.release(&replica, mode).await.unwrap();
    }
    let total: u64 = h.fake.with(|state| state.counts.values().sum());
    assert_eq!(total, 0, "release must not call Azure");
    let capabilities = h.provider.capabilities();
    assert_eq!((capabilities.addressing, capabilities.scaler), (Addressing::Url, Scaler::Platform));
}

// C5: a replica that dies outside the gateway disappears from `list`, and the
// next wake brings a replica back.
#[tokio::test]
async fn c05_replicas_killed_outside_the_gateway_are_seen_and_rewoken() {
    let h = harness().await;
    h.fake.seed_app(APP, &[(1, "2026-11-01T00:00:00Z")]);
    h.fake.with(|state| state.apps.get_mut(APP).unwrap().revisions[0].1.replicas = 2);
    assert_eq!(
        h.provider.list(&deployment(1)).await.unwrap().iter().filter(|r| r.phase == ReplicaPhase::Ready).count(),
        2
    );
    h.fake.with(|state| state.apps.get_mut(APP).unwrap().revisions[0].1.replicas = 0);
    assert!(h.provider.list(&deployment(1)).await.unwrap().is_empty());
    let fresh = AcaProvider::new(h.arm.clone(), aca_config(&h.fake)).unwrap();
    fresh.wake(&deployment(1), 1).await.unwrap();
    assert_eq!(h.fake.count("scale from zero"), 1);
    assert_eq!(fresh.list(&deployment(1)).await.unwrap().len(), 1);
}

// C7: the request cap the gateway preempts before (Standard: 240 s → preempt at 180 s).
#[test]
fn c07_request_cap_drives_a_planned_preempt() {
    let mut config = AcaConfig::new("sub", "rg");
    assert_eq!(planned_preempt_after(&config.capabilities()), Some(Duration::from_secs(180)));
    config.ingress = IngressMode::StandardIdleVerified;
    assert_eq!(planned_preempt_after(&config.capabilities()), None);
    config.ingress = IngressMode::Premium { request_idle_timeout_minutes: 30 };
    let premium = config.capabilities();
    assert_eq!(planned_preempt_after(&premium), None);
    assert!(heartbeat_keeps_stream_open(Duration::from_secs(15), &premium));
}

// C8a: a token is reused until `exp - 5 min`, then refreshed before it expires.
#[tokio::test]
async fn c08_token_refreshes_before_expiry() {
    let fake = FakeAzure::start().await;
    fake.with(|state| state.expires_in = 3600);
    let now = Arc::new(AtomicU64::new(1_000_000));
    let clock_now = now.clone();
    let tokens = TokenProvider::new(secret_credential(&fake, "right-secret"))
        .unwrap()
        .with_clock(Arc::new(move || clock_now.load(Ordering::SeqCst)));
    let first = tokens.token(ARM_SCOPE).await.unwrap();
    now.fetch_add(3600 - 301, Ordering::SeqCst);
    assert_eq!(tokens.token(ARM_SCOPE).await.unwrap(), first, "still > 5 min left");
    now.fetch_add(2, Ordering::SeqCst);
    assert_ne!(tokens.token(ARM_SCOPE).await.unwrap(), first, "refreshed 5 min before exp");
    assert_eq!(fake.count("token"), 2);
}

// C8b: local clock skewed ±5 min. `expires_in` is relative, so skew does not
// matter; with absolute `expires_on` (managed identity) a token the API
// already rejects is dropped on 401 and fetched again exactly once.
#[tokio::test]
async fn c08_clock_skew_and_401_refresh_once() {
    for skew in [-300i64, 300] {
        let fake = FakeAzure::start().await;
        let tokens = Arc::new(
            TokenProvider::new(AzureCredential::AppServiceIdentity {
                endpoint: format!("{}/msi/token", fake.base()),
                header: SecretString::new("identity-header-secret"),
                client_id: None,
            })
            .unwrap()
            .with_clock(Arc::new(move || {
                let real = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
                real.saturating_add_signed(skew)
            })),
        );
        let arm = ArmClient::new(arm_config(&fake), tokens.clone()).unwrap();
        fake.seed_app(APP, &[(1, "2026-11-01T00:00:00Z")]);
        arm.get(&AcaConfig::new("sub", "rg").app_path(APP, "")).await.unwrap();
        // The server revokes the token early (as Entra does after a key roll).
        fake.with(|state| state.valid_tokens.clear());
        arm.get(&AcaConfig::new("sub", "rg").app_path(APP, "")).await.unwrap();
        assert_eq!(fake.count("msi"), 2, "skew {skew}: one refresh after the 401");
        assert_eq!(fake.count("arm 401"), 1);
        // A token rejected twice is an auth error, not a refresh loop.
        fake.with(|state| state.arm_always_401 = true);
        let error = arm.get(&AcaConfig::new("sub", "rg").app_path(APP, "")).await;
        assert!(matches!(&error, Err(error) if error.kind == ProviderErrorKind::Auth), "{error:?}");
    }
}

// C8c: every credential kind against the fake endpoints.
#[tokio::test]
async fn c08_every_credential_kind_obtains_a_token() {
    let fake = FakeAzure::start().await;
    let directory = std::env::temp_dir().join(format!("graphrt-azure-wi-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let token_file = directory.join("token");
    std::fs::write(&token_file, "header.payload.signature\n").unwrap();
    let certificate = format!("{}{}", include_str!("fixtures/cert.pem"), include_str!("fixtures/key.pem"));
    fake.with(|state| state.imds_booting = 1);
    let credentials = [
        AzureCredential::Imds { endpoint: fake.base(), client_id: Some("user-assigned".into()) },
        AzureCredential::AppServiceIdentity {
            endpoint: format!("{}/msi/token", fake.base()),
            header: SecretString::new("identity-header-secret"),
            client_id: None,
        },
        secret_credential(&fake, "right-secret"),
        AzureCredential::ClientCertificate {
            authority_host: fake.base(),
            tenant_id: "tenant".into(),
            client_id: "client".into(),
            key: graphrt_provider_azure::auth::CertificateKey::from_pem(&certificate).unwrap(),
        },
        AzureCredential::WorkloadIdentity {
            authority_host: fake.base(),
            tenant_id: "tenant".into(),
            client_id: "client".into(),
            token_file: token_file.clone(),
        },
    ];
    for credential in credentials {
        let kind = credential.kind();
        let tokens = TokenProvider::new(credential).unwrap();
        assert!(tokens.token(ARM_SCOPE).await.is_ok(), "{kind}");
    }
    assert_eq!(fake.count("imds"), 2, "IMDS 410 while booting is retried");
    let form = fake.with(|state| state.last_form.clone());
    assert_eq!(form["client_assertion"], "header.payload.signature", "federated token file is the assertion");
    assert_eq!(form["scope"], ARM_SCOPE);
    std::fs::remove_dir_all(&directory).unwrap();
}

// C8d: no credential or token in Debug output, errors, or request URLs.
#[tokio::test]
async fn c08_no_credentials_in_logs() {
    let h = harness().await;
    h.fake.seed_app(APP, &[(1, "2026-11-01T00:00:00Z")]);
    h.provider.wake(&deployment(1), 0).await.unwrap();
    let wrong = TokenProvider::new(secret_credential(&h.fake, "wrong-secret-value")).unwrap();
    let rejected = wrong.token(ARM_SCOPE).await.unwrap_err();
    let token = h.tokens.token(ARM_SCOPE).await.unwrap();
    let mut rendered = vec![
        format!("{rejected}"),
        format!("{rejected:?}"),
        format!("{:?}", h.tokens),
        format!("{:?}", h.arm),
        format!("{:?}", h.provider),
        format!("{:?}", wrong.credential()),
        format!("{token:?}"),
        format!("{}", ProviderError::from(rejected.clone())),
    ];
    rendered.extend(
        h.fake
            .with(|state| state.log.clone())
            .into_iter()
            .filter(|line| !line.starts_with("POST /tenant/oauth2"))
            .map(|line| line.split(' ').take(2).collect::<Vec<_>>().join(" ")),
    );
    for line in rendered {
        for needle in ["right-secret", "wrong-secret-value", "fake-token-", "identity-header-secret"] {
            assert!(!line.contains(needle), "{needle:?} leaked in {line:?}");
        }
    }
    assert!(rejected.to_string().contains("invalid_client"));
    let _ = AuthEndpoints::default();
}

// C9: 429/5xx are retried with backoff; persistent throttling surfaces as RateLimited.
#[tokio::test]
async fn c09_throttling_and_server_errors_back_off() {
    let h = harness().await;
    h.fake.seed_app(APP, &[(1, "2026-11-01T00:00:00Z")]);
    h.fake.with(|state| state.arm_faults.extend([429, 503, 500]));
    assert_eq!(h.provider.revisions(APP).await.unwrap().len(), 1);
    assert_eq!(h.fake.count("arm fault"), 3);
    h.fake.with(|state| state.arm_faults.extend([429; 8]));
    let error = h.provider.revisions(APP).await.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::RateLimited, "{error:?}");
    assert_eq!(h.fake.count("arm fault"), 3 + 4, "max_attempts bounds the retries");
}

// C10: secrets only as Key Vault references; values go to Key Vault, never to ARM or errors.
#[tokio::test]
async fn c10_secrets_are_references_only() {
    let h = harness().await;
    let vault = KeyVaultClient::new(
        KeyVaultConfig {
            allowed_host_suffixes: vec!["127.0.0.1".into()],
            require_https: false,
            ..KeyVaultConfig::default()
        },
        h.tokens.clone(),
    )
    .unwrap();
    let url = vault.set_secret(&h.fake.base(), "openai-key", &SecretString::new("sk-live-value")).await.unwrap();
    assert_eq!(url, format!("{}/secrets/openai-key", h.fake.base()));
    assert_eq!(h.fake.with(|state| state.secrets["openai-key"].clone()), "sk-live-value");
    let strict = KeyVaultClient::new(KeyVaultConfig::default(), h.tokens.clone()).unwrap();
    let refused =
        strict.set_secret(&h.fake.base(), "openai-key", &SecretString::new("sk-live-value")).await.unwrap_err();
    assert!(!refused.to_string().contains("sk-live-value"));

    let driver = AcaDeployDriver::new(h.arm.clone(), aca_config(&h.fake), KeyVaultConfig::default()).unwrap();
    driver.apply(&spec(1)).await.unwrap();
    let put = h.fake.with(|state| state.log.iter().find(|line| line.starts_with("PUT ")).cloned().unwrap());
    assert!(put.contains("\"keyVaultUrl\":\"https://kv1.vault.azure.net/secrets/openai-key\""));
    assert!(!put.contains("sk-live-value"));
    let mut clear = spec(2);
    clear.env.insert("DB_PASSWORD".into(), "hunter2".into());
    let error = driver.apply(&clear).await.unwrap_err().to_string();
    assert!(error.contains("DB_PASSWORD") && !error.contains("hunter2"));
}

// C11: version N-1 stays wakeable for 7 days after N, then is retired (gateway → 410).
#[tokio::test]
async fn c11_old_version_pool_day_six_and_day_eight() {
    let h = harness().await;
    h.fake.seed_app(APP, &[(1, "2026-11-01T00:00:00Z"), (2, "2026-11-10T00:00:00Z")]);
    let superseded = parse_rfc3339("2026-11-10T00:00:00Z").unwrap();
    assert!(h.provider.retire_old_versions(APP, superseded + 6 * DAY).await.unwrap().is_empty());
    h.provider.wake(&deployment(1), 1).await.unwrap();

    let retired = h.provider.retire_old_versions(APP, superseded + 8 * DAY).await.unwrap();
    assert_eq!(retired, vec!["bot--v1".to_string()]);
    let error = h.provider.wake(&deployment(1), 1).await.unwrap_err();
    assert!(error.kind == ProviderErrorKind::NotFound, "retired version → NotFound (410 version_retired): {error:?}");
    h.provider.wake(&deployment(2), 1).await.unwrap();
}

// C12: scale to zero is the platform's (minReplicas 0); reconciliation
// deactivates stray revisions a crashed deploy left behind.
#[tokio::test]
async fn c12_scale_to_zero_and_orphan_cleanup() {
    let h = harness().await;
    let body = app_spec(1).to_arm(Vec::new()).unwrap();
    assert_eq!(body["properties"]["template"]["scale"]["minReplicas"], 0);
    h.fake.seed_app(APP, &[(1, "2026-11-01T00:00:00Z"), (2, "2026-11-02T00:00:00Z")]);
    h.fake.with(|state| {
        let app = state.apps.get_mut(APP).unwrap();
        app.revisions.push((
            "bot--hotfix".into(),
            common::Revision { active: true, created: "2026-11-03T00:00:00Z".into(), replicas: 1 },
        ));
    });
    let retired = h.provider.retire_old_versions(APP, parse_rfc3339("2026-11-04T00:00:00Z").unwrap()).await.unwrap();
    assert_eq!(retired, vec!["bot--hotfix".to_string()]);
    let active = h.provider.revisions(APP).await.unwrap().into_iter().filter(|r| r.active).count();
    assert_eq!(active, 2);
}

// Deploy driver: create, add a version (old label kept at 0 %), idempotent re-apply.
#[tokio::test]
async fn deploy_creates_labels_and_keeps_old_versions() {
    let h = harness().await;
    let driver = AcaDeployDriver::new(h.arm.clone(), aca_config(&h.fake), KeyVaultConfig::default()).unwrap();
    assert_eq!(driver.apply(&spec(1)).await.unwrap(), deployment(1));
    assert_eq!(driver.apply(&spec(2)).await.unwrap(), deployment(2));
    let traffic =
        h.fake.with(|state| state.apps[APP].body["properties"]["configuration"]["ingress"]["traffic"].clone());
    assert_eq!(
        traffic,
        serde_json::json!([
            {"revisionName": "bot--v2", "weight": 100, "label": "v2"},
            {"revisionName": "bot--v1", "weight": 0, "label": "v1"},
        ])
    );
    assert!(h.fake.count("operation poll") >= 4, "each PUT/PATCH waits for its operation");
    let puts = h.fake.count("arm PUT app");
    driver.apply(&spec(2)).await.unwrap();
    assert_eq!(h.fake.count("arm PUT app"), puts, "re-applying a version is a no-op");
    assert!(driver.apply(&spec(1)).await.is_ok(), "an existing version is a no-op too");
    h.provider.wake(&deployment(2), 1).await.unwrap();
}
