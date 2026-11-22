//! Fly-specific behaviour on top of C1–C13: pool order, suspend → stop
//! fallback, capacity fallback, pool growth, version isolation, and the deploy
//! driver (secrets, idempotent pool, kill_timeout).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, clippy::print_stdout)]

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use graphrt_provider_api::{
    DeployDriver, DeploymentRef, DeploymentSpec, EndpointAddress, Provider, ProviderErrorKind, ReleaseMode,
    ReplicaPhase, ReplicaRef, Resources, SecretRef,
};
use graphrt_provider_fly::{
    ClientPolicy, FlyDeployConfig, FlyDeployDriver, FlyProvider, FlyProviderConfig, FlyToken, TokenOrigin,
};
use support::{APP, FakeFly, TOKEN};

fn token_origin() -> TokenOrigin {
    TokenOrigin::Fixed(FlyToken::parse(TOKEN).unwrap())
}

fn provider_for(fake: &FakeFly, max_machines: u32) -> FlyProvider {
    support::install_crypto_provider();
    let mut config = FlyProviderConfig::single_app(APP, token_origin());
    config.api_base_url = fake.server.base_url();
    config.max_machines_per_version = max_machines;
    config.start_timeout = Duration::from_secs(5);
    FlyProvider::new(config).unwrap()
}

fn chat_v1() -> DeploymentRef {
    DeploymentRef::new("chat", "v1", APP)
}

#[tokio::test]
async fn wake_resumes_a_suspended_machine_before_cold_booting_a_stopped_one() {
    let fake = FakeFly::start().await;
    let _stopped = fake.seed_machine("chat", "v1", 0, "stopped", 512);
    let suspended = fake.seed_machine("chat", "v1", 1, "suspended", 512);
    let provider = provider_for(&fake, 2);

    let endpoints = provider.wake(&chat_v1(), 1).await.unwrap();

    assert_eq!(endpoints.len(), 1);
    assert_eq!(endpoints[0].replica.id, suspended);
    let EndpointAddress::Http(address) = &endpoints[0].address else { panic!("6PN address expected") };
    assert!(address.starts_with("http://[fdaa:") && address.ends_with("]:8080"), "{address}");
    assert_eq!(fake.state().count("POST start"), 1);
}

#[tokio::test]
async fn warm_wake_makes_one_list_call_and_no_mutation() {
    let fake = FakeFly::start().await;
    fake.seed_machine("chat", "v1", 0, "started", 512);
    let provider = provider_for(&fake, 2);

    provider.wake(&chat_v1(), 1).await.unwrap();

    assert_eq!(fake.server.requests().len(), 1);
    assert_eq!(fake.state().count("POST start"), 0);
}

#[tokio::test]
async fn release_suspends_and_falls_back_to_stop_when_suspend_is_refused() {
    let fake = FakeFly::start().await;
    let small = fake.seed_machine("chat", "v1", 0, "started", 512);
    let large = fake.seed_machine("chat", "v1", 1, "started", 4096);
    let provider = provider_for(&fake, 2);
    let replica = |id: &str| ReplicaRef { deployment: chat_v1(), id: id.to_owned() };

    provider.release(&replica(&small), ReleaseMode::Suspend).await.unwrap();
    provider.release(&replica(&large), ReleaseMode::Suspend).await.unwrap();

    assert_eq!(fake.state().machine(&small).unwrap().state, "suspended");
    assert_eq!(fake.state().machine(&large).unwrap().state, "stopped");
    let suspends_after_first_release = fake.state().count("POST suspend");
    assert_eq!(suspends_after_first_release, 2);

    // A Machine that refused suspend goes straight to stop next time.
    provider.wake(&chat_v1(), 2).await.unwrap();
    provider.release(&replica(&large), ReleaseMode::ScaleIn).await.unwrap();
    assert_eq!(fake.state().count("POST suspend"), suspends_after_first_release);
    assert_eq!(fake.state().machine(&large).unwrap().state, "stopped");
}

#[tokio::test]
async fn stop_mode_sends_sigterm_with_the_kill_timeout() {
    let fake = FakeFly::start().await;
    let machine = fake.seed_machine("chat", "v1", 0, "started", 512);
    let provider = provider_for(&fake, 1);

    provider.release(&ReplicaRef { deployment: chat_v1(), id: machine.clone() }, ReleaseMode::Stop).await.unwrap();

    let stop_request = fake.server.requests().into_iter().find(|request| request.path.ends_with("/stop")).unwrap();
    let body: serde_json::Value = serde_json::from_slice(&stop_request.body).unwrap();
    assert_eq!(body, serde_json::json!({"signal": "SIGTERM", "timeout": "300s"}));
}

