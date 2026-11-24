//! Direct deploys (`FlyDirect`) against the fake Machines API: app and IP
//! creation, secrets and the API key, the version Machine's config, promotion,
//! rollback, pause/resume, removal, failure paths and secret non-leakage.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use graphrt_provider_fly::direct::{
    AgentFile, DeployRequest, DeploymentState, DirectSettings, ErrorCode, FlyDirect, ServiceMode, Step, StepEvent,
    StepStatus,
};
use graphrt_provider_fly::{ClientPolicy, FlyToken, TokenOrigin};
use support::{FakeFly, TOKEN};

const APP: &str = "support-bot";
const OPENAI_VALUE: &str = "sk-direct-test-value-0123456789";
const API_KEY: &str = "grk_0123456789abcdef0123456789abcdef0123456789abcdef";

fn settings(fake: &FakeFly) -> DirectSettings {
    support::install_crypto_provider();
    let mut settings = DirectSettings::new(TokenOrigin::Fixed(FlyToken::parse(TOKEN).unwrap()));
    settings.api_base_url = fake.server.base_url();
    settings.client_policy = ClientPolicy { requests_per_second: 1000.0, burst: 1000, ..ClientPolicy::default() };
    settings.start_timeout = Duration::from_secs(5);
    settings.health_timeout = Duration::from_millis(300);
    settings.poll_interval = Duration::from_millis(20);
    settings
}

fn direct(fake: &FakeFly) -> FlyDirect {
    FlyDirect::new(settings(fake), APP.to_owned()).unwrap()
}

fn request(version: &str, now_ms: u64) -> DeployRequest {
    DeployRequest {
        deployment: "support-bot".into(),
        version: version.into(),
        region: "ams".into(),
        mode: ServiceMode::ScaleToZero,
        org_slug: "personal".into(),
        image: "ghcr.io/bimapangestu28/graphrt:0.5.0".into(),
        secrets: BTreeMap::from([("OPENAI_API_KEY".into(), OPENAI_VALUE.into())]),
        api_key_candidate: API_KEY.into(),
        rotate_api_key: false,
        agent: AgentFile { file_name: "support.yaml".into(), contents: format!("# {version}\n").into_bytes() },
        now_ms,
    }
}

async fn deploy(direct: &FlyDirect, request: &DeployRequest) -> (Vec<StepEvent>, Result<String, ErrorCode>) {
    let mut events = Vec::new();
    let result = direct.deploy(request, &mut |event| events.push(event)).await;
    (events, result.map(|outcome| outcome.version).map_err(|failure| failure.error.code))
}

#[tokio::test]
async fn first_deploy_creates_app_ips_secrets_and_a_healthy_current_machine() {
    let fake = FakeFly::start_empty().await;
    let direct = direct(&fake);
    let mut events = Vec::new();

    let outcome = direct.deploy(&request("v1", 1_000), &mut |event| events.push(event)).await.unwrap();

    assert_eq!(outcome.url, "https://support-bot.fly.dev");
    assert_eq!(outcome.workflow, "support");
    assert_eq!(outcome.api_key.as_deref(), Some(API_KEY), "the first deploy shows the key once");
    let steps: Vec<(Step, StepStatus)> = events.iter().map(|event| (event.step, event.status)).collect();
    use {Step::*, StepStatus::*};
    assert_eq!(
        steps,
        [
            (Preparing, Running),
            (Preparing, Done),
            (Secrets, Running),
            (Secrets, Done),
            (Starting, Running),
            (Starting, Done),
            (Checking, Running),
            (Checking, Done)
        ]
    );
    let state = fake.state();
    assert_eq!(state.apps.get(APP).map(String::as_str), Some("personal"));
    assert_eq!(state.ips.get(APP).map(Vec::len), Some(2), "a v6 and a shared v4");
    assert_eq!(state.secrets.get("OPENAI_API_KEY").map(String::as_str), Some(OPENAI_VALUE));
    assert_eq!(state.secrets.get("GRAPHRT_API_KEY").map(String::as_str), Some(API_KEY));
    let machine = &state.machines[0];
    assert_eq!(machine.state, "started");
    assert_eq!(machine.region, "ams");
    assert_eq!(machine.metadata("graphrt_role"), Some("current"));
    assert_eq!(machine.config["services"][0]["autostop"], "stop");
    assert_eq!(machine.config["files"][0]["guest_path"], "/workflows/support.yaml");
    assert_eq!(machine.config["image"], "ghcr.io/bimapangestu28/graphrt:0.5.0");
}

