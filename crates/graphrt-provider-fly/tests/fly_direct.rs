//! Direct deploys (`FlyDirect`) against the fake Machines API: app and IP
//! creation, secrets and the API key, the version Machine's config, promotion,
//! rollback, pause/resume, removal, failure paths and secret non-leakage.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use graphrt_provider_fly::direct::{
    AgentFile, ChatAccess, ChatOptions, DeployRequest, DeploymentState, DirectSettings, ErrorCode, FlyDirect,
    ServeOptions, ServiceMode, Step, StepEvent, StepStatus,
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
        image: "ghcr.io/graphrt/graphrt:0.5.0".into(),
        secrets: BTreeMap::from([("OPENAI_API_KEY".into(), OPENAI_VALUE.into())]),
        api_key_candidate: API_KEY.into(),
        rotate_api_key: false,
        agent: AgentFile { file_name: "support.yaml".into(), contents: format!("# {version}\n").into_bytes() },
        now_ms,
        serve: ServeOptions::default(),
        drop_store: false,
    }
}

/// A deploy with the monitoring page, which keeps devices and so needs the store.
fn monitored(version: &str, now_ms: u64) -> DeployRequest {
    let mut request = request(version, now_ms);
    request.serve = ServeOptions {
        monitor: true,
        push: Some("mailto:owner@example.com".into()),
        expire_approvals: true,
        ..ServeOptions::default()
    };
    request
}

