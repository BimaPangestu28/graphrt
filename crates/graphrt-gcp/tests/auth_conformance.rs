//! C8 (auth) for `auth-gcp` against a fake metadata server and token endpoint:
//! caching until `exp - 5 min`, refresh before expiry, ±5 min clock skew,
//! one refresh on 401, and no credential in any log line or error.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod support;

use std::sync::atomic::Ordering;
use std::time::Duration;

use graphrt_gcp::auth::{GcpAuthBuilder, ServiceAccountKey};
use graphrt_provider_api::conformance::{FakeResponse, Fault};
use support::{FakeGoogle, jwt_claims, service_account_json};

#[tokio::test]
async fn c08_service_account_access_token_is_signed_and_cached() {
    let google = FakeGoogle::start("http://127.0.0.1:9").await;
    let auth = google.service_account_auth();
    let tokens = auth.access_tokens();
    let first = tokens.token(false).await.unwrap();
    assert!(first.expose().starts_with("ya29.fake-"));
    // Twenty concurrent callers share the cached token: no new fetch.
    let all = futures::future::join_all((0..20).map(|_| tokens.token(false))).await;
    assert!(all.iter().all(|token| token.as_ref().unwrap() == &first));
    assert_eq!(google.requests_to("POST", "/token").len(), 1);
    assert_eq!(auth.principal(), support::CLIENT_EMAIL);
}

#[tokio::test]
async fn c08_concurrent_first_fetch_is_single_flight() {
    let google = FakeGoogle::start("http://127.0.0.1:9").await;
    let auth = google.metadata_auth();
    let tokens = auth.access_tokens();
    let all = futures::future::join_all((0..20).map(|_| tokens.token(false))).await;
    assert!(all.iter().all(Result::is_ok));
    assert_eq!(google.requests_to("GET", "/computeMetadata/v1/instance/service-accounts/default/token").len(), 1);
}

#[tokio::test]
async fn c08_refreshes_five_minutes_before_expiry() {
    let google = FakeGoogle::start("http://127.0.0.1:9").await;
    // 301 s tokens are due for refresh 1 s after they are fetched.
    google.state.lock().unwrap().access_token_lifetime_secs = 301;
    let tokens = google.metadata_auth().access_tokens();
    let first = tokens.token(false).await.unwrap();
    assert_eq!(tokens.token(false).await.unwrap(), first, "still fresh");
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    let second = tokens.token(false).await.unwrap();
    assert_ne!(second, first, "refreshed before exp, not after");
}

#[tokio::test]
async fn c08_id_tokens_are_cached_per_audience() {
    let google = FakeGoogle::start("http://127.0.0.1:9").await;
    for auth in [google.metadata_auth(), google.service_account_auth()] {
        let first = auth.id_tokens("https://a.run.app").token(false).await.unwrap();
        assert_eq!(jwt_claims(first.expose())["aud"], "https://a.run.app");
        assert_eq!(auth.id_tokens("https://a.run.app").token(false).await.unwrap(), first);
        let other = auth.id_tokens("graphrt/p/r/s").token(false).await.unwrap();
        assert_eq!(jwt_claims(other.expose())["aud"], "graphrt/p/r/s");
    }
}

#[tokio::test]
async fn c08_clock_skew_of_five_minutes_either_way_is_corrected_once() {
    for offset in [300_i64, -300] {
        let google = FakeGoogle::start("http://127.0.0.1:9").await;
        google.state.lock().unwrap().token_clock_offset_secs = offset;
        let auth = google.service_account_auth();
        auth.access_tokens().token(false).await.unwrap();
        // One rejected assertion, one corrected from the endpoint's Date header.
        assert_eq!(google.requests_to("POST", "/token").len(), 2, "offset {offset}");
        // The learned offset is reused: the ID token needs one request only.
        auth.id_tokens("https://a.run.app").token(false).await.unwrap();
        assert_eq!(google.requests_to("POST", "/token").len(), 3, "offset {offset}");
    }
}

#[tokio::test]
async fn c08_401_forces_exactly_one_refresh() {
    let google = FakeGoogle::start("http://127.0.0.1:9").await;
    let tokens = google.metadata_auth().access_tokens();
    let stale = tokens.token(false).await.unwrap();
    let fresh = tokens.token(true).await.unwrap();
    assert_ne!(stale, fresh);
    assert_eq!(tokens.token(false).await.unwrap(), fresh);
}

#[tokio::test]
async fn c08_metadata_server_must_identify_itself() {
    let google = FakeGoogle::start("http://127.0.0.1:9").await;
    google.inject(Fault {
        path_prefix: "/computeMetadata".into(),
        response: FakeResponse::json(200, &serde_json::json!({"access_token": "ya29.spoofed", "expires_in": 3600})),
        count: 1,
    });
    let error = google.metadata_auth().access_tokens().token(false).await.unwrap_err();
    assert!(error.message.contains("Metadata-Flavor"), "{error}");
}

#[tokio::test]
async fn c08_no_credential_in_debug_output_or_errors() {
    let google = FakeGoogle::start("http://127.0.0.1:9").await;
    let key = ServiceAccountKey::from_json(&service_account_json()).unwrap();
    let key_debug = format!("{key:?}");
    let auth = google.service_account_auth();
    let token = auth.access_tokens().token(false).await.unwrap();
    let id_token = auth.id_tokens("https://a.run.app").token(false).await.unwrap();

    // A rejected assertion: the error names the OAuth error, never the assertion.
    google.inject(Fault {
        path_prefix: "/token".into(),
        response: FakeResponse::json(
            400,
            &serde_json::json!({"error": "invalid_grant", "error_description": "Invalid JWT Signature."}),
        ),
        count: 1,
    });
    let rejected = GcpAuthBuilder::service_account(ServiceAccountKey::from_json(&service_account_json()).unwrap())
        .token_endpoint(format!("{}/token", google.auth_url()))
        .build()
        .unwrap();
    let error = rejected.access_tokens().token(false).await.unwrap_err();

    let log = format!("{key_debug}\n{auth:?}\n{rejected:?}\n{error}\n{error:?}\n{token:?}\n{id_token:?}");
    let pem_line = support::TEST_KEY_PEM.lines().nth(5).unwrap();
    for secret in [token.expose(), id_token.expose(), pem_line, "eyJ"] {
        assert!(!log.contains(secret), "credential leaked into: {log}");
    }
    assert!(error.message.contains("invalid_grant"));
    assert_eq!(google.tokens_issued.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn c08_key_file_round_trip() {
    let directory = std::env::temp_dir().join(format!("graphrt-gcp-test-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("key.json");
    std::fs::write(&path, service_account_json()).unwrap();
    let key = ServiceAccountKey::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(key.client_email(), support::CLIENT_EMAIL);
    assert_eq!(key.project_id(), Some(support::PROJECT));
    std::fs::remove_dir_all(&directory).unwrap();
}
