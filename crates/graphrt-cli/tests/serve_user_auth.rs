//! `--verify-jwt` / `--user-rate-limit`: a verified end-user JWT authorizes run
//! starts without the API key, sets `sys.user_id`, and is rate limited per `sub`;
//! invalid tokens and every other endpoint stay closed.
#![cfg(feature = "jwt")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use graphrt_cli::header_inputs::HeaderInputSpec;
use graphrt_cli::serve::{build_state, discover, router};
use graphrt_cli::setup::ServiceOptions;
use graphrt_cli::user_auth::UserAuth;
use graphrt_cli::user_auth::token::{ClaimRules, VerifyKey};
use graphrt_core::engine::Limits;
use http_body_util::BodyExt;
use serde_json::{Value as Json, json};
use tower::ServiceExt;

const SECRET: &[u8] = b"test-jwt-secret-with-at-least-32-bytes";
const HEADER: &str = "X-User-Authorization";
const API_KEY: &str = "server-only-key";
const ISSUER: &str = "https://project.supabase.co/auth/v1";

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("graphrt-user-auth-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_workflow(dir: &TempDir) -> PathBuf {
    let flows = dir.path().join("flows");
    std::fs::create_dir_all(&flows).unwrap();
    std::fs::write(
        flows.join("chat.yaml"),
        r#"version: "1"
name: chat
graph:
  nodes:
    - id: start
      type: start
      variables:
        - { variable: user_jwt, type: secret, required: true }
    - id: done
      type: answer
      answer: "user={{#sys.user_id#}}"
  edges:
    - { source: start, target: done }
"#,
    )
    .unwrap();
    // A workflow not built for end users (no `user_jwt` input).
    let examples = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    std::fs::copy(examples.join("hello.yaml"), flows.join("internal.yaml")).unwrap();
    flows
}

async fn app(dir: &TempDir, per_minute: u32) -> axum::Router {
    let flows = write_workflow(dir);
    let files = discover(&flows).expect("discover");
    let options = ServiceOptions {
        plugins: graphrt_cli::extensions::PluginOptions { dirs: Vec::new(), no_default: true, trusted: Vec::new() },
        ..ServiceOptions::default()
    };
    let specs: Vec<HeaderInputSpec> = vec![format!("user_jwt={HEADER}").parse().unwrap()];
    let rules = ClaimRules { issuer: Some(ISSUER.into()), audience: Some("authenticated".into()), leeway_secs: 30 };
    let user_auth = UserAuth::with_keys(&specs[0], vec![VerifyKey::hs256(SECRET)], rules, per_minute);
    let state = build_state(&files, &options, Limits::default(), Some(API_KEY.into()))
        .await
        .expect("state")
        .with_header_inputs(&specs)
        .expect("header inputs")
        .with_user_auth(Some(user_auth));
    router(Arc::new(state), 1 << 20)
}

fn token_for(subject: &str, secret: &[u8], expires_in: i64) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    let header = URL_SAFE_NO_PAD.encode(json!({"alg": "HS256", "typ": "JWT"}).to_string());
    let claims = json!({ "sub": subject, "exp": now + expires_in, "iss": ISSUER, "aud": "authenticated", "role": "authenticated" });
    let input = format!("{header}.{}", URL_SAFE_NO_PAD.encode(claims.to_string()));
    let tag = ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret), input.as_bytes());
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(tag.as_ref()))
}

fn start(user_token: Option<&str>, api_key: Option<&str>, body: &Json) -> Request<Body> {
    let mut req = Request::post("/v1/workflows/chat/runs").header("content-type", "application/json");
    if let Some(token) = user_token {
        req = req.header(HEADER, format!("Bearer {token}"));
    }
    if let Some(key) = api_key {
        req = req.header("authorization", format!("Bearer {key}"));
    }
    req.body(Body::from(body.to_string())).unwrap()
}

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
    let res = app.clone().oneshot(req).await.expect("response");
    let (status, headers) = (res.status(), res.headers().clone());
    let body = res.into_body().collect().await.expect("body").to_bytes();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn a_valid_user_token_starts_runs_without_the_api_key() {
    let dir = TempDir::new("valid");
    let app = app(&dir, 0).await;
    let token = token_for("user-123", SECRET, 3600);
    // A client-supplied `sys.user_id` is replaced by the verified `sub`.
    let body = json!({ "sys": { "user_id": "someone-else" } });
    let (status, _, text) = send(&app, start(Some(&token), None, &body)).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let result: Json = serde_json::from_str(&text).unwrap();
    assert_eq!(result["answer"], "user=user-123", "{result}");
    assert!(!text.contains(&token), "token echoed: {text}");
}

