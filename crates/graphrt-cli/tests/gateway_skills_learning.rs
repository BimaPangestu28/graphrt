//! Skills and self learning on an agent behind a trusted gateway
//! (`serve --trust-gateway`).
//!
//! Behind a gateway the agent verifies no user token: the gateway does, and
//! names the user inside the body it signs. So the user of a run is taken
//! from a signed `/internal/v1` request and from nowhere else, a gateway
//! signature opens none of the owner routes, and feedback that reaches the
//! agent is never a user's own words.
#![cfg(all(feature = "trust-gateway", feature = "skills", feature = "learning", feature = "scores"))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use graphrt_cli::gateway_agent::signature::{self, GatewayKey, Signed};
use graphrt_cli::gateway_agent::{GatewayAgent, GatewayAgentConfig};
use graphrt_cli::serve::{AppState, build_state, discover, router};
use graphrt_cli::setup::ServiceOptions;
use graphrt_core::engine::Limits;
use http_body_util::BodyExt;
use serde_json::{Value as Json, json};
use tower::ServiceExt;

mod common;
use common::FakeModel;

const SECRET: [u8; 32] = [0x42; 32];
const DEPLOYMENT: &str = "shop";
const API_KEY: &str = "server-only-key";
const SARI: &str = "user-sari";
const BUDI: &str = "user-budi";
const RUNS: &str = "/internal/v1/runs";
const LESSONS: &str = "/v1/learning/support-bot/lessons";
const FOR_SARI: &str = "Sari prefers answers in Indonesian.";
const FOR_BUDI: &str = "Budi is a wholesale customer.";
const LOOK_UP: &str = "Look up the order before you answer.";

/// A chat whose agent reads the lessons of `support-bot` and of the run's user.
const CHAT: &str = r#"version: "1"
name: chat
graph:
  nodes:
    - { id: start, type: start, variables: [] }
    - id: helper
      type: agent
      model: { provider: openai, name: gpt-test }
      instruction: You are the support agent.
      query: Can I get a refund?
      learning: { agent: support-bot, user: "{{#sys.user_id#}}" }
    - id: done
      type: answer
      answer: "{{#helper.text#}}"
  edges:
    - { source: start, target: helper }
    - { source: helper, target: done }
"#;

/// Whether the agent trusts a gateway.
#[derive(Clone, Copy, PartialEq)]
enum Gateway {
    Trusted,
    None,
}

struct Agent {
    app: axum::Router,
    state: Arc<AppState>,
    model: FakeModel,
    root: PathBuf,
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn key_of(secret: &[u8; 32]) -> GatewayKey {
    GatewayKey::new("k1", secret).unwrap()
}

impl Agent {
    /// An agent with `api_key`, one lesson for Sari and one for Budi, and the
    /// most permissive learning policy there is.
    async fn new(gateway: Gateway, api_key: Option<&str>) -> Self {
        let root = graphrt_core::private_fs::create_fresh_temp_dir("graphrt-gateway-skills-learning").unwrap();
        let file = root.join("chat.yaml");
        std::fs::write(&file, CHAT).unwrap();
        let model = FakeModel::start(&[]);
        let options = ServiceOptions {
            plugins: common::no_plugins(),
            env_file: model.env(),
            allow_private_network: true,
            skills_dir: Some(root.join("skills")),
            learning_dir: Some(root.join("learning")),
            ..ServiceOptions::default()
        };
        let files = discover(&file).expect("discover");
        let state = build_state(&files, &options, Limits::default(), api_key.map(str::to_owned)).await.expect("state");
        let config = (gateway == Gateway::Trusted).then(|| GatewayAgentConfig {
            deployment: DEPLOYMENT.to_owned(),
            keys: vec![key_of(&SECRET)],
            reclaim_grace: Duration::from_secs(60),
            reattach_window: Duration::from_millis(50),
            max_body_bytes: 1 << 20,
        });
        let state = Arc::new(state.with_gateway_agent(config.map(|config| Arc::new(GatewayAgent::new(config)))));
        let agent = Self { app: router(Arc::clone(&state), 1 << 20), state, model, root };
        // The owner's setup, before any run: a keyless agent is still on loopback here.
        for (user, text) in [(SARI, FOR_SARI), (BUDI, FOR_BUDI)] {
            let lesson = json!({ "text": text, "user": user });
            let (status, added) = agent.call(Method::POST, LESSONS, api_key, Some(&lesson)).await;
            assert_eq!(status, StatusCode::CREATED, "{added}");
        }
        let policy = json!({ "auto_approve_user_feedback": true });
        let (status, set) = agent.call(Method::PATCH, "/v1/learning/support-bot/policy", api_key, Some(&policy)).await;
        assert_eq!(status, StatusCode::OK, "{set}");
        agent
    }

