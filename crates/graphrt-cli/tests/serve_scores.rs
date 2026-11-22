//! Scores of served runs: `GET /v1/runs/{id}/scores`, human feedback through
//! `POST /v1/runs/{id}/feedback` (validation, auth, JWT ownership), storage in
//! the memory and directory stores, and the `evaluate` span of each score.
#![cfg(feature = "scores")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use graphrt_cli::serve::{AppState, build_state, discover, router};
use graphrt_cli::setup::ServiceOptions;
use graphrt_core::engine::Limits;
use http_body_util::BodyExt;
use serde_json::{Value as Json, json};
use tower::ServiceExt;

const API_KEY: &str = "server-key";

fn options() -> ServiceOptions {
    ServiceOptions {
        plugins: graphrt_cli::extensions::PluginOptions { dirs: Vec::new(), no_default: true, trusted: Vec::new() },
        ..ServiceOptions::default()
    }
}

async fn hello_state(api_key: Option<&str>) -> AppState {
    let hello = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/hello.yaml");
    let files = discover(&hello).expect("discover");
    build_state(&files, &options(), Limits::default(), api_key.map(str::to_owned)).await.expect("state")
}

fn post(path: &str, key: Option<&str>, body: &Json) -> Request<Body> {
    let mut req = Request::post(path).header("content-type", "application/json");
    if let Some(key) = key {
        req = req.header("authorization", format!("Bearer {key}"));
    }
    req.body(Body::from(body.to_string())).unwrap()
}

fn get(path: &str, key: Option<&str>) -> Request<Body> {
    let mut req = Request::get(path);
    if let Some(key) = key {
        req = req.header("authorization", format!("Bearer {key}"));
    }
    req.body(Body::empty()).unwrap()
}

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, Json) {
    let res = app.clone().oneshot(req).await.expect("response");
    let status = res.status();
    let body = res.into_body().collect().await.expect("body").to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Json::Null))
}

