//! What a phone needs from `graphrt serve` to watch an agent: a device token
//! that reads runs and resolves approvals and nothing else, the approvals
//! inbox, requests that expire, push notifications and the monitoring page.
#![cfg(all(feature = "web-monitor", feature = "web-push", feature = "code"))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use graphrt_cli::approvals::{Approvals, EXPIRED_REASON, expire_due};
use graphrt_cli::devices::Devices;
use graphrt_cli::notify::webpush::WebPush;
use graphrt_cli::notify::{Notifications, Notifier};
use graphrt_cli::serve::{AppState, Monitor, build_state, discover, router};
use graphrt_cli::setup::ServiceOptions;
use graphrt_core::engine::Limits as EngineLimits;
use graphrt_core::store::now_ms;
use http_body_util::BodyExt;
use serde_json::{Value as Json, json};
use tower::ServiceExt;

const API_KEY: &str = "server-only-key";

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("graphrt-monitor-{tag}-{}", std::process::id()));
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

/// `refund` asks a person and rejects by itself after a day, `strict` stops
/// when nobody answers, `broken` fails, `hello` just answers.
fn write_workflows(dir: &TempDir) -> PathBuf {
    let flows = dir.path().join("flows");
    std::fs::create_dir_all(&flows).unwrap();
    std::fs::write(
        flows.join("refund.yaml"),
        r#"version: "1"
name: refund
graph:
  nodes:
    - { id: start, type: start }
    - id: ask
      type: human-input
      prompt: "Refund 40 EUR to Ana?"
      form:
        fields:
          - { name: note, label: Note, type: text, required: false, default: "no answer in time" }
      actions:
        - { id: approve, title: Approve }
        - { id: reject, title: Reject }
      expire_after_ms: 86400000
      on_expire: { action: reject }
    - { id: paid, type: answer, answer: "refunded" }
    - { id: kept, type: answer, answer: "rejected ({{#ask.note#}})" }
  edges:
    - { source: start, target: ask }
    - { source: ask, target: paid, handle: approve }
    - { source: ask, target: kept, handle: reject }
"#,
    )
    .unwrap();
    std::fs::write(
        flows.join("strict.yaml"),
        r#"version: "1"
name: strict
graph:
  nodes:
    - { id: start, type: start }
    - id: ask
      type: human-input
      prompt: "Go?"
      expire_after_ms: 1000
    - { id: done, type: answer, answer: "went" }
  edges:
    - { source: start, target: ask }
    - { source: ask, target: done, handle: submit }
"#,
    )
    .unwrap();
    std::fs::write(
        flows.join("broken.yaml"),
        r#"version: "1"
name: broken
graph:
  nodes:
    - { id: start, type: start }
    - id: boom
      type: code
      code_language: javascript
      code: |
        function main() { throw new Error("the ledger is closed"); }
      outputs:
        done: { type: boolean }
    - { id: done, type: answer, answer: "never" }
  edges:
    - { source: start, target: boom }
    - { source: boom, target: done }
"#,
    )
    .unwrap();
    std::fs::write(
        flows.join("hello.yaml"),
        r#"version: "1"
name: hello
graph:
  nodes:
    - { id: start, type: start }
    - { id: done, type: answer, answer: "hi" }
  edges:
    - { source: start, target: done }
"#,
    )
    .unwrap();
    flows
}

/// How the server under test is set up.
#[derive(Default)]
struct Setup {
    push: Option<Arc<WebPush>>,
    /// Without the monitoring page, its inbox and its devices.
    plain: bool,
}

async fn state(dir: &TempDir, setup: Setup) -> Arc<AppState> {
    let flows = write_workflows(dir);
    let files = discover(&flows).expect("discover");
    let options = ServiceOptions {
        plugins: graphrt_cli::extensions::PluginOptions {
            dirs: Vec::new(),
            no_default: true,
            trusted: Vec::new(),
            pool: Default::default(),
            allow_private_network: false,
        },
        ..ServiceOptions::default()
    };
    let state = build_state(&files, &options, EngineLimits::default(), Some(API_KEY.into())).await.expect("state");
    if setup.plain {
        return Arc::new(state);
    }
    let notifiers: Vec<Arc<dyn Notifier>> =
        setup.push.iter().map(|push| Arc::clone(push) as Arc<dyn Notifier>).collect();
    let state = state
        .with_devices(Some(Devices::open(None).expect("devices")), Some("https://agent.example.com".to_owned()))
        .with_approvals(Some(Approvals { expire: false }))
        .with_push(setup.push)
        .with_notifications(Some(Notifications::new(notifiers, false)))
        .with_monitor(Some(Monitor::new(true)));
    Arc::new(state)
}