fn command_of(machine: &support::FakeMachine) -> Vec<String> {
    let command = machine.config.pointer("/processes/0/cmd").or_else(|| machine.config.pointer("/init/cmd"));
    command.and_then(|cmd| cmd.as_array()).unwrap().iter().map(|part| part.as_str().unwrap().to_owned()).collect()
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
    assert_eq!(machine.config["image"], "ghcr.io/graphrt/graphrt:0.5.0");
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

#[tokio::test]
async fn a_deploy_without_stored_state_creates_no_volume() {
    let fake = FakeFly::start_empty().await;
    let direct = direct(&fake);
    let mut with_chat = request("v1", 1_000);
    with_chat.serve.chat = Some(ChatOptions { access: ChatAccess::Guests, ..ChatOptions::default() });
    with_chat.serve.cors_origins = vec!["https://www.example.com".into()];
    with_chat.serve.expire_approvals = true;

    let outcome = direct.deploy(&with_chat, &mut |_| {}).await.unwrap();

    assert!(outcome.store.is_none());
    assert_eq!(outcome.serve.chat, Some(ChatAccess::Guests));
    let state = fake.state();
    assert!(state.volumes.is_empty(), "a chat page keeps nothing on the server: no volume, no extra cost");
    assert_eq!(state.count("POST volumes"), 0);
    let machine = &state.machines[0];
    assert!(machine.config.get("mounts").is_none());
    assert_eq!(
        command_of(machine)[6..],
        [
            "--chat",
            "support",
            "--chat-access",
            "guests",
            "--cors-origin",
            "https://www.example.com",
            "--expire-approvals"
        ]
    );
}

#[tokio::test]
async fn monitoring_gets_one_volume_as_the_store_in_the_machines_region() {
    let fake = FakeFly::start_empty().await;
    let direct = direct(&fake);

    let outcome = direct.deploy(&monitored("v1", 1_000), &mut |_| {}).await.unwrap();

    let store = outcome.store.unwrap();
    assert!(store.in_use);
    assert_eq!((store.size_gb, store.region.as_str()), (1, "ams"));
    {
        let state = fake.state();
        let volume = state.volume_named("graphrt_store").unwrap();
        assert_eq!(volume.id, store.volume);
        let machine = &state.machines[0];
        assert_eq!(volume.attached_machine_id.as_deref(), Some(machine.id.as_str()));
        assert_eq!(machine.name, "graphrt-store");
        assert_eq!(machine.config["mounts"][0]["path"], "/data");
        assert_eq!(machine.config["processes"][0]["user"], "root");
        assert_eq!(
            command_of(machine)[6..],
            [
                "--store",
                "/data",
                "--store-ttl-days",
                "30",
                "--monitor",
                "--expire-approvals",
                "--push",
                "mailto:owner@example.com",
                "--public-url",
                "https://support-bot.fly.dev"
            ]
        );
    }
    let status = direct.status(2_000).await.unwrap();
    assert_eq!(status.store.unwrap().volume, store.volume);
}

#[tokio::test]
async fn redeploys_and_stops_keep_the_same_volume_on_the_same_machine() {
    let fake = FakeFly::start_empty().await;
    let direct = direct(&fake);
    let first = direct.deploy(&monitored("v1", 1_000), &mut |_| {}).await.unwrap();
    let (machine_id, volume_id) = {
        let state = fake.state();
        (state.machines[0].id.clone(), state.volumes[0].id.clone())
    };

    // Scale to zero: the Fly proxy stops the Machine when idle and starts it again.
    fake.state().machines[0].state = "stopped".to_owned();
    let second = direct.deploy(&monitored("v2", 2_000), &mut |_| {}).await.unwrap();
    let third = direct.deploy(&monitored("v3", 3_000), &mut |_| {}).await.unwrap();

    assert_eq!(first.store, second.store);
    assert_eq!(second.store, third.store);
    {
        let state = fake.state();
        assert_eq!(state.volumes.len(), 1, "one volume for the life of the deployment");
        assert_eq!(state.count("POST volumes"), 1);
        assert_eq!(state.machines.len(), 1, "the Machine with the volume is updated in place");
        let machine = &state.machines[0];
        assert_eq!(machine.id, machine_id);
        assert_eq!(machine.state, "started");
        assert_eq!(machine.metadata("graphrt_version"), Some("v3"));
        assert_eq!(machine.metadata("graphrt_role"), Some("current"));
        assert_eq!(machine.config["mounts"][0]["volume"], volume_id.as_str());
        assert_eq!(machine.config["files"][0]["raw_value"], "IyB2Mwo=", "it serves the newest agent file");
        assert_eq!(state.volumes[0].attached_machine_id.as_deref(), Some(machine_id.as_str()));
    }
    let versions = direct.list_versions().await.unwrap();
    assert_eq!(versions.iter().map(|version| version.label.as_str()).collect::<Vec<_>>(), ["v3"]);
}

#[tokio::test]
async fn pause_and_resume_keep_the_volume() {
    let fake = FakeFly::start_empty().await;
    let direct = direct(&fake);
    direct.deploy(&monitored("v1", 1_000), &mut |_| {}).await.unwrap();

    assert!(direct.pause().await.unwrap());
    assert!(fake.state().machines[0].config["mounts"][0]["volume"].is_string());
    direct.resume(&mut |_| {}).await.unwrap().unwrap();

    let state = fake.state();
    assert_eq!(state.machines[0].state, "started");
    assert_eq!(state.volumes[0].attached_machine_id.as_deref(), Some(state.machines[0].id.as_str()));
}

#[tokio::test]
async fn an_unhealthy_version_with_a_store_puts_the_previous_one_back() {
    let fake = FakeFly::start_empty().await;
    let direct = direct(&fake);
    direct.deploy(&monitored("v1", 1_000), &mut |_| {}).await.unwrap();
    fake.state().failing_checks = true;

    let (events, result) = deploy(&direct, &monitored("v2", 2_000)).await;

    assert_eq!(result.unwrap_err(), ErrorCode::HealthCheckFailed);
    let failed = events.last().unwrap();
    assert!(failed.message.contains("the version that served before was started again"), "{}", failed.message);
    fake.state().failing_checks = false;
    let state = fake.state();
    assert_eq!(state.machines.len(), 1);
    let machine = &state.machines[0];
    assert_eq!(machine.metadata("graphrt_version"), Some("v1"));
    assert_eq!(machine.metadata("graphrt_role"), Some("current"));
    assert_eq!(machine.config["files"][0]["raw_value"], "IyB2MQo=", "v1's own agent file serves again");
    assert!(matches!(machine.state.as_str(), "started" | "starting"));
    assert_eq!(state.volumes.len(), 1, "the store is untouched");
}

#[tokio::test]
async fn turning_monitoring_on_later_moves_to_a_machine_with_the_volume() {
    let fake = FakeFly::start_empty().await;
    let direct = direct(&fake);
    deploy(&direct, &request("v1", 1_000)).await.1.unwrap();

    let outcome = direct.deploy(&monitored("v2", 2_000), &mut |_| {}).await.unwrap();

    assert!(outcome.store.unwrap().in_use);
    let state = fake.state();
    assert_eq!(state.machines.len(), 2, "a Machine cannot get a volume later: a new one is created");
    let old = state.machines.iter().find(|machine| machine.metadata("graphrt_version") == Some("v1")).unwrap();
    assert_eq!(old.metadata("graphrt_role"), Some("standby"));
    let new = state.machine_named("graphrt-store").unwrap();
    assert_eq!(new.metadata("graphrt_role"), Some("current"));
}

#[tokio::test]
async fn turning_monitoring_off_keeps_the_data_until_the_store_is_dropped() {
    let fake = FakeFly::start_empty().await;
    let direct = direct(&fake);
    direct.deploy(&monitored("v1", 1_000), &mut |_| {}).await.unwrap();

    let mut events = Vec::new();
    let kept = direct.deploy(&request("v2", 2_000), &mut |event| events.push(event)).await.unwrap();

    let store = kept.store.unwrap();
    assert!(!store.in_use, "the serving version no longer uses it");
    assert!(events.last().unwrap().message.contains("--drop-store"), "{}", events.last().unwrap().message);
    {
        let state = fake.state();
        assert_eq!(state.volumes.len(), 1, "data is never deleted without being asked");
        assert_eq!(state.machine_named("graphrt-store").unwrap().metadata("graphrt_role"), Some("standby"));
    }
    assert_eq!(direct.status(2_500).await.unwrap().store.map(|store| store.in_use), Some(false));

    // Turning it on again finds the devices that were paired before.
    let again = direct.deploy(&monitored("v3", 3_000), &mut |_| {}).await.unwrap();
    assert_eq!(again.store.as_ref().map(|store| store.volume.as_str()), Some(store.volume.as_str()));
    assert_eq!(fake.state().count("POST volumes"), 1);

    let mut dropping = request("v4", 4_000);
    dropping.drop_store = true;
    let mut events = Vec::new();
    let dropped = direct.deploy(&dropping, &mut |event| events.push(event)).await.unwrap();

    assert!(dropped.store.is_none());
    assert!(events.last().unwrap().message.contains("the store volume was deleted"));
    let state = fake.state();
    assert!(state.volumes.is_empty());
    assert!(state.machine_named("graphrt-store").is_none(), "the Machine that held the volume is gone too");
    assert_eq!(state.count("DELETE volumes"), 1);
}

#[tokio::test]
async fn dropping_a_store_that_does_not_exist_is_not_an_error() {
    let fake = FakeFly::start_empty().await;
    let direct = direct(&fake);
    let mut dropping = request("v1", 1_000);
    dropping.drop_store = true;

    let outcome = direct.deploy(&dropping, &mut |_| {}).await.unwrap();

    assert!(outcome.store.is_none());
    assert_eq!(fake.state().count("DELETE volumes"), 0);
}

#[tokio::test]
async fn a_store_needs_a_region_and_cannot_be_dropped_by_the_deploy_that_needs_it() {
    let fake = FakeFly::start_empty().await;
    let direct = direct(&fake);
    let mut no_region = monitored("v1", 1_000);
    no_region.region = String::new();

    let failure = direct.deploy(&no_region, &mut |_| {}).await.unwrap_err();

    assert_eq!(failure.error.code, ErrorCode::InvalidInput);
    assert!(failure.error.message.contains("--region"), "{}", failure.error.message);
    assert!(fake.state().volumes.is_empty() && fake.state().machines.is_empty());

    let mut contradicting = monitored("v1", 1_000);
    contradicting.drop_store = true;
    let failure = direct.deploy(&contradicting, &mut |_| {}).await.unwrap_err();
    assert_eq!(failure.error.code, ErrorCode::InvalidInput);
}

#[tokio::test]
async fn removing_the_deployment_removes_the_volume_with_the_app() {
    let fake = FakeFly::start_empty().await;
    let direct = direct(&fake);
    direct.deploy(&monitored("v1", 1_000), &mut |_| {}).await.unwrap();

    assert!(direct.remove().await.unwrap());

    assert!(fake.state().volumes.is_empty(), "deleting a Fly app deletes its volumes");
}

/// A bundle too large for the Machine config: random-ish bytes of `size`.
fn large_agent(version: &str, size: usize, seed: u8) -> DeployRequest {
    let mut request = request(version, 10_000);
    let mut state = u32::from(seed).wrapping_mul(2_654_435_761).wrapping_add(1);
    let contents = (0..size)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        })
        .collect();
    request.agent = AgentFile { file_name: "support.grt".into(), contents };
    request
}