#[tokio::test]
async fn releasing_a_gone_machine_is_ok() {
    let fake = FakeFly::start().await;
    let provider = provider_for(&fake, 1);
    let missing = ReplicaRef { deployment: chat_v1(), id: "deadbeef".to_owned() };
    provider.release(&missing, ReleaseMode::Suspend).await.unwrap();
}

#[tokio::test]
async fn capacity_on_one_host_moves_on_to_the_next_machine() {
    let fake = FakeFly::start().await;
    let full = fake.seed_machine("chat", "v1", 0, "suspended", 512);
    let free = fake.seed_machine("chat", "v1", 1, "stopped", 512);
    fake.state().full_hosts.push(full.clone());
    let provider = provider_for(&fake, 2);

    let endpoints = provider.wake(&chat_v1(), 1).await.unwrap();

    assert_eq!(endpoints[0].replica.id, free);
}

#[tokio::test]
async fn exhausted_pool_grows_with_a_clone_of_a_pool_member() {
    let fake = FakeFly::start().await;
    let full = fake.seed_machine("chat", "v1", 0, "stopped", 512);
    fake.state().full_hosts.push(full);
    let provider = provider_for(&fake, 2);

    let endpoints = provider.wake(&chat_v1(), 1).await.unwrap();

    let state = fake.state();
    let clone = state.machine(&endpoints[0].replica.id).unwrap();
    assert_eq!(clone.name, "chat-v1-1");
    assert_eq!(clone.config["metadata"]["graphrt_version"], "v1");
    assert_eq!(clone.config["stop_config"]["timeout"], "300s");
}

#[tokio::test]
async fn full_pool_without_room_to_grow_is_a_capacity_error() {
    let fake = FakeFly::start().await;
    let full = fake.seed_machine("chat", "v1", 0, "stopped", 512);
    fake.state().full_hosts.push(full);
    let provider = provider_for(&fake, 1);

    let error = provider.wake(&chat_v1(), 1).await.unwrap_err();

    assert_eq!(error.kind, ProviderErrorKind::Capacity);
}

#[tokio::test]
async fn versions_are_isolated_pools() {
    let fake = FakeFly::start().await;
    fake.seed_machine("chat", "v1", 0, "stopped", 512);
    let old_version_machine = fake.seed_machine("chat", "v0", 0, "suspended", 512);
    let provider = provider_for(&fake, 2);

    provider.wake(&chat_v1(), 1).await.unwrap();
    let old_pool = provider.list(&DeploymentRef::new("chat", "v0", APP)).await.unwrap();

    assert_eq!(old_pool.len(), 1);
    assert_eq!(old_pool[0].replica.id, old_version_machine);
    assert_eq!(old_pool[0].phase, ReplicaPhase::Suspended);
    // The N-1 pool stays wakeable (decision 6: 7 days).
    let resumed = provider.wake(&DeploymentRef::new("chat", "v0", APP), 1).await.unwrap();
    assert_eq!(resumed[0].replica.id, old_version_machine);
}

#[tokio::test]
async fn unconfigured_apps_are_refused_without_a_call() {
    let fake = FakeFly::start().await;
    let provider = provider_for(&fake, 1);

    let error = provider.wake(&DeploymentRef::new("chat", "v1", "someone-elses-app"), 1).await.unwrap_err();

    assert_eq!(error.kind, ProviderErrorKind::Fatal);
    assert!(fake.server.requests().is_empty());
}

#[tokio::test]
async fn auth_errors_never_carry_the_token() {
    let fake = FakeFly::start().await;
    support::install_crypto_provider();
    let mut config =
        FlyProviderConfig::single_app(APP, TokenOrigin::Fixed(FlyToken::parse("fm2_wrong_token_value").unwrap()));
    config.api_base_url = fake.server.base_url();
    let provider = FlyProvider::new(config).unwrap();

    let error = provider.list(&chat_v1()).await.unwrap_err();

    assert_eq!(error.kind, ProviderErrorKind::Auth);
    assert!(!error.to_string().contains("wrong_token_value"));
    // One try plus one reload after the 401.
    assert_eq!(fake.server.requests().len(), 2);
}