async fn app(dir: &TempDir, setup: Setup) -> axum::Router {
    router(state(dir, setup).await, 1 << 20)
}

/// Who sends a request.
#[derive(Clone, Copy)]
enum As<'a> {
    Nobody,
    Key,
    /// A paired device: its token.
    Device(&'a str),
}

fn request(method: &str, uri: &str, who: As<'_>, body: Option<&Json>) -> Request<Body> {
    let mut request = Request::builder().method(method).uri(uri).header("host", "10.0.0.5:8080");
    request = match who {
        As::Nobody => request,
        As::Key => request.header("authorization", format!("Bearer {API_KEY}")),
        As::Device(token) => request.header("authorization", format!("Bearer {token}")),
    };
    match body {
        Some(body) => request.header("content-type", "application/json").body(Body::from(body.to_string())).unwrap(),
        None => request.body(Body::empty()).unwrap(),
    }
}

async fn send(app: &axum::Router, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
    let response = app.clone().oneshot(request).await.expect("response");
    let (status, headers) = (response.status(), response.headers().clone());
    let body = response.into_body().collect().await.expect("body").to_bytes();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

async fn call(app: &axum::Router, method: &str, uri: &str, who: As<'_>, body: Option<&Json>) -> (StatusCode, Json) {
    let (status, _, text) = send(app, request(method, uri, who, body)).await;
    (status, serde_json::from_str(&text).unwrap_or(Json::String(text)))
}

/// Pair a device with `scopes` (every scope when empty) and return its id and token.
async fn pair(app: &axum::Router, name: &str, scopes: &[&str]) -> (String, String) {
    let body = json!({ "name": name, "scopes": scopes });
    let (status, paired) = call(app, "POST", "/v1/devices", As::Key, Some(&body)).await;
    assert_eq!(status, StatusCode::CREATED, "{paired}");
    (paired["device"]["id"].as_str().unwrap().to_owned(), paired["token"].as_str().unwrap().to_owned())
}

/// Start `workflow` in the background and wait until it is in `status`.
async fn start_until(app: &axum::Router, workflow: &str, status: &str) -> String {
    let start = json!({ "async": true });
    let (code, accepted) = call(app, "POST", &format!("/v1/workflows/{workflow}/runs"), As::Key, Some(&start)).await;
    assert_eq!(code, StatusCode::ACCEPTED, "{accepted}");
    let run_id = accepted["run_id"].as_str().unwrap().to_owned();
    wait_for_status(app, &run_id, status).await;
    run_id
}

async fn wait_for_status(app: &axum::Router, run_id: &str, status: &str) -> Json {
    let mut last = Json::Null;
    for _ in 0..400 {
        let (_, record) = call(app, "GET", &format!("/v1/runs/{run_id}"), As::Key, None).await;
        if record["status"] == status && record["active"] == false {
            return record;
        }
        last = record;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("run {run_id} never became {status}: {last}");
}

// ---- Device tokens --------------------------------------------------------------

#[tokio::test]
async fn a_device_is_paired_listed_and_revoked_with_the_api_key_only() {
    let dir = TempDir::new("pair");
    let app = app(&dir, Setup::default()).await;
    let body = json!({ "name": "Bima's phone", "expires_in_days": 30 });
    // Pairing takes the API key.
    assert_eq!(call(&app, "POST", "/v1/devices", As::Nobody, Some(&body)).await.0, StatusCode::UNAUTHORIZED);
    let (status, headers, text) = send(&app, request("POST", "/v1/devices", As::Key, Some(&body))).await;
    assert_eq!(status, StatusCode::CREATED, "{text}");
    assert_eq!(headers["cache-control"], "no-store", "the token is shown once");
    let paired: Json = serde_json::from_str(&text).unwrap();
    let (id, token) = (paired["device"]["id"].as_str().unwrap(), paired["token"].as_str().unwrap());
    assert!(token.starts_with(&format!("grd1.{id}.")), "{token}");
    assert_eq!(paired["device"]["scopes"], json!(["runs:read", "approvals:resolve"]));
    assert!(paired["device"]["expires_ms"].as_u64().unwrap() > now_ms());
    // The pairing payload names the server as devices reach it (`--public-url`).
    assert_eq!(paired["pairing"], json!({ "v": 1, "url": "https://agent.example.com", "token": token }));
    assert_eq!(paired["link"], format!("https://agent.example.com/monitor/#pair={token}"));
    // A request may name the address itself.
    let named = json!({ "name": "tablet", "url": "http://192.168.1.5:8080/" });
    let (_, tablet) = call(&app, "POST", "/v1/devices", As::Key, Some(&named)).await;
    assert_eq!(tablet["pairing"]["url"], "http://192.168.1.5:8080");
    for bad in [
        json!({ "name": "" }),
        json!({ "name": "x", "expires_in_days": 0 }),
        json!({ "name": "x", "url": "agent.example.com" }),
    ] {
        assert_eq!(call(&app, "POST", "/v1/devices", As::Key, Some(&bad)).await.0, StatusCode::BAD_REQUEST, "{bad}");
    }

    // A scope this server does not know is refused, not dropped.
    let unknown_scope = json!({ "name": "x", "scopes": ["runs:write"] });
    let (status, _) = call(&app, "POST", "/v1/devices", As::Key, Some(&unknown_scope)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // The list never shows a token or its hash.
    let (status, _, listed) = send(&app, request("GET", "/v1/devices", As::Key, None)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(listed.contains("Bima's phone") && !listed.contains("token") && !listed.contains("hash"), "{listed}");
    // A device token never pairs, lists or revokes devices.
    assert_eq!(call(&app, "GET", "/v1/devices", As::Device(token), None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(call(&app, "POST", "/v1/devices", As::Device(token), Some(&body)).await.0, StatusCode::UNAUTHORIZED);
    let other = tablet["device"]["id"].as_str().unwrap();
    let revoke_other = format!("/v1/devices/{other}");
    assert_eq!(call(&app, "DELETE", &revoke_other, As::Device(token), None).await.0, StatusCode::UNAUTHORIZED);
    // It can ask who it is.
    let (status, me) = call(&app, "GET", "/v1/devices/me", As::Device(token), None).await;
    assert_eq!((status, me["name"].as_str()), (StatusCode::OK, Some("Bima's phone")));
    assert!(me["last_used_ms"].as_u64().is_some());
    assert_eq!(call(&app, "GET", "/v1/devices/me", As::Key, None).await.0, StatusCode::BAD_REQUEST);

    // Revoked: the token stops working at once.
    assert_eq!(call(&app, "GET", "/v1/runs", As::Device(token), None).await.0, StatusCode::OK);
    assert_eq!(call(&app, "DELETE", &format!("/v1/devices/{id}"), As::Key, None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&app, "DELETE", &format!("/v1/devices/{id}"), As::Key, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&app, "GET", "/v1/runs", As::Device(token), None).await.0, StatusCode::UNAUTHORIZED);
    // A device can also unpair itself.
    let tablet_token = tablet["token"].as_str().unwrap();
    assert_eq!(call(&app, "DELETE", "/v1/devices/me", As::Device(tablet_token), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&app, "GET", "/v1/devices/me", As::Device(tablet_token), None).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_device_reads_runs_and_changes_nothing() {
    let dir = TempDir::new("read");
    let app = app(&dir, Setup::default()).await;
    let (_, token) = pair(&app, "phone", &[]).await;
    let device = As::Device(&token);
    let run_id = start_until(&app, "hello", "succeeded").await;
    let paused = start_until(&app, "refund", "paused").await;

    // Reading: the list, a run with everything in it, its events.
    let (status, runs) = call(&app, "GET", "/v1/runs", device, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(runs["runs"].as_array().unwrap().len(), 2);
    let (status, record) = call(&app, "GET", &format!("/v1/runs/{run_id}"), device, None).await;
    assert_eq!((status, record["answer"].as_str()), (StatusCode::OK, Some("hi")));
    let (status, events) = call(&app, "GET", &format!("/v1/runs/{run_id}/events?stream=false"), device, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(events.as_array().unwrap().iter().any(|event| event["type"] == "node_started"), "every event");

    // Everything else is refused: starting, stopping, resuming, deleting, the rest of the API.
    let empty = json!({});
    let resume = json!({ "inputs": { "ask": { "action": "approve" } } });
    for (method, uri, body, expected) in [
        ("POST", "/v1/workflows/hello/runs".to_owned(), Some(&empty), StatusCode::UNAUTHORIZED),
        ("POST", format!("/v1/runs/{paused}/abort"), None, StatusCode::FORBIDDEN),
        ("POST", format!("/v1/runs/{paused}/resume"), Some(&resume), StatusCode::FORBIDDEN),
        ("DELETE", format!("/v1/runs/{run_id}"), None, StatusCode::UNAUTHORIZED),
        ("GET", "/v1/workflows".to_owned(), None, StatusCode::UNAUTHORIZED),
        ("GET", "/v1/nodes".to_owned(), None, StatusCode::UNAUTHORIZED),
        ("POST", "/v1/validate".to_owned(), Some(&empty), StatusCode::UNAUTHORIZED),
        ("GET", "/v1/hooks".to_owned(), None, StatusCode::UNAUTHORIZED),
    ] {
        assert_eq!(call(&app, method, &uri, device, body).await.0, expected, "{method} {uri}");
    }
    let (_, record) = call(&app, "GET", &format!("/v1/runs/{paused}"), As::Key, None).await;
    assert_eq!(record["status"], "paused", "the refused resume changed nothing");

    // Scopes narrow a token further.
    let (_, reader) = pair(&app, "wall display", &["runs:read"]).await;
    assert_eq!(call(&app, "GET", "/v1/runs", As::Device(&reader), None).await.0, StatusCode::OK);
    let (status, refused) = call(&app, "GET", "/v1/approvals", As::Device(&reader), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert!(refused["error"].as_str().unwrap().contains("approvals:resolve"));
    let (_, approver) = pair(&app, "approver", &["approvals:resolve"]).await;
    assert_eq!(call(&app, "GET", "/v1/approvals", As::Device(&approver), None).await.0, StatusCode::OK);
    assert_eq!(call(&app, "GET", "/v1/runs", As::Device(&approver), None).await.0, StatusCode::FORBIDDEN);
    // A made-up token is not the API key.
    for uri in ["/v1/runs", "/v1/approvals"] {
        assert_eq!(call(&app, "GET", uri, As::Device("grd1.dev-00.nope"), None).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(call(&app, "GET", uri, As::Nobody, None).await.0, StatusCode::UNAUTHORIZED);
    }
}

#[tokio::test]
async fn a_server_without_devices_takes_no_device_token() {
    let dir = TempDir::new("plain");
    let app = app(&dir, Setup { plain: true, ..Setup::default() }).await;
    let device = As::Device("grd1.dev-0011223344556677.c2VjcmV0");
    assert_eq!(call(&app, "GET", "/v1/runs", device, None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(call(&app, "GET", "/v1/runs", As::Key, None).await.0, StatusCode::OK);
    // What is off does not exist, for anyone: no route of it asks for a credential.
    for uri in ["/v1/devices", "/v1/approvals", "/v1/push", "/monitor/", "/v1/monitor/config", "/v1/nothing"] {
        for who in [As::Key, As::Nobody, device] {
            assert_eq!(call(&app, "GET", uri, who, None).await.0, StatusCode::NOT_FOUND, "{uri}");
        }
    }
}

// ---- The approvals inbox --------------------------------------------------------

#[tokio::test]
async fn the_inbox_lists_what_waits_and_an_answer_lets_the_run_continue() {
    let dir = TempDir::new("inbox");
    let app = app(&dir, Setup::default()).await;
    let (_, token) = pair(&app, "phone", &[]).await;
    let device = As::Device(&token);
    start_until(&app, "hello", "succeeded").await;
    let run_id = start_until(&app, "refund", "paused").await;
    let other = start_until(&app, "strict", "paused").await;

    let (status, inbox) = call(&app, "GET", "/v1/approvals", device, None).await;
    assert_eq!(status, StatusCode::OK, "{inbox}");
    let entries = inbox["approvals"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "runs that ended are not listed: {inbox}");
    let entry = entries.iter().find(|entry| entry["run_id"] == run_id.as_str()).unwrap();
    assert_eq!(entry["workflow"], "refund");
    let asked = &entry["requests"][0];
    assert_eq!((asked["key"].as_str(), asked["kind"].as_str()), (Some("ask"), Some("human_input")));
    assert_eq!(asked["data"]["prompt"], "Refund 40 EUR to Ana?");
    assert_eq!(asked["data"]["actions"][0]["id"], "approve");
    let paused_ms = entry["paused_ms"].as_u64().unwrap();
    assert_eq!(asked["expires_ms"].as_u64(), Some(paused_ms + 86_400_000));
    // One workflow only.
    let (_, strict_only) = call(&app, "GET", "/v1/approvals?workflow=strict", device, None).await;
    assert_eq!(strict_only["approvals"].as_array().unwrap().len(), 1);
    assert_eq!(strict_only["approvals"][0]["run_id"], other.as_str());

    // An answer must be for something that waits.
    let resolve = format!("/v1/approvals/{run_id}");
    assert_eq!(call(&app, "POST", &resolve, device, Some(&json!({}))).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    for bad in [json!({ "inputs": {} }), json!({ "inputs": { "nope": { "action": "approve" } } })] {
        let (status, refused) = call(&app, "POST", &resolve, device, Some(&bad)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {refused}");
    }
    assert_eq!(call(&app, "POST", &resolve, As::Nobody, None).await.0, StatusCode::UNAUTHORIZED);
    let answer = json!({ "inputs": { "ask": { "action": "approve", "values": { "note": "ok" } } } });
    let (status, accepted) = call(&app, "POST", &resolve, device, Some(&answer)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    assert_eq!(accepted["run_id"], run_id.as_str());
    assert!(accepted["first_event"].as_u64().unwrap() > 0, "where to follow the rest of the run");
    let record = wait_for_status(&app, &run_id, "succeeded").await;
    assert_eq!(record["answer"], "refunded");

    // Answered: it left the inbox, and a second answer is refused.
    let (_, inbox) = call(&app, "GET", "/v1/approvals", As::Key, None).await;
    assert_eq!(inbox["approvals"].as_array().unwrap().len(), 1);
    assert_eq!(call(&app, "POST", &resolve, device, Some(&answer)).await.0, StatusCode::CONFLICT);
    assert_eq!(call(&app, "POST", "/v1/approvals/run-unknown", device, Some(&answer)).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_expired_request_is_answered_as_its_node_asks_or_the_run_stops() {
    let dir = TempDir::new("expire");
    let state = state(&dir, Setup::default()).await;
    let app = router(Arc::clone(&state), 1 << 20);
    let rejects = start_until(&app, "refund", "paused").await;
    let stops = start_until(&app, "strict", "paused").await;

    // Nothing is due yet.
    assert_eq!(expire_due(&state, now_ms()).await, 0);
    // `strict` gives a second, `refund` a day.
    assert_eq!(expire_due(&state, now_ms() + 60_000).await, 1);
    let stopped = wait_for_status(&app, &stops, "aborted").await;
    assert_eq!(stopped["error"], EXPIRED_REASON);
    let (_, events) = call(&app, "GET", &format!("/v1/runs/{stops}/events?stream=false"), As::Key, None).await;
    let last = events.as_array().unwrap().last().unwrap();
    assert_eq!((last["type"].as_str(), last["reason"].as_str()), (Some("run_aborted"), Some(EXPIRED_REASON)));
    let (_, record) = call(&app, "GET", &format!("/v1/runs/{rejects}"), As::Key, None).await;
    assert_eq!(record["status"], "paused");

    // A day later `refund` answers itself: the named action with the fields' defaults.
    assert_eq!(expire_due(&state, now_ms() + 2 * 86_400_000).await, 1);
    let rejected = wait_for_status(&app, &rejects, "succeeded").await;
    assert_eq!(rejected["answer"], "rejected (no answer in time)");
    assert_eq!(expire_due(&state, now_ms() + 2 * 86_400_000).await, 0, "nothing waits any more");
}

// ---- Push notifications ---------------------------------------------------------

/// One request a push service received.
#[derive(Clone, Debug)]
struct Pushed {
    path: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

/// A push service on a local port: it keeps what it receives and answers
/// `201`, or `410` for the subscription `gone`.
async fn push_service() -> (String, Arc<Mutex<Vec<Pushed>>>) {
    let received: Arc<Mutex<Vec<Pushed>>> = Arc::default();
    let sink = Arc::clone(&received);
    let service = axum::Router::new().fallback(move |request: Request<Body>| {
        let sink = Arc::clone(&sink);
        async move {
            let (parts, body) = request.into_parts();
            let body = body.collect().await.map(|body| body.to_bytes().to_vec()).unwrap_or_default();
            let path = parts.uri.path().to_owned();
            sink.lock().unwrap().push(Pushed { path: path.clone(), headers: parts.headers, body });
            if path.ends_with("/gone") { StatusCode::GONE } else { StatusCode::CREATED }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, service).await });
    (format!("http://{address}"), received)
}

/// What a browser sends to subscribe at `endpoint`: a P-256 public key and a secret.
fn subscription(endpoint: &str) -> Json {
    let random = ring::rand::SystemRandom::new();
    let private = ring::agreement::EphemeralPrivateKey::generate(&ring::agreement::ECDH_P256, &random).unwrap();
    let public = private.compute_public_key().unwrap();
    json!({
        "endpoint": endpoint,
        "keys": { "p256dh": URL_SAFE_NO_PAD.encode(public.as_ref()), "auth": URL_SAFE_NO_PAD.encode([7_u8; 16]) },
    })
}

async fn wait_for_pushes(received: &Mutex<Vec<Pushed>>, count: usize) -> Vec<Pushed> {
    for _ in 0..400 {
        let pushed = received.lock().unwrap().clone();
        if pushed.len() >= count {
            return pushed;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("expected {count} push request(s), got {:?}", received.lock().unwrap().len());
}

#[tokio::test]
async fn subscribed_browsers_hear_of_failures_and_of_runs_that_wait() {
    let dir = TempDir::new("push");
    let (service, received) = push_service().await;
    // The local push service is a private, plain http address: allowed for this test only.
    let push = Arc::new(WebPush::open(None, "mailto:ops@example.com", true).unwrap());
    let app = app(&dir, Setup { push: Some(Arc::clone(&push)), ..Setup::default() }).await;
    let (device_id, token) = pair(&app, "phone", &[]).await;
    let device = As::Device(&token);

    // The key a browser subscribes with, then its subscription.
    assert_eq!(call(&app, "GET", "/v1/push", As::Nobody, None).await.0, StatusCode::UNAUTHORIZED);
    let (status, info) = call(&app, "GET", "/v1/push", device, None).await;
    assert_eq!(status, StatusCode::OK);
    let public_key = info["public_key"].as_str().unwrap().to_owned();
    assert_eq!(URL_SAFE_NO_PAD.decode(&public_key).unwrap().len(), 65);
    let phone = format!("{service}/push/phone");
    let subscribe = subscription(&phone);
    assert_eq!(
        call(&app, "POST", "/v1/push/subscriptions", As::Nobody, Some(&subscribe)).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(call(&app, "POST", "/v1/push/subscriptions", device, Some(&subscribe)).await.0, StatusCode::CREATED);
    let mut bad = subscription(&phone);
    bad["keys"]["p256dh"] = json!("AAAA");
    assert_eq!(call(&app, "POST", "/v1/push/subscriptions", device, Some(&bad)).await.0, StatusCode::BAD_REQUEST);

    // A run that fails: one encrypted message, signed with the server's key.
    let failed = start_until(&app, "broken", "failed").await;
    let pushed = wait_for_pushes(&received, 1).await;
    let message = &pushed[0];
    assert_eq!(message.path, "/push/phone");
    assert_eq!(message.headers["content-encoding"], "aes128gcm");
    assert_eq!(message.headers["urgency"], "normal");
    assert_eq!(message.headers["ttl"], "86400");
    let authorization = message.headers["authorization"].to_str().unwrap();
    assert!(authorization.starts_with("vapid t=") && authorization.ends_with(&format!(", k={public_key}")));
    // The body is the aes128gcm header (salt, record size 4096, a 65 byte key), then ciphertext.
    assert_eq!(message.body[16..21], [0, 0, 16, 0, 65]);
    let text = String::from_utf8_lossy(&message.body);
    assert!(!text.contains("broken") && !text.contains(&failed), "the push service cannot read the message");

    // A run that waits for a person is urgent. A run that succeeds says nothing.
    start_until(&app, "hello", "succeeded").await;
    start_until(&app, "refund", "paused").await;
    let pushed = wait_for_pushes(&received, 2).await;
    assert_eq!(pushed.len(), 2);
    assert_eq!(pushed[1].headers["urgency"], "high");

    // "Send a test notification", for the device's own subscription.
    let test = json!({ "endpoint": phone });
    assert_eq!(call(&app, "POST", "/v1/push/test", device, Some(&test)).await.0, StatusCode::ACCEPTED);
    assert_eq!(wait_for_pushes(&received, 3).await.len(), 3);
    let (_, other) = pair(&app, "tablet", &[]).await;
    assert_eq!(call(&app, "POST", "/v1/push/test", As::Device(&other), Some(&test)).await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        call(&app, "DELETE", "/v1/push/subscriptions", As::Device(&other), Some(&test)).await.0,
        StatusCode::NOT_FOUND
    );

    // A subscription the push service no longer knows is dropped.
    let gone = subscription(&format!("{service}/push/gone"));
    assert_eq!(call(&app, "POST", "/v1/push/subscriptions", As::Key, Some(&gone)).await.0, StatusCode::CREATED);
    assert_eq!(push.subscription_count(), 2);
    start_until(&app, "broken", "failed").await;
    wait_for_pushes(&received, 5).await;
    for _ in 0..200 {
        if push.subscription_count() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(push.subscription_count(), 1);

    // Revoking the device ends its subscriptions too.
    assert_eq!(
        call(&app, "DELETE", &format!("/v1/devices/{device_id}"), As::Key, None).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(push.subscription_count(), 0);
}

#[tokio::test]
async fn a_push_endpoint_must_be_a_public_https_address() {
    let dir = TempDir::new("push-endpoint");
    let push = Arc::new(WebPush::open(None, "mailto:ops@example.com", false).unwrap());
    let app = app(&dir, Setup { push: Some(push), ..Setup::default() }).await;
    for endpoint in ["http://push.example.net/a", "https://127.0.0.1/a", "https://10.0.0.8/a", "file:///etc/passwd"] {
        let (status, refused) =
            call(&app, "POST", "/v1/push/subscriptions", As::Key, Some(&subscription(endpoint))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{endpoint}: {refused}");
    }
    let public = subscription("https://push.example.net/a");
    assert_eq!(call(&app, "POST", "/v1/push/subscriptions", As::Key, Some(&public)).await.0, StatusCode::CREATED);
    let remove = json!({ "endpoint": "https://push.example.net/a" });
    assert_eq!(call(&app, "DELETE", "/v1/push/subscriptions", As::Key, Some(&remove)).await.0, StatusCode::NO_CONTENT);
}

// ---- The monitoring page --------------------------------------------------------

#[tokio::test]
async fn the_monitoring_page_is_served_with_a_strict_policy() {
    let dir = TempDir::new("page");
    let app = app(&dir, Setup::default()).await;
    let (status, headers, _) = send(&app, request("GET", "/monitor", As::Nobody, None)).await;
    assert_eq!(status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(headers["location"], "monitor/");
    let (status, headers, _) = send(&app, request("GET", "/monitor/", As::Nobody, None)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers["content-type"].to_str().unwrap().starts_with("text/html"));
    let policy = headers["content-security-policy"].to_str().unwrap();
    for part in ["script-src 'self'", "connect-src 'self'", "worker-src 'self'", "frame-ancestors 'none'"] {
        assert!(policy.contains(part), "{policy}");
    }
    assert!(!policy.contains("unsafe"), "{policy}");
    assert_eq!(headers["referrer-policy"], "no-referrer");
    for (file, kind) in [
        ("monitor.js", "text/javascript"),
        ("sw.js", "text/javascript"),
        ("manifest.webmanifest", "application/manifest+json"),
        ("icon-192.png", "image/png"),
        ("icon-512.png", "image/png"),
        ("icon-maskable.png", "image/png"),
        ("apple-touch-icon.png", "image/png"),
    ] {
        let uri = format!("/monitor/{file}");
        let (status, headers, _) = send(&app, request("GET", &uri, As::Nobody, None)).await;
        assert_eq!(status, StatusCode::OK, "{file}");
        assert!(headers["content-type"].to_str().unwrap().starts_with(kind), "{file}");
        assert_eq!(headers["x-content-type-options"], "nosniff");
        let again = Request::get(&uri).header("if-none-match", &headers["etag"]).body(Body::empty()).unwrap();
        assert_eq!(send(&app, again).await.0, StatusCode::NOT_MODIFIED, "{file}");
    }
    assert_eq!(send(&app, request("GET", "/monitor/secrets.txt", As::Nobody, None)).await.0, StatusCode::NOT_FOUND);
    let (status, config) = call(&app, "GET", "/v1/monitor/config", As::Nobody, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(config, json!({ "access": "key", "devices": true, "approvals": true, "push": false }));
}