fn exec_steps(fake: &FakeFly) -> Vec<String> {
    fake.state().exec_commands.iter().map(|command| command.get(3).cloned().unwrap_or_default()).collect()
}

#[tokio::test]
async fn an_agent_larger_than_a_machine_config_is_uploaded_onto_the_store_volume() {
    let fake = FakeFly::start_empty().await;
    let direct = direct(&fake);
    let first = large_agent("v1", 5 * 1024 * 1024 + 123, 1);
    let mut events = Vec::new();
    let outcome = direct.deploy(&first, &mut |event| events.push(event)).await.unwrap();
    assert_eq!(outcome.version, "v1");

    let state = fake.state();
    let volume = state.volume_named("graphrt_store").expect("a store volume").clone();
    assert_eq!(volume.size_gb, 2, "room for the agent and the next one");
    let machine = state.machine_named("graphrt-store").expect("the store Machine").clone();
    assert_eq!(machine.metadata("graphrt_role"), Some("current"));
    assert!(machine.config.get("files").is_none(), "the agent is not in the Machine config");
    assert_eq!(machine.config.pointer("/guest/memory_mb").and_then(|value| value.as_u64()), Some(512));
    let command = command_of(&machine);
    assert!(command[1].starts_with("/data/agents/"), "{command:?}");
    assert!(!command.contains(&"--store".to_owned()), "the volume only holds the agent: {command:?}");
    let files = state.volume_files.get(&volume.id).expect("files on the volume");
    let dir = command[1].trim_start_matches("/data/agents/");
    assert_eq!(files.get(&format!("{dir}/support.grt")), Some(&first.agent.contents), "byte for byte");
    let steps: Vec<String> = state.exec_commands.iter().map(|command| command[3].clone()).collect();
    // hash, begin, 3 pieces of 2 MiB, commit, then prune after promotion.
    assert_eq!(steps, ["hash", "begin", "append", "append", "append", "commit", "prune"]);
    drop(state);
    let uploading = events.iter().filter(|event| event.message.starts_with("Uploading the agent")).count();
    assert_eq!(uploading, 3, "one progress line per piece");
    let started = events.iter().find(|event| event.step == Step::Starting && event.status == StepStatus::Done).unwrap();
    assert!(started.message.contains("uploaded onto the store volume"), "{}", started.message);

    // The same agent again (a new version label): nothing is uploaded.
    fake.state().exec_commands.clear();
    let again = large_agent("v2", 5 * 1024 * 1024 + 123, 1);
    direct.deploy(&again, &mut |_| {}).await.unwrap();
    assert_eq!(exec_steps(&fake), ["hash", "prune"]);

    // A changed agent goes into a new directory; the old one is pruned after promotion.
    fake.state().exec_commands.clear();
    let changed = large_agent("v3", 3 * 1024 * 1024, 2);
    direct.deploy(&changed, &mut |_| {}).await.unwrap();
    let state = fake.state();
    let files = state.volume_files.get(&volume.id).unwrap();
    assert_eq!(files.len(), 1, "only the serving agent stays: {:?}", files.keys().collect::<Vec<_>>());
    assert_eq!(files.values().next(), Some(&changed.agent.contents));
    let machine = state.machine_named("graphrt-store").unwrap();
    assert!(command_of(machine)[1].ends_with(files.keys().next().unwrap().split('/').next().unwrap()));
}