async fn start_hello(app: &axum::Router, key: Option<&str>) -> String {
    let (status, result) =
        send(app, post("/v1/workflows/hello/runs", key, &json!({ "inputs": { "name": "Ada" } }))).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    result["run_id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn feedback_is_stored_replaced_and_listed() {
    let app = router(Arc::new(hello_state(Some(API_KEY)).await), 1 << 20);
    let run_id = start_hello(&app, Some(API_KEY)).await;
    let feedback = format!("/v1/runs/{run_id}/feedback");
    let scores = format!("/v1/runs/{run_id}/scores");

    let body = json!({ "name": "thumbs", "value": true, "comment": "spot on", "user": "reviewer-1" });
    let (status, score) = send(&app, post(&feedback, Some(API_KEY), &body)).await;
    assert_eq!(status, StatusCode::CREATED, "{score}");
    assert_eq!((score["source"].as_str(), score["user"].as_str()), (Some("human"), Some("reviewer-1")));
    assert_eq!(score["id"].as_str().map(str::len), Some(32));

    // The same reviewer rating the same thing again replaces the score.
    let again = json!({ "name": "thumbs", "value": false, "user": "reviewer-1" });
    let (status, replaced) = send(&app, post(&feedback, Some(API_KEY), &again)).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(replaced["id"], score["id"]);
    let other = json!({ "name": "quality", "value": 4, "user": "reviewer-1" });
    assert_eq!(send(&app, post(&feedback, Some(API_KEY), &other)).await.0, StatusCode::CREATED);
    let text = json!({ "name": "category", "value": "billing" });
    assert_eq!(send(&app, post(&feedback, Some(API_KEY), &text)).await.0, StatusCode::CREATED);

    let (status, listed) = send(&app, get(&scores, Some(API_KEY))).await;
    assert_eq!(status, StatusCode::OK);
    let listed = listed["scores"].as_array().unwrap();
    assert_eq!(listed.len(), 3, "{listed:?}");
    assert_eq!(listed[0]["value"], false);
    assert_eq!(listed[1]["value"], 4.0);
    assert_eq!(listed[2]["value"], "billing");
}

#[tokio::test]
async fn feedback_is_validated_and_needs_the_key() {
    let app = router(Arc::new(hello_state(Some(API_KEY)).await), 1 << 20);
    let run_id = start_hello(&app, Some(API_KEY)).await;
    let feedback = format!("/v1/runs/{run_id}/feedback");
    let ok = json!({ "name": "thumbs", "value": 1 });
    assert_eq!(send(&app, post(&feedback, None, &ok)).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(send(&app, post(&feedback, Some("wrong"), &ok)).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(send(&app, get(&format!("/v1/runs/{run_id}/scores"), None)).await.0, StatusCode::UNAUTHORIZED);
    let missing = send(&app, post("/v1/runs/run-nope/feedback", Some(API_KEY), &ok)).await;
    assert_eq!(missing.0, StatusCode::NOT_FOUND);
    assert_eq!(send(&app, get("/v1/runs/run-nope/scores", Some(API_KEY))).await.0, StatusCode::NOT_FOUND);
    let (status, error) = send(&app, post(&feedback, Some(API_KEY), &json!({ "name": "", "value": 1 }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    let long = json!({ "name": "n", "value": "x".repeat(501) });
    assert_eq!(send(&app, post(&feedback, Some(API_KEY), &long)).await.0, StatusCode::BAD_REQUEST);
    for bad in [json!({ "name": "n", "value": {"a": 1} }), json!({ "name": "n", "value": 1, "score": 2 })] {
        let (status, _) = send(&app, post(&feedback, Some(API_KEY), &bad)).await;
        assert!(status.is_client_error(), "{bad}: {status}");
    }
    let (_, listed) = send(&app, get(&format!("/v1/runs/{run_id}/scores"), Some(API_KEY))).await;
    assert_eq!(listed["scores"], json!([]));
}

#[tokio::test]
async fn feedback_authenticates_before_reading_the_body() {
    let app = router(Arc::new(hello_state(Some(API_KEY)).await), 1 << 20);
    let run_id = start_hello(&app, Some(API_KEY)).await;
    let feedback = format!("/v1/runs/{run_id}/feedback");
    // Without a key, neither the content type nor the body may be judged first.
    let untyped = Request::post(&feedback).body(Body::from("{}")).unwrap();
    assert_eq!(send(&app, untyped).await.0, StatusCode::UNAUTHORIZED);
    let malformed = Request::post(&feedback).header("content-type", "application/json").body(Body::from("{")).unwrap();
    assert_eq!(send(&app, malformed).await.0, StatusCode::UNAUTHORIZED);
    // With the key, the body is still checked.
    let keyed = Request::post(&feedback)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {API_KEY}"))
        .body(Body::from("{"))
        .unwrap();
    assert_eq!(send(&app, keyed).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_directory_store_keeps_scores_next_to_the_run() {
    let dir = std::env::temp_dir().join(format!("graphrt-serve-scores-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = graphrt_cli::store::FileStore::open(&dir).unwrap();
    let state = hello_state(None).await.with_stores(graphrt_cli::store::Stores::of(Arc::new(store)));
    let app = router(Arc::new(state), 1 << 20);
    let run_id = start_hello(&app, None).await;
    let body = json!({ "name": "thumbs", "value": true });
    assert_eq!(send(&app, post(&format!("/v1/runs/{run_id}/feedback"), None, &body)).await.0, StatusCode::CREATED);
    let stored: Json =
        serde_json::from_slice(&std::fs::read(dir.join("runs").join(&run_id).join("scores.json")).unwrap()).unwrap();
    assert_eq!(stored[0]["name"], "thumbs");
    let request = Request::delete(format!("/v1/runs/{run_id}")).body(Body::empty()).unwrap();
    assert_eq!(send(&app, request).await.0, StatusCode::NO_CONTENT);
    assert!(!dir.join("runs").join(&run_id).exists(), "deleted with the run");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(feature = "otel")]
#[tokio::test]
async fn each_score_is_an_evaluate_span_in_the_run_trace() {
    use graphrt_otel::{AttrValue, CaptureMode, InMemoryExporter};
    let exporter = InMemoryExporter::new();
    let telemetry =
        Arc::new(graphrt_cli::otel::Telemetry::with_exporter(Arc::new(exporter.clone()), CaptureMode::NoContent));
    let state = hello_state(None).await.with_telemetry(Some(telemetry.clone()));
    let state = Arc::new(state);
    let app = router(state.clone(), 1 << 20);
    let run_id = start_hello(&app, None).await;
    let record = state.store().get(&run_id).await.unwrap().unwrap();
    let body = json!({ "name": "helpful", "value": 0.75, "comment": "mostly" });
    assert_eq!(send(&app, post(&format!("/v1/runs/{run_id}/feedback"), None, &body)).await.0, StatusCode::CREATED);
    let spans = exporter.spans();
    let span = spans.iter().find(|s| s.name == "evaluate helpful").expect("evaluate span");
    assert_eq!(Some(span.trace_id.to_hex()), record.trace_id);
    assert_eq!(span.parent_span_id.map(|id| id.to_hex()), record.span_id);
    let event = &span.events[0];
    assert_eq!(event.name, "gen_ai.evaluation.result");
    let attr = |key: &str| event.attributes.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
    assert_eq!(attr("gen_ai.evaluation.name"), Some(AttrValue::from("helpful")));
    assert_eq!(attr("gen_ai.evaluation.score.value"), Some(AttrValue::Double(0.75)));
    assert_eq!(attr("gen_ai.evaluation.explanation"), Some(AttrValue::from("mostly")));
    assert_eq!(span.attr("graphrt.evaluation.source"), Some(&AttrValue::from("human")));
    let metrics = telemetry.metrics().encode();
    assert!(metrics.iter().any(|m| m["name"] == "graphrt.eval.score"), "{metrics:?}");
}

#[cfg(feature = "jwt")]
mod user_tokens {
    use std::time::{SystemTime, UNIX_EPOCH};

    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use graphrt_cli::header_inputs::HeaderInputSpec;
    use graphrt_cli::user_auth::UserAuth;
    use graphrt_cli::user_auth::token::{ClaimRules, VerifyKey};

    use super::*;

    const SECRET: &[u8] = b"test-jwt-secret-with-at-least-32-bytes";
    const HEADER: &str = "X-User-Authorization";

    fn token_for(subject: &str) -> String {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let header = URL_SAFE_NO_PAD.encode(json!({"alg": "HS256", "typ": "JWT"}).to_string());
        let claims = json!({ "sub": subject, "exp": now + 3600 });
        let input = format!("{header}.{}", URL_SAFE_NO_PAD.encode(claims.to_string()));
        let tag = ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, SECRET), input.as_bytes());
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(tag.as_ref()))
    }

    async fn app() -> axum::Router {
        let dir = std::env::temp_dir().join(format!("graphrt-scores-jwt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let workflow = dir.join("chat.yaml");
        std::fs::write(
            &workflow,
            r#"version: "1"
name: chat
graph:
  nodes:
    - id: start
      type: start
      variables:
        - { variable: user_jwt, type: secret, required: false }
    - { id: done, type: answer, answer: "hi" }
  edges:
    - { source: start, target: done }
"#,
        )
        .unwrap();
        let files = discover(&workflow).unwrap();
        let specs: Vec<HeaderInputSpec> = vec![format!("user_jwt={HEADER}").parse().unwrap()];
        let rules = ClaimRules { issuer: None, audience: None, leeway_secs: 30 };
        let user_auth = UserAuth::with_keys(&specs[0], vec![VerifyKey::hs256(SECRET)], rules, 0);
        let state = build_state(&files, &options(), Limits::default(), Some(API_KEY.into()))
            .await
            .unwrap()
            .with_header_inputs(&specs)
            .unwrap()
            .with_user_auth(Some(user_auth));
        router(Arc::new(state), 1 << 20)
    }

    fn with_token(mut req: Request<Body>, token: &str) -> Request<Body> {
        req.headers_mut().insert(HEADER, format!("Bearer {token}").parse().unwrap());
        req
    }

    async fn start_as(app: &axum::Router, token: Option<&str>) -> String {
        let req = match token {
            Some(token) => with_token(post("/v1/workflows/chat/runs", None, &json!({})), token),
            None => post("/v1/workflows/chat/runs", Some(API_KEY), &json!({})),
        };
        let (status, result) = send(app, req).await;
        assert_eq!(status, StatusCode::OK, "{result}");
        result["run_id"].as_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn users_rate_only_their_own_runs_as_themselves() {
        let app = app().await;
        let alice = token_for("alice");
        let bob = token_for("bob");
        let run_id = start_as(&app, Some(&alice)).await;
        let feedback = format!("/v1/runs/{run_id}/feedback");
        // The body cannot claim another user: the verified subject is the author.
        let body = json!({ "name": "thumbs", "value": true, "user": "mallory" });
        let (status, score) = send(&app, with_token(post(&feedback, None, &body), &alice)).await;
        assert_eq!(status, StatusCode::CREATED, "{score}");
        assert_eq!(score["user"], "alice");

        let (status, error) = send(&app, with_token(post(&feedback, None, &body), &bob)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
        let invalid = format!("{alice}x");
        assert_eq!(send(&app, with_token(post(&feedback, None, &body), &invalid)).await.0, StatusCode::UNAUTHORIZED);

        // A run started with the API key has no user: no user token may rate it.
        let internal = start_as(&app, None).await;
        let internal_feedback = format!("/v1/runs/{internal}/feedback");
        let (status, _) = send(&app, with_token(post(&internal_feedback, None, &body), &alice)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        // An API-key run labelled with a body `user_id` belongs to that user:
        // the record cannot tell a claimed user from a verified one.
        let (status, labelled) =
            send(&app, post("/v1/workflows/chat/runs", Some(API_KEY), &json!({ "user_id": "alice" }))).await;
        assert_eq!(status, StatusCode::OK, "{labelled}");
        let labelled_feedback = format!("/v1/runs/{}/feedback", labelled["run_id"].as_str().unwrap());
        assert_eq!(send(&app, with_token(post(&labelled_feedback, None, &body), &alice)).await.0, StatusCode::CREATED);
        assert_eq!(send(&app, with_token(post(&labelled_feedback, None, &body), &bob)).await.0, StatusCode::FORBIDDEN);
        // The API key still rates any run, with the body's user.
        let (status, keyed) = send(&app, post(&internal_feedback, Some(API_KEY), &body)).await;
        assert_eq!((status, keyed["user"].as_str()), (StatusCode::CREATED, Some("mallory")));

        // Reading scores stays an API-key endpoint.
        let scores = format!("/v1/runs/{run_id}/scores");
        assert_eq!(send(&app, with_token(get(&scores, None), &alice)).await.0, StatusCode::UNAUTHORIZED);
        let (status, listed) = send(&app, get(&scores, Some(API_KEY))).await;
        assert_eq!((status, listed["scores"].as_array().map(Vec::len)), (StatusCode::OK, Some(1)));
    }
}
