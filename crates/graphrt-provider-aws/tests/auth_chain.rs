//! Credential chain against fake metadata endpoints (C8, C10).
#![cfg(all(feature = "provider-ecs", feature = "provider-lambda"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

mod support;

use graphrt_provider_aws::auth::{
    AwsCredentials, ChainConfig, Clock, CredentialChain, CredentialSource, parse_iso8601,
};

fn scratch_file(name: &str, contents: &str) -> String {
    let dir = std::env::temp_dir().join(format!("graphrt-aws-test-{}-{name}", std::process::id()));
    std::fs::write(&dir, contents).unwrap();
    dir.to_string_lossy().into_owned()
}

#[tokio::test]
async fn environment_credentials_win() {
    let config = ChainConfig {
        static_credentials: Some(AwsCredentials {
            access_key_id: "A".into(),
            secret_access_key: "S".into(),
            session_token: None,
        }),
        container_relative_uri: Some("/never-called".into()),
        imds_disabled: true,
        ..Default::default()
    };
    let chain = CredentialChain::new(config, Clock::default()).unwrap();
    assert_eq!(chain.credentials().await.unwrap().access_key_id, "A");
    assert_eq!(chain.source().await, Some(CredentialSource::Environment));
}

#[tokio::test]
async fn container_endpoint_is_cached_until_five_minutes_before_expiry() {
    let fake = support::start().await;
    let now = parse_iso8601("2026-11-22T10:00:00Z").unwrap();
    fake.state.lock().unwrap().cred_expiration = Some("2026-11-22T10:15:00Z".into());
    let clock = Clock::fixed(now);
    let config = ChainConfig {
        container_relative_uri: Some("/creds".into()),
        container_host: Some(fake.base_url.clone()),
        imds_disabled: true,
        ..Default::default()
    };
    let chain = CredentialChain::new(config, clock.clone()).unwrap();
    chain.credentials().await.unwrap();
    chain.credentials().await.unwrap();
    assert_eq!(fake.state.lock().unwrap().cred_hits, 1, "cached");
    clock.correct_to(now + 9 * 60);
    chain.credentials().await.unwrap();
    assert_eq!(fake.state.lock().unwrap().cred_hits, 1, "still more than 5 min left");
    clock.correct_to(now + 10 * 60 + 1);
    chain.credentials().await.unwrap();
    assert_eq!(fake.state.lock().unwrap().cred_hits, 2, "refreshed before exp − 5 min");
    chain.invalidate().await;
    chain.credentials().await.unwrap();
    assert_eq!(fake.state.lock().unwrap().cred_hits, 3);
    assert_eq!(chain.source().await, Some(CredentialSource::ContainerEndpoint));
}

#[tokio::test]
async fn pod_identity_full_uri_sends_the_token_file() {
    let fake = support::start().await;
    let token_file = scratch_file("pod-identity", "pod-identity-token\n");
    let config = ChainConfig {
        container_full_uri: Some(format!("{}/creds", fake.base_url)),
        container_authorization_token_file: Some(token_file),
        imds_disabled: true,
        ..Default::default()
    };
    let chain = CredentialChain::new(config, Clock::default()).unwrap();
    let creds = chain.credentials().await.unwrap();
    assert_eq!(creds.session_token.as_deref(), Some(support::SESSION_TOKEN));
    assert_eq!(fake.state.lock().unwrap().container_auth_seen, vec!["pod-identity-token".to_owned()]);
}

#[tokio::test]
async fn full_uri_to_an_arbitrary_http_host_is_refused() {
    let config = ChainConfig {
        container_full_uri: Some("http://attacker.example/creds".into()),
        imds_disabled: true,
        ..Default::default()
    };
    let chain = CredentialChain::new(config, Clock::default()).unwrap();
    let error = chain.credentials().await.unwrap_err();
    assert_eq!(error.source, "container");
}

#[tokio::test]
async fn web_identity_assumes_the_role_through_sts() {
    let fake = support::start().await;
    let token_file = scratch_file("web-identity", "web-identity-jwt");
    let config = ChainConfig {
        web_identity_token_file: Some(token_file.clone()),
        role_arn: Some("arn:aws:iam::123:role/graphrt-gateway".into()),
        sts_endpoint: Some(fake.base_url.clone()),
        imds_disabled: true,
        ..Default::default()
    };
    let chain = CredentialChain::new(config.clone(), Clock::default()).unwrap();
    let creds = chain.credentials().await.unwrap();
    assert_eq!(creds.access_key_id, support::ACCESS_KEY);
    assert_eq!(chain.source().await, Some(CredentialSource::WebIdentity));
    let form = fake.state.lock().unwrap().sts_forms[0].clone();
    assert!(form.contains("Action=AssumeRoleWithWebIdentity") && form.contains("RoleSessionName=graphrt-gateway"));

    // A rejected token yields the STS error code, never the token.
    std::fs::write(&token_file, "stale-jwt").unwrap();
    let chain = CredentialChain::new(config, Clock::default()).unwrap();
    let error = chain.credentials().await.unwrap_err();
    assert!(error.message.contains("InvalidIdentityToken"), "{error}");
    assert!(!error.to_string().contains("stale-jwt"));
}

#[tokio::test]
async fn imds_v2_uses_a_session_token() {
    let fake = support::start().await;
    let config = ChainConfig { imds_endpoint: Some(fake.base_url.clone()), ..Default::default() };
    let chain = CredentialChain::new(config, Clock::default()).unwrap();
    let creds = chain.credentials().await.unwrap();
    assert_eq!(creds.access_key_id, support::ACCESS_KEY);
    assert!(fake.state.lock().unwrap().imds_token_seen);
    assert_eq!(chain.source().await, Some(CredentialSource::Imds));
    let debug = format!("{chain:?} {creds:?}");
    assert!(!debug.contains(support::SECRET_KEY) && !debug.contains(support::SESSION_TOKEN), "{debug}");
}

#[tokio::test]
async fn nothing_configured_and_imds_disabled_is_a_clear_error() {
    let chain =
        CredentialChain::new(ChainConfig { imds_disabled: true, ..Default::default() }, Clock::default()).unwrap();
    let error = chain.credentials().await.unwrap_err();
    assert!(error.message.contains("no credentials"), "{error}");
}