#[tokio::test]
async fn invalid_tokens_are_refused_even_with_the_api_key() {
    let dir = TempDir::new("invalid");
    let app = app(&dir, 0).await;
    for token in [token_for("u", b"another-secret", 3600), token_for("u", SECRET, -3600), "not-a-jwt".to_owned()] {
        let (status, headers, text) = send(&app, start(Some(&token), Some(API_KEY), &json!({}))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{text}");
        assert!(text.contains("invalid user token"), "{text}");
        assert_eq!(headers["www-authenticate"], "Bearer error=\"invalid_token\"");
    }
}

#[tokio::test]
async fn without_a_token_the_api_key_rules_apply() {
    let dir = TempDir::new("no-token");
    let app = app(&dir, 0).await;
    let (status, _, text) = send(&app, start(None, None, &json!({}))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{text}");
    assert!(text.contains("bearer token"), "{text}");
    // With the key, the required header-mapped input is still missing.
    let (status, _, text) = send(&app, start(None, Some(API_KEY), &json!({}))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{text}");
    assert!(text.contains("missing `x-user-authorization` header"), "{text}");
}

#[tokio::test]
async fn user_tokens_do_not_open_other_endpoints() {
    let dir = TempDir::new("other");
    let app = app(&dir, 0).await;
    let token = token_for("user-123", SECRET, 3600);
    for uri in ["/v1/runs", "/v1/workflows"] {
        let req = Request::get(uri).header(HEADER, format!("Bearer {token}")).body(Body::empty()).unwrap();
        let (status, _, text) = send(&app, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}: {text}");
    }
}

#[tokio::test]
async fn each_user_is_rate_limited_separately() {
    let dir = TempDir::new("rate");
    let app = app(&dir, 2).await;
    let alice = token_for("alice", SECRET, 3600);
    for _ in 0..2 {
        let (status, _, text) = send(&app, start(Some(&alice), None, &json!({}))).await;
        assert_eq!(status, StatusCode::OK, "{text}");
    }
    let (status, headers, text) = send(&app, start(Some(&alice), None, &json!({}))).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{text}");
    let retry_after: u64 = headers["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((1..=31).contains(&retry_after), "{retry_after}");
    // A fresh token of the same user shares the quota; another user does not.
    let alice_again = token_for("alice", SECRET, 7200);
    assert_eq!(send(&app, start(Some(&alice_again), None, &json!({}))).await.0, StatusCode::TOO_MANY_REQUESTS);
    let bob = token_for("bob", SECRET, 3600);
    assert_eq!(send(&app, start(Some(&bob), None, &json!({}))).await.0, StatusCode::OK);
}

/// A JWKS served on loopback, fetched by `--jwt-jwks-url`, verifies ES256 tokens.
#[tokio::test]
async fn es256_tokens_verify_against_a_fetched_jwks() {
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};

    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
    let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
    let point = pair.public_key().as_ref();
    let jwks = json!({ "keys": [{
        "kty": "EC", "crv": "P-256", "alg": "ES256", "use": "sig", "kid": "k1",
        "x": URL_SAFE_NO_PAD.encode(&point[1..33]), "y": URL_SAFE_NO_PAD.encode(&point[33..65]),
    }]});
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let jwks_app = axum::Router::new().route(
        "/auth/v1/.well-known/jwks.json",
        axum::routing::get(move || {
            let jwks = jwks.clone();
            async move { axum::Json(jwks) }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, jwks_app).await.unwrap() });

    let args = graphrt_cli::args::UserAuthArgs {
        verify_jwt: Some("user_jwt".into()),
        jwt_jwks_url: Some(format!("http://{addr}/auth/v1/.well-known/jwks.json")),
        jwt_issuer: Some(ISSUER.into()),
        jwt_audience: Some("authenticated".into()),
        ..Default::default()
    };
    let specs: Vec<HeaderInputSpec> = vec![format!("user_jwt={HEADER}").parse().unwrap()];
    let auth = UserAuth::from_args(&args, &specs, |_| None).await.expect("config").expect("enabled");

    let sign = |kid: &str| {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let header = URL_SAFE_NO_PAD.encode(json!({"alg": "ES256", "kid": kid}).to_string());
        let claims = json!({ "sub": "user-es", "exp": now + 600, "iss": ISSUER, "aud": "authenticated" });
        let input = format!("{header}.{}", URL_SAFE_NO_PAD.encode(claims.to_string()));
        let sig = pair.sign(&rng, input.as_bytes()).unwrap();
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig.as_ref()))
    };
    let headers = |token: String| {
        let mut map = axum::http::HeaderMap::new();
        map.insert(HEADER, format!("Bearer {token}").parse().unwrap());
        map
    };
    let admitted = auth.admit(&headers(sign("k1"))).await.expect("valid");
    assert!(
        matches!(admitted, graphrt_cli::user_auth::Admission::User(ref t) if t.subject == "user-es"),
        "{admitted:?}"
    );
    let refused = auth.admit(&headers(sign("unknown"))).await.expect_err("unknown kid");
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn misconfigured_flags_are_usage_errors() {
    let specs: Vec<HeaderInputSpec> = vec![format!("user_jwt={HEADER}").parse().unwrap()];
    let unmapped = graphrt_cli::args::UserAuthArgs {
        verify_jwt: Some("other".into()),
        jwt_secret_env: Some("S".into()),
        ..Default::default()
    };
    assert!(UserAuth::from_args(&unmapped, &specs, |_| Some("x".into())).await.is_err());
    let no_keys = graphrt_cli::args::UserAuthArgs { verify_jwt: Some("user_jwt".into()), ..Default::default() };
    assert!(UserAuth::from_args(&no_keys, &specs, |_| None).await.is_err());
    let unset = graphrt_cli::args::UserAuthArgs {
        verify_jwt: Some("user_jwt".into()),
        jwt_secret_env: Some("S".into()),
        ..Default::default()
    };
    assert!(UserAuth::from_args(&unset, &specs, |_| None).await.is_err());
    let http_jwks = graphrt_cli::args::UserAuthArgs {
        verify_jwt: Some("user_jwt".into()),
        jwt_jwks_url: Some("http://example.com/jwks".into()),
        ..Default::default()
    };
    assert!(UserAuth::from_args(&http_jwks, &specs, |_| None).await.is_err());
    let off = graphrt_cli::args::UserAuthArgs::default();
    assert!(UserAuth::from_args(&off, &specs, |_| None).await.unwrap().is_none());
}

#[tokio::test]
async fn issuer_and_audience_fall_back_to_environment_variables() {
    let specs: Vec<HeaderInputSpec> = vec![format!("user_jwt={HEADER}").parse().unwrap()];
    let args = graphrt_cli::args::UserAuthArgs {
        verify_jwt: Some("user_jwt".into()),
        jwt_secret_env: Some("JWT_SECRET".into()),
        ..Default::default()
    };
    let env = |name: &str| match name {
        "JWT_SECRET" => Some(String::from_utf8(SECRET.to_vec()).unwrap()),
        "GRAPHRT_JWT_ISSUER" => Some("https://elsewhere.example/auth/v1".to_owned()),
        "GRAPHRT_JWT_AUDIENCE" => Some("authenticated".to_owned()),
        _ => None,
    };
    let auth = UserAuth::from_args(&args, &specs, env).await.unwrap().unwrap();
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(HEADER, format!("Bearer {}", token_for("u", SECRET, 600)).parse().unwrap());
    // The token's `iss` is ISSUER, not the one from the environment.
    let refused = auth.admit(&headers).await.expect_err("wrong issuer");
    let body = refused.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("wrong issuer"));
}

#[tokio::test]
async fn user_tokens_only_start_workflows_that_take_them() {
    let dir = TempDir::new("scope");
    let app = app(&dir, 0).await;
    let token = token_for("user-123", SECRET, 3600);
    let req = Request::post("/v1/workflows/internal/runs")
        .header("content-type", "application/json")
        .header(HEADER, format!("Bearer {token}"))
        .body(Body::from(json!({ "inputs": { "name": "Ada" } }).to_string()))
        .unwrap();
    let (status, _, text) = send(&app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    assert!(text.contains("does not accept user tokens"), "{text}");
    // The API key still starts it.
    let req = Request::post("/v1/workflows/internal/runs")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {API_KEY}"))
        .body(Body::from(json!({ "inputs": { "name": "Ada" } }).to_string()))
        .unwrap();
    assert_eq!(send(&app, req).await.0, StatusCode::OK);
}
