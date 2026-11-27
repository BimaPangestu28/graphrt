//! Direct deploys against behaviour seen on real Fly that the first fake did
//! not have: an organization at its Machine limit (Fly's Free plan allows 3
//! Machines), where the oldest standby versions make room for a new version
//! and the serving version and the store are never removed; a rollback whose
//! Machine cannot start, which leaves that version a standby; and a Machine
//! that shows its previous config, `replacing`, for a moment after an update.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use graphrt_provider_fly::direct::{
    AgentFile, DeployRequest, DeploymentState, DirectSettings, ErrorCode, FlyDirect, ServeOptions, ServiceMode, Step,
    StepEvent, StepStatus,
};
use graphrt_provider_fly::{ClientPolicy, FlyToken, TokenOrigin};
use support::{FakeFly, TOKEN};

const APP: &str = "support-bot";
const API_KEY: &str = "grk_0123456789abcdef0123456789abcdef0123456789abcdef";

fn direct(fake: &FakeFly) -> FlyDirect {
    support::install_crypto_provider();
    let mut settings = DirectSettings::new(TokenOrigin::Fixed(FlyToken::parse(TOKEN).unwrap()));
    settings.api_base_url = fake.server.base_url();
    settings.client_policy = ClientPolicy { requests_per_second: 1000.0, burst: 1000, ..ClientPolicy::default() };
    settings.start_timeout = Duration::from_secs(5);
    settings.health_timeout = Duration::from_millis(300);
    settings.poll_interval = Duration::from_millis(20);
    FlyDirect::new(settings, APP.to_owned()).unwrap()
}

fn request(version: &str, now_ms: u64) -> DeployRequest {
    DeployRequest {
        deployment: "support-bot".into(),
        version: version.into(),
        region: "ams".into(),
        mode: ServiceMode::ScaleToZero,
        org_slug: "personal".into(),
        image: "ghcr.io/graphrt/graphrt:0.6.3".into(),
        secrets: BTreeMap::new(),
        api_key_candidate: API_KEY.into(),
        rotate_api_key: false,
        agent: AgentFile::new("support.yaml", format!("# {version}\n").into_bytes()),
        now_ms,
        serve: ServeOptions::default(),
        drop_store: false,
    }
}

fn monitored(version: &str, now_ms: u64) -> DeployRequest {
    let mut request = request(version, now_ms);
    request.serve = ServeOptions { monitor: true, ..ServeOptions::default() };
    request
}

async fn deploy(direct: &FlyDirect, request: &DeployRequest) -> (Vec<StepEvent>, Result<String, (ErrorCode, String)>) {
    let mut events = Vec::new();
    let result = direct.deploy(request, &mut |event| events.push(event)).await;
    let result = result.map(|outcome| outcome.version).map_err(|failure| (failure.error.code, failure.error.message));
    (events, result)
}

fn starting_message(events: &[StepEvent]) -> &str {
    events
        .iter()
        .find(|event| event.step == Step::Starting && event.status == StepStatus::Done)
        .map(|event| event.message.as_str())
        .unwrap()
}

async fn labels(direct: &FlyDirect) -> Vec<String> {
    direct.list_versions().await.unwrap().into_iter().map(|version| version.label).collect()
}

#[tokio::test]
async fn at_the_limit_the_oldest_standby_version_makes_room_for_the_new_one() {
    let fake = FakeFly::start_empty().await;
    fake.state().machine_limit = Some(2);
    let direct = direct(&fake);
    deploy(&direct, &request("v1", 1_000)).await.1.unwrap();
    deploy(&direct, &request("v2", 2_000)).await.1.unwrap();

    let (events, result) = deploy(&direct, &request("v3", 3_000)).await;

    assert_eq!(result.unwrap(), "v3");
    assert!(
        starting_message(&events).contains("removed standby version(s) v1 to stay within"),
        "the deploy says which version it removed: {}",
        starting_message(&events)
    );
    assert_eq!(labels(&direct).await, ["v3", "v2"]);
    assert_eq!(fake.state().machines.len(), 2);
}