#[tokio::test]
async fn secret_values_and_the_key_only_travel_in_the_secrets_call() {
    let fake = FakeFly::start_empty().await;
    let direct = direct(&fake);

    deploy(&direct, &request("v1", 1_000)).await.1.unwrap();

    for recorded in fake.server.requests() {
        let body = recorded.body_text();
        let carries_secret = body.contains(OPENAI_VALUE) || body.contains(API_KEY);
        let is_secrets_call = recorded.method == "POST" && recorded.path.ends_with("/secrets");
        assert!(!carries_secret || is_secrets_call, "{} {} carries a secret", recorded.method, recorded.path);
        assert!(!recorded.path.contains(OPENAI_VALUE) && !recorded.path.contains(API_KEY));
    }
}

#[tokio::test]
async fn redeploy_is_idempotent_and_keeps_the_existing_api_key() {
    let fake = FakeFly::start_empty().await;
    let direct = direct(&fake);
    deploy(&direct, &request("v1", 1_000)).await.1.unwrap();

    let mut events = Vec::new();
    let again = direct.deploy(&request("v1", 2_000), &mut |event| events.push(event)).await.unwrap();

    assert_eq!(again.api_key, None, "the key is shown only when it is created");
    let state = fake.state();
    assert_eq!(state.machines.len(), 1, "same version, same Machine");
    assert_eq!(state.ips.get(APP).map(Vec::len), Some(2), "no new IPs");
    assert_eq!(state.count("POST app"), 1, "the app is created once");
}

#[tokio::test]
async fn rotate_api_key_replaces_and_returns_it() {
    let fake = FakeFly::start_empty().await;
    let direct = direct(&fake);
    deploy(&direct, &request("v1", 1_000)).await.1.unwrap();
    let mut rotated = request("v2", 2_000);
    rotated.rotate_api_key = true;
    rotated.api_key_candidate = format!("{API_KEY}rotated");

    let outcome = direct.deploy(&rotated, &mut |_| {}).await.unwrap();

    assert_eq!(outcome.api_key.as_deref(), Some(rotated.api_key_candidate.as_str()));
    assert_eq!(fake.state().secrets.get("GRAPHRT_API_KEY"), Some(&rotated.api_key_candidate));
}

#[tokio::test]
async fn new_version_demotes_the_old_one_to_a_stopped_standby_without_services() {
    let fake = FakeFly::start_empty().await;
    let direct = direct(&fake);
    deploy(&direct, &request("v1", 1_000)).await.1.unwrap();
    deploy(&direct, &request("v2", 2_000)).await.1.unwrap();

    let status = direct.status(3_000).await.unwrap();
    assert_eq!(status.status, DeploymentState::Online);
    assert_eq!(status.current_version.as_deref(), Some("v2"));
    assert_eq!(status.last_deployed_at.as_deref(), Some("1970-01-01T00:00:02.000Z"));
    let versions = direct.list_versions().await.unwrap();
    assert_eq!(versions.iter().map(|version| version.label.as_str()).collect::<Vec<_>>(), ["v2", "v1"]);
    let state = fake.state();
    let old = state.machines.iter().find(|machine| machine.metadata("graphrt_version") == Some("v1")).unwrap();
    assert_eq!(old.state, "stopped");
    assert_eq!(old.metadata("graphrt_role"), Some("standby"));
    assert!(old.config.get("services").is_none(), "the proxy must never route to or autostart an old version");
}