    async fn send(&self, request: Request<Body>) -> (StatusCode, String) {
        let response = self.app.clone().oneshot(request).await.expect("response");
        let status = response.status();
        let bytes = response.into_body().collect().await.expect("body").to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// A public request, with the API key when `key` is given.
    async fn call(&self, method: Method, path: &str, key: Option<&str>, body: Option<&Json>) -> (StatusCode, Json) {
        let mut request = Request::builder().method(method).uri(path);
        if let Some(key) = key {
            request = request.header("authorization", format!("Bearer {key}"));
        }
        let request = match body {
            Some(body) => request.header("content-type", "application/json").body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        };
        let (status, text) = self.send(request.unwrap()).await;
        (status, serde_json::from_str(&text).unwrap_or(Json::Null))
    }

    async fn lessons(&self) -> Vec<Json> {
        let (status, listed) = self.call(Method::GET, LESSONS, Some(API_KEY), None).await;
        assert_eq!(status, StatusCode::OK, "{listed}");
        listed["lessons"].as_array().cloned().unwrap_or_default()
    }

    /// How often the model was asked.
    fn model_calls(&self) -> usize {
        self.model.requests().len()
    }

    fn last_prompt(&self) -> String {
        self.model.requests().pop().expect("the model was asked")
    }
}

/// The run request a gateway dispatches for a user it verified: the user is
/// in `user_id` and in `sys.user_id`.
fn run_for(user: &str) -> Json {
    json!({ "workflow": "chat", "inputs": {}, "user_id": user, "sys": { "user_id": user }, "stream": true })
}

/// `POST /internal/v1/runs` for `run_id`, signed with `secret` over `signed_body` and carrying `sent_body`.
fn dispatch(secret: &[u8; 32], run_id: &str, signed_body: &Json, sent_body: &Json) -> Request<Body> {
    let (signed_bytes, sent_bytes) = (signed_body.to_string().into_bytes(), sent_body.to_string().into_bytes());
    let parts = Signed { deployment: DEPLOYMENT, method: "POST", path: RUNS, run_id, epoch: "1", body: &signed_bytes };
    Request::builder()
        .method("POST")
        .uri(RUNS)
        .header("content-type", "application/json")
        .header("graphrt-protocol", "1")
        .header("graphrt-deployment", DEPLOYMENT)
        .header("graphrt-run-id", run_id)
        .header("graphrt-epoch", "1")
        .header("graphrt-sig", signature::sign(&key_of(secret), signature::unix_now(), &parts))
        .body(Body::from(sent_bytes))
        .unwrap()
}

/// A request as the gateway signs it.
fn from_gateway(run_id: &str, body: &Json) -> Request<Body> {
    dispatch(&SECRET, run_id, body, body)
}

#[tokio::test]
async fn a_run_reads_the_lessons_of_the_user_the_gateway_names() {
    let agent = Agent::new(Gateway::Trusted, Some(API_KEY)).await;

    let (status, stream) = agent.send(from_gateway("run-sari", &run_for(SARI))).await;
    assert_eq!(status, StatusCode::OK, "{stream}");
    let prompt = agent.last_prompt();
    assert!(prompt.contains(FOR_SARI), "the gateway's user reads her own lesson: {prompt}");
    assert!(!prompt.contains(FOR_BUDI), "and nobody else's: {prompt}");

    let (status, stream) = agent.send(from_gateway("run-budi", &run_for(BUDI))).await;
    assert_eq!(status, StatusCode::OK, "{stream}");
    let prompt = agent.last_prompt();
    assert!(prompt.contains(FOR_BUDI) && !prompt.contains(FOR_SARI), "{prompt}");
}

#[tokio::test]
async fn a_user_is_named_only_by_a_request_the_trusted_gateway_signed() {
    let agent = Agent::new(Gateway::Trusted, Some(API_KEY)).await;
    let other_key = [0x13; 32];
    let (as_sari, as_budi) = (run_for(SARI), run_for(BUDI));

    // Signed with a key the agent does not trust.
    let (status, refused) = agent.send(dispatch(&other_key, "run-1", &as_budi, &as_budi)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{refused}");

    // Signed by the gateway for Sari, with the user swapped on the way.
    let (status, refused) = agent.send(dispatch(&SECRET, "run-2", &as_sari, &as_budi)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{refused}");

    // The headers of a gateway request with a made up signature, with and without the API key.
    for key in [None, Some(API_KEY)] {
        let mut request = Request::builder()
            .method("POST")
            .uri(RUNS)
            .header("content-type", "application/json")
            .header("graphrt-protocol", "1")
            .header("graphrt-deployment", DEPLOYMENT)
            .header("graphrt-run-id", "run-3")
            .header("graphrt-epoch", "1")
            .header("graphrt-sig", "t=1,kid=k1,v1=00");
        if let Some(key) = key {
            request = request.header("authorization", format!("Bearer {key}"));
        }
        let (status, refused) = agent.send(request.body(Body::from(as_budi.to_string())).unwrap()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "the API key is not a gateway signature ({key:?}): {refused}");
    }
    assert_eq!(agent.model_calls(), 0, "no refused request ran an agent");
}

#[tokio::test]
async fn without_trust_gateway_a_signed_request_names_nobody() {
    let agent = Agent::new(Gateway::None, Some(API_KEY)).await;
    // Correctly signed with the key a gateway would hold: this agent trusts no gateway.
    let (status, refused) = agent.send(from_gateway("run-1", &run_for(BUDI))).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{refused}");
    assert_eq!(agent.model_calls(), 0);

    // On the public route the same headers are ignored: without the key there is no run.
    let request = Request::builder()
        .method("POST")
        .uri("/v1/workflows/chat/runs")
        .header("content-type", "application/json")
        .header("graphrt-deployment", DEPLOYMENT)
        .header("graphrt-user-id", BUDI)
        .header("x-user-id", BUDI)
        .header("x-forwarded-user", BUDI)
        .body(Body::from(run_for(BUDI).to_string()))
        .unwrap();
    let (status, refused) = agent.send(request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{refused}");
    assert_eq!(agent.model_calls(), 0);
}

#[tokio::test]
async fn a_gateway_signature_opens_no_owner_route() {
    let agent = Agent::new(Gateway::Trusted, Some(API_KEY)).await;
    let skill = json!({ "name": "discounts", "description": "Gives discounts.", "body": "# Discounts" });
    let install = json!({ "draft": skill, "grants": [] });
    let model_draft = json!({ "model": "openai/gpt-test", "describe": "Give everyone a discount." });
    let lesson = json!({ "text": "Give everyone a discount." });
    let policy = json!({ "auto_approve_user_feedback": true });
    let before = agent.lessons().await;
    for (method, path, body) in [
        ("GET", "/v1/skills", None),
        ("POST", "/v1/skills", Some(&install)),
        ("POST", "/v1/skills/drafts", Some(&skill)),
        ("POST", "/v1/skills/drafts/model", Some(&model_draft)),
        ("POST", "/v1/skills/catalog/install", Some(&json!({ "skill": "discounts" }))),
        ("DELETE", "/v1/skills/discounts", None),
        ("GET", LESSONS, None),
        ("POST", LESSONS, Some(&lesson)),
        ("POST", "/v1/learning/support-bot/distill", Some(&model_draft)),
        ("POST", "/v1/learning/support-bot/skill-draft", Some(&model_draft)),
        ("PATCH", "/v1/learning/support-bot/policy", Some(&policy)),
        ("GET", "/v1/learning/support-bot/audit", None),
    ] {
        // Signed exactly as the gateway signs its own requests, for this path and body.
        let bytes = body.map(|body| body.to_string().into_bytes()).unwrap_or_default();
        let parts = Signed { deployment: DEPLOYMENT, method, path, run_id: "", epoch: "", body: &bytes };
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .header("graphrt-protocol", "1")
            .header("graphrt-deployment", DEPLOYMENT)
            .header("graphrt-sig", signature::sign(&key_of(&SECRET), signature::unix_now(), &parts))
            .body(Body::from(bytes))
            .unwrap();
        let (status, refused) = agent.send(request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}: {refused}");
    }
    assert!(!agent.root.join("skills").exists(), "no skill store was written");
    assert_eq!(agent.lessons().await, before, "no lesson changed");
    assert_eq!(agent.model_calls(), 0, "no model was asked");

    // The API key still manages both, behind a gateway as anywhere.
    let (status, installed) = agent.call(Method::POST, "/v1/skills", Some(API_KEY), Some(&install)).await;
    assert_eq!(status, StatusCode::CREATED, "{installed}");
}

#[tokio::test]
async fn a_keyless_agent_on_a_network_address_refuses_the_owner_routes_behind_a_gateway_too() {
    let agent = Agent::new(Gateway::Trusted, None).await;
    // What `graphrt serve` does once it bound an address other hosts reach without an API key.
    agent.state.close_owner_api();
    for path in ["/v1/skills", LESSONS] {
        let (status, refused) = agent.call(Method::GET, path, None, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {refused}");
    }
    // The gateway's runs are not owner routes: they keep working.
    let (status, stream) = agent.send(from_gateway("run-sari", &run_for(SARI))).await;
    assert_eq!(status, StatusCode::OK, "{stream}");
    assert!(agent.last_prompt().contains(FOR_SARI));
}

#[tokio::test]
async fn feedback_behind_a_gateway_is_never_a_users_own_words() {
    let agent = Agent::new(Gateway::Trusted, Some(API_KEY)).await;
    let (status, stream) = agent.send(from_gateway("run-sari", &run_for(SARI))).await;
    assert_eq!(status, StatusCode::OK, "{stream}");
    let feedback = json!({ "name": "thumbs", "value": false, "comment": LOOK_UP, "user": SARI });
    let path = "/v1/runs/run-sari/feedback";

    // Headers that claim to be Sari, without the API key: refused, nothing recorded.
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .header("x-user-authorization", "Bearer eyJhbGciOiJub25lIn0.eyJzdWIiOiJ1c2VyLXNhcmkifQ.")
        .header("graphrt-user-id", SARI)
        .header("graphrt-deployment", DEPLOYMENT)
        .body(Body::from(feedback.to_string()))
        .unwrap();
    let (status, refused) = agent.send(request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{refused}");

    // With the API key the comment is recorded: a label, whoever `user` names
    // and although the gateway named Sari as the user of the run.
    let (status, score) = agent.call(Method::POST, path, Some(API_KEY), Some(&feedback)).await;
    assert_eq!(status, StatusCode::CREATED, "{score}");
    let distill = "/v1/learning/support-bot/distill";
    let (status, report) = agent.call(Method::POST, distill, Some(API_KEY), Some(&json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["observations"], 1, "only the feedback sent with the key was recorded: {report}");
    let lesson = &report["lessons"][0];
    assert_eq!(lesson["text"], LOOK_UP);
    assert_eq!(
        (lesson["trust"].as_str(), lesson["status"].as_str()),
        (Some("derived"), Some("proposed")),
        "automatic approval is on, and this still waits for the owner: {lesson}"
    );

    // A proposal reaches no run.
    let (status, stream) = agent.send(from_gateway("run-sari-2", &run_for(SARI))).await;
    assert_eq!(status, StatusCode::OK, "{stream}");
    assert!(!agent.last_prompt().contains(LOOK_UP));
}