#[tokio::test]
async fn at_the_limit_with_no_standby_version_the_error_says_what_to_do_and_the_old_version_serves() {
    let fake = FakeFly::start_empty().await;
    fake.state().machine_limit = Some(1);
    let direct = direct(&fake);
    deploy(&direct, &request("v1", 1_000)).await.1.unwrap();

    let (_, result) = deploy(&direct, &request("v2", 2_000)).await;

    let (code, message) = result.unwrap_err();
    assert_eq!(code, ErrorCode::FlyApi);
    assert!(message.contains("Pay As You Go"), "{message}");
    assert!(message.contains("no standby version of this deployment was left to remove"), "{message}");
    let status = direct.status(3_000).await.unwrap();
    assert_eq!(status.status, DeploymentState::Online);
    assert_eq!(status.current_version.as_deref(), Some("v1"));
}

#[tokio::test]
async fn the_machine_with_the_store_is_never_removed_to_make_room() {
    let fake = FakeFly::start_empty().await;
    fake.state().machine_limit = Some(2);
    let direct = direct(&fake);
    deploy(&direct, &request("v1", 1_000)).await.1.unwrap();
    // Turning monitoring on creates the Machine with the store; v1 becomes standby.
    deploy(&direct, &monitored("v2", 2_000)).await.1.unwrap();
    // Turning it off needs a new Machine: v1 is removed for it, the store Machine stays.
    let (events, result) = deploy(&direct, &request("v3", 3_000)).await;
    assert_eq!(result.unwrap(), "v3");
    assert!(starting_message(&events).contains("removed standby version(s) v1"));

    let (_, result) = deploy(&direct, &request("v4", 4_000)).await;

    let (code, message) = result.unwrap_err();
    assert_eq!(code, ErrorCode::FlyApi);
    assert!(message.contains("no standby version of this deployment was left to remove"), "{message}");
    {
        let state = fake.state();
        assert!(state.machine_named("graphrt-store").is_some(), "the store Machine is kept");
        assert!(state.volume_named("graphrt_store").is_some(), "the store volume is kept");
    }
    let status = direct.status(5_000).await.unwrap();
    assert_eq!(status.current_version.as_deref(), Some("v3"));
}

#[tokio::test]
async fn a_rollback_that_cannot_start_leaves_the_version_a_standby_and_the_current_one_serving() {
    let fake = FakeFly::start_empty().await;
    let direct = direct(&fake);
    deploy(&direct, &request("v1", 1_000)).await.1.unwrap();
    deploy(&direct, &request("v2", 2_000)).await.1.unwrap();
    let v1 = fake
        .state()
        .machines
        .iter()
        .find(|machine| machine.metadata("graphrt_version") == Some("v1"))
        .unwrap()
        .id
        .clone();
    fake.state().full_hosts.push(v1.clone());

    let result = direct.rollback("v1", &mut |_| {}).await;

    assert_eq!(result.unwrap_err().code, ErrorCode::Capacity);
    {
        let state = fake.state();
        let old = state.machine(&v1).unwrap();
        assert_eq!(old.metadata("graphrt_role"), Some("standby"));
        assert!(old.config.get("services").is_none(), "the proxy must not route to a version that did not start");
    }
    assert_eq!(direct.status(3_000).await.unwrap().current_version.as_deref(), Some("v2"));
}

#[tokio::test]
async fn a_resume_right_after_a_pause_sees_the_pause_while_fly_still_applies_it() {
    let fake = FakeFly::start_empty().await;
    let direct = direct(&fake);
    deploy(&direct, &monitored("v1", 1_000)).await.1.unwrap();
    fake.state().replacing_reads = 3;

    assert!(direct.pause().await.unwrap());
    let resumed = direct.resume(&mut |_| {}).await.unwrap();

    assert_eq!(resumed.map(|serving| serving.version), Some("v1".to_owned()), "the resume must not be a no-op");
    let state = fake.state();
    let machine = state.machine_named("graphrt-store").unwrap();
    assert_eq!(machine.state, "started");
    assert!(machine.config.get("services").is_some());
}

#[tokio::test]
async fn after_a_deploy_only_the_new_version_is_current_while_fly_still_applies_the_demotion() {
    let fake = FakeFly::start_empty().await;
    let direct = direct(&fake);
    deploy(&direct, &request("v1", 1_000)).await.1.unwrap();
    fake.state().replacing_reads = 3;
    deploy(&direct, &request("v2", 2_000)).await.1.unwrap();

    let status = direct.status(3_000).await.unwrap();

    let current: Vec<_> = status.machines.iter().filter(|machine| machine.role.as_deref() == Some("current")).collect();
    assert_eq!(current.len(), 1, "{:?}", status.machines);
    assert_eq!(status.current_version.as_deref(), Some("v2"));
    assert_eq!(direct.rollback("v1", &mut |_| {}).await.unwrap().version, "v1");
}