#[tokio::test]
async fn a_large_agent_grows_the_store_volume_and_an_unhealthy_one_puts_the_previous_version_back() {
    let fake = FakeFly::start_empty().await;
    let direct = direct(&fake);
    direct.deploy(&monitored("v1", 1_000), &mut |_| {}).await.unwrap();
    assert_eq!(fake.state().volume_named("graphrt_store").unwrap().size_gb, 1);
    let old_command = command_of(fake.state().machine_named("graphrt-store").unwrap());
    assert_eq!(old_command[1], "/workflows");

    // A large agent on the same store Machine, whose version never gets healthy.
    fake.state().failing_checks = true;
    let mut big = large_agent("v2", 2 * 1024 * 1024, 4);
    big.serve = monitored("v2", 2_000).serve;
    let (_, result) = deploy(&direct, &big).await;
    assert_eq!(result, Err(ErrorCode::HealthCheckFailed));
    let state = fake.state();
    let volume = state.volume_named("graphrt_store").unwrap().clone();
    assert_eq!(volume.size_gb, 2, "grown for the agent");
    assert_eq!(state.count("PUT extend"), 1);
    let machine = state.machine_named("graphrt-store").unwrap();
    assert_eq!(command_of(machine), old_command, "the previous version serves again");
    assert!(state.volume_files[&volume.id].values().any(|file| file == &big.agent.contents), "uploaded intact");
}

#[tokio::test]
async fn a_large_agent_needs_a_region_and_keeps_its_volume() {
    let fake = FakeFly::start_empty().await;
    let direct = direct(&fake);
    let mut no_region = large_agent("v1", 2 * 1024 * 1024, 5);
    no_region.region = String::new();
    let (_, result) = deploy(&direct, &no_region).await;
    assert_eq!(result, Err(ErrorCode::InvalidInput));
    let mut dropping = large_agent("v1", 2 * 1024 * 1024, 5);
    dropping.drop_store = true;
    let (_, result) = deploy(&direct, &dropping).await;
    assert_eq!(result, Err(ErrorCode::InvalidInput));
    assert!(fake.state().volumes.is_empty());
}