#[tokio::test]
async fn rollback_brings_a_standby_version_back_and_demotes_the_current_one() {
    let fake = FakeFly::start_empty().await;
    let direct = direct(&fake);
    deploy(&direct, &request("v1", 1_000)).await.1.unwrap();
    deploy(&direct, &request("v2", 2_000)).await.1.unwrap();
    let mut events = Vec::new();

    let serving = direct.rollback("v1", &mut |event| events.push(event)).await.unwrap();

    assert_eq!(serving.version, "v1");
    assert_eq!(events.last().map(|event| (event.step, event.status)), Some((Step::Checking, StepStatus::Done)));
    let status = direct.status(3_000).await.unwrap();
    assert_eq!(status.current_version.as_deref(), Some("v1"));
    {
        let state = fake.state();
        let v1 = state.machines.iter().find(|machine| machine.metadata("graphrt_version") == Some("v1")).unwrap();
        assert_eq!(v1.config["files"][0]["raw_value"], "IyB2MQo=", "v1 runs its own agent file again");
        assert_eq!(v1.config["services"][0]["autostart"], true);
        let v2 = state.machines.iter().find(|machine| machine.metadata("graphrt_version") == Some("v2")).unwrap();
        assert_eq!(v2.metadata("graphrt_role"), Some("standby"));
    }
    let unknown = direct.rollback("v9", &mut |_| {}).await.unwrap_err();
    assert_eq!(unknown.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn unhealthy_version_is_discarded_and_the_previous_one_keeps_serving() {
    let fake = FakeFly::start_empty().await;
    let direct = direct(&fake);
    deploy(&direct, &request("v1", 1_000)).await.1.unwrap();
    fake.state().failing_checks = true;

    let (events, result) = deploy(&direct, &request("v2", 2_000)).await;

    assert_eq!(result.unwrap_err(), ErrorCode::HealthCheckFailed);
    let failed = events.last().unwrap();
    assert_eq!((failed.step, failed.status), (Step::Checking, StepStatus::Failed));
    assert!(failed.message.contains("/readyz"), "{}", failed.message);
    fake.state().failing_checks = false;
    let status = direct.status(3_000).await.unwrap();
    assert_eq!(status.current_version.as_deref(), Some("v1"));
    assert_eq!(fake.state().machines.len(), 1, "the failed candidate was destroyed");
}

#[tokio::test]
async fn pause_takes_the_app_off_the_proxy_and_resume_restores_it() {
    let fake = FakeFly::start_empty().await;
    let direct = direct(&fake);
    let mut always_on = request("v1", 1_000);
    always_on.mode = ServiceMode::AlwaysOn;
    deploy(&direct, &always_on).await.1.unwrap();

    assert!(direct.pause().await.unwrap());
    assert!(!direct.pause().await.unwrap(), "pausing twice is a no-op");
    assert_eq!(direct.status(2_000).await.unwrap().status, DeploymentState::Paused);
    {
        let state = fake.state();
        assert_eq!(state.machines[0].state, "stopped");
        assert!(state.machines[0].config.get("services").is_none());
    }

    let resumed = direct.resume(&mut |_| {}).await.unwrap();

    assert_eq!(resumed.map(|serving| serving.version).as_deref(), Some("v1"));
    assert_eq!(direct.status(3_000).await.unwrap().status, DeploymentState::Online);
    let state = fake.state();
    assert_eq!(state.machines[0].state, "started");
    assert_eq!(state.machines[0].config["services"][0]["min_machines_running"], 1);
}

#[tokio::test]
async fn remove_deletes_the_app_and_status_then_reports_not_found() {
    let fake = FakeFly::start_empty().await;
    let direct = direct(&fake);
    deploy(&direct, &request("v1", 1_000)).await.1.unwrap();

    assert!(direct.remove().await.unwrap());
    assert!(!direct.remove().await.unwrap(), "removing twice is not an error");
    assert_eq!(direct.status(2_000).await.unwrap_err().code, ErrorCode::NotFound);
    assert!(fake.state().apps.is_empty());
}

#[tokio::test]
async fn app_name_owned_by_someone_else_is_a_distinct_error() {
    let fake = FakeFly::start_empty().await;
    fake.state().taken_app_names.push(APP.to_owned());

    let (events, result) = deploy(&direct(&fake), &request("v1", 1_000)).await;

    assert_eq!(result.unwrap_err(), ErrorCode::AppNameTaken);
    assert_eq!(events.last().map(|event| (event.step, event.status)), Some((Step::Preparing, StepStatus::Failed)));
}

#[tokio::test]
async fn bad_token_is_unauthorized_and_never_echoed() {
    let fake = FakeFly::start_empty().await;
    let mut settings = settings(&fake);
    settings.token_origin = TokenOrigin::Fixed(FlyToken::parse("fm2_wrong_token_value").unwrap());
    let direct = FlyDirect::new(settings, APP.to_owned()).unwrap();
    let mut events = Vec::new();

    let failure = direct.deploy(&request("v1", 1_000), &mut |event| events.push(event)).await.unwrap_err();

    assert_eq!(failure.error.code, ErrorCode::Unauthorized);
    let everything = format!("{failure} {failure:?} {events:?}");
    assert!(!everything.contains("wrong_token_value"), "{everything}");
}

#[tokio::test]
async fn failure_after_storing_a_new_key_still_hands_the_key_over() {
    let fake = FakeFly::start_empty().await;
    fake.state().failing_checks = true;

    let failure = direct(&fake).deploy(&request("v1", 1_000), &mut |_| {}).await.unwrap_err();

    assert_eq!(failure.error.code, ErrorCode::HealthCheckFailed);
    assert_eq!(failure.api_key.as_deref(), Some(API_KEY), "otherwise the stored key could never be learned");
    assert_eq!(direct(&fake).status(2_000).await.unwrap().status, DeploymentState::Offline);
}

#[tokio::test]
async fn keeps_only_the_configured_number_of_standby_versions() {
    let fake = FakeFly::start_empty().await;
    let mut settings = settings(&fake);
    settings.keep_versions = 2;
    let direct = FlyDirect::new(settings, APP.to_owned()).unwrap();
    for (index, version) in ["v1", "v2", "v3", "v4", "v5"].iter().enumerate() {
        deploy(&direct, &request(version, 1_000 * (index as u64 + 1))).await.1.unwrap();
    }

    let versions = direct.list_versions().await.unwrap();

    assert_eq!(versions.iter().map(|version| version.label.as_str()).collect::<Vec<_>>(), ["v5", "v4", "v3"]);
}