fn deploy_driver(fake: &FakeFly, environment: BTreeMap<&'static str, &'static str>) -> FlyDeployDriver {
    support::install_crypto_provider();
    FlyDeployDriver::new(FlyDeployConfig {
        api_base_url: fake.server.base_url(),
        token_origin: token_origin(),
        client_policy: ClientPolicy::default(),
        env_lookup: Arc::new(move |name| environment.get(name).map(|value| (*value).to_owned())),
    })
}

fn spec(app: &str) -> DeploymentSpec {
    DeploymentSpec {
        name: "chat".into(),
        version: "v2".into(),
        image: Some("ghcr.io/example/agent:2".into()),
        command: vec![
            "serve".into(),
            "/app/workflows".into(),
            "--addr".into(),
            "[::]:8080".into(),
            "--trust-gateway".into(),
        ],
        env: BTreeMap::from([("RUST_LOG".into(), "info".into())]),
        secrets: vec![SecretRef { env: "OPENAI_API_KEY".into(), reference: "env:LOCAL_OPENAI_KEY".into() }],
        max_active_runs: 8,
        min_replicas: 0,
        max_replicas: 2,
        grace_secs: 120,
        resources: Resources { cpu_millis: 1000, memory_mib: 512 },
        workflow_fingerprint: "sha256:abc".into(),
        provider_settings: BTreeMap::from([
            ("app".into(), serde_json::json!(app)),
            ("org".into(), serde_json::json!("acme")),
            ("region".into(), serde_json::json!("ams")),
        ]),
    }
}

#[tokio::test]
async fn deploy_creates_app_secrets_and_a_stopped_pool_idempotently() {
    let fake = FakeFly::start().await;
    let driver = deploy_driver(&fake, BTreeMap::from([("LOCAL_OPENAI_KEY", "sk-live-secret-value")]));

    let deployment = driver.apply(&spec("chat-new")).await.unwrap();
    driver.apply(&spec("chat-new")).await.unwrap();

    assert_eq!(deployment, DeploymentRef::new("chat", "v2", "chat-new"));
    let state = fake.state();
    assert_eq!(state.apps.get("chat-new").map(String::as_str), Some("acme"));
    assert_eq!(state.secrets.get("OPENAI_API_KEY").map(String::as_str), Some("sk-live-secret-value"));
    let pool: Vec<_> =
        state.machines.iter().filter(|machine| machine.config["metadata"]["graphrt_version"] == "v2").collect();
    assert_eq!(pool.len(), 2, "a retried deploy must not double the pool");
    for machine in &pool {
        assert_eq!(machine.state, "created");
        assert_eq!(machine.config["stop_config"], serde_json::json!({"signal": "SIGTERM", "timeout": "120s"}));
        assert_eq!(machine.config["restart"]["policy"], "no");
        assert!(machine.config.get("services").is_none(), "no public services: 6PN only");
        assert_eq!(machine.config["init"]["cmd"][4], "--trust-gateway");
    }
    drop(state);
    // The secret value only ever goes to the secrets endpoint.
    for request in fake.server.requests() {
        if request.body_text().contains("sk-live-secret-value") {
            assert!(request.path.ends_with("/secrets"), "secret value sent to {}", request.path);
        }
    }
}

#[tokio::test]
async fn deploy_requires_existing_fly_secret_references() {
    let fake = FakeFly::start().await;
    let driver = deploy_driver(&fake, BTreeMap::new());
    let mut referenced = spec(APP);
    referenced.secrets = vec![SecretRef { env: "SUPABASE_KEY".into(), reference: "fly:SUPABASE_KEY".into() }];

    let error = driver.apply(&referenced).await.unwrap_err();
    assert!(error.message.contains("SUPABASE_KEY"), "{error}");

    fake.state().secrets.insert("SUPABASE_KEY".into(), "set-with-flyctl".into());
    driver.apply(&referenced).await.unwrap();
}

#[tokio::test]
async fn deployed_pool_is_wakeable_by_the_provider() {
    let fake = FakeFly::start().await;
    let driver = deploy_driver(&fake, BTreeMap::from([("LOCAL_OPENAI_KEY", "sk-x")]));
    let deployment = driver.apply(&spec(APP)).await.unwrap();
    let provider = provider_for(&fake, 2);

    let endpoints = provider.wake(&deployment, 2).await.unwrap();
    assert_eq!(endpoints.len(), 2);

    let destroyed = driver.retire_version(&deployment).await.unwrap();
    assert_eq!(destroyed, 2);
    assert!(provider.list(&deployment).await.unwrap().is_empty());
}
