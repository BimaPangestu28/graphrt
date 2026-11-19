//! Webhook triggers (`--webhooks`): mapping, signatures, idempotency, body limits
//! and signed completion callbacks.
#![cfg(feature = "serve")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use graphrt_cli::serve::{AppState, build_state, discover, router};
use graphrt_cli::setup::ServiceOptions;
use graphrt_cli::store::{FileStore, MemoryStore};
use graphrt_cli::webhooks::Webhooks;
use graphrt_cli::webhooks::signature::hmac_sha256;
use graphrt_core::engine::Limits;
use graphrt_core::store::RunStore;
use http_body_util::BodyExt;
use serde_json::{Value as Json, json};
use tower::ServiceExt;

const GITHUB_SECRET: &str = "gh-webhook-secret";
const STRIPE_SECRET: &str = "whsec_test";

/// A temp directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("graphrt-webhooks-{tag}-{}", std::process::id()));
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

/// `hello` (one `name` input) and `echo` (one `body` JSON input).
fn workflows(dir: &TempDir) -> PathBuf {
    let flows = dir.path().join("flows");
    std::fs::create_dir_all(&flows).unwrap();
    let examples = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    std::fs::copy(examples.join("hello.yaml"), flows.join("hello.yaml")).unwrap();
    std::fs::write(
        flows.join("echo.yaml"),
        r#"version: "1"
name: echo
graph:
  nodes:
    - id: start
      type: start
      variables:
        - { variable: body, type: json, required: true }
    - id: say
      type: template-transform
      template: "{{ body.action }} by {{ body.sender.login }}"
      variables:
        - { variable: body, value_selector: [start, body] }
    - id: end
      type: end
      outputs:
        - { variable: text, value_selector: [say, output] }
  edges:
    - { source: start, target: say }
    - { source: say, target: end }
"#,
    )
    .unwrap();
    flows
}

fn vars() -> Vec<(String, String)> {
    vec![
        ("GITHUB_SECRET".to_owned(), GITHUB_SECRET.to_owned()),
        ("STRIPE_SECRET".to_owned(), STRIPE_SECRET.to_owned()),
        ("CALLBACK_SECRET".to_owned(), "cb-secret".to_owned()),
    ]
}

fn hooks_config() -> Json {
    json!({ "hooks": {
        "echo": { "workflow": "echo" },
        "greet": {
            "workflow": "hello",
            "inputs": { "name": "/sender/login" },
            "headers": { "excited": "X-Excited" },
        },
        "github": {
            "workflow": "echo",
            "body_input": "body",
            "signature": { "scheme": "github", "secret_env": "GITHUB_SECRET" },
            "idempotency_header": "X-GitHub-Delivery",
        },
        "stripe": {
            "workflow": "echo",
            "signature": { "scheme": "stripe", "secret_env": "STRIPE_SECRET" },
        },
    } })
}

async fn state_with(
    flows: &Path,
    hooks: Json,
    store: Arc<dyn RunStore>,
    api_key: Option<&str>,
    allow_private: bool,
) -> Arc<AppState> {
    let files = discover(flows).unwrap();
    let options = ServiceOptions {
        plugins: graphrt_cli::extensions::PluginOptions { dirs: Vec::new(), no_default: true, trusted: Vec::new() },
        allow_private_network: allow_private,
        ..ServiceOptions::default()
    };
    let webhooks = Webhooks::from_value(hooks, &vars(), allow_private).unwrap();
    let state = build_state(&files, &options, Limits::default(), api_key.map(str::to_owned))
        .await
        .unwrap()
        .with_store(store)
        .with_webhooks(webhooks)
        .unwrap();
    Arc::new(state)
}

async fn app(dir: &TempDir) -> axum::Router {
    let flows = workflows(dir);
    router(state_with(&flows, hooks_config(), Arc::new(MemoryStore::new()), None, false).await, 1 << 20)
}

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, Json) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or_else(|_| Json::String(String::from_utf8_lossy(&body).into())))
}

fn delivery(uri: &str, body: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::post(uri).header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::from(body.to_owned())).unwrap()
}

fn github_signature(body: &str) -> String {
    let mac = hmac_sha256(GITHUB_SECRET.as_bytes(), &[body.as_bytes()]);
    format!("sha256={}", graphrt_cli::hex::to_hex(&mac))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

/// Poll `GET /v1/runs/{id}` until the run left `running`.
async fn settled(app: &axum::Router, run_id: &str) -> Json {
    for _ in 0..500 {
        let (status, run) = send(app, Request::get(format!("/v1/runs/{run_id}")).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK, "{run}");
        if run["status"] != "running" && run["active"] == false {
            return run;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("run {run_id} did not settle");
}

#[tokio::test]
async fn whole_body_becomes_one_input_and_returns_202() {
    let dir = TempDir::new("body");
    let app = app(&dir).await;
    let body = r#"{"action":"opened","sender":{"login":"ada"}}"#;
    let (status, accepted) = send(&app, delivery("/v1/hooks/echo", body, &[])).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    assert_eq!(accepted["status"], "running");
    assert_eq!(accepted["duplicate"], false);
    let run_id = accepted["run_id"].as_str().unwrap();
    assert_eq!(accepted["run"], format!("/v1/runs/{run_id}"));
    let run = settled(&app, run_id).await;
    assert_eq!(run["status"], "succeeded", "{run}");
    assert_eq!(run["workflow"], "echo");
    assert_eq!(run["outputs"]["text"], "opened by ada");
}

#[tokio::test]
async fn pointer_and_header_mapping() {
    let dir = TempDir::new("mapping");
    let app = app(&dir).await;
    let body = r#"{"sender":{"login":"Grace"},"ignored":true}"#;
    let (status, accepted) = send(&app, delivery("/v1/hooks/greet", body, &[("x-excited", "true")])).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    let run = settled(&app, accepted["run_id"].as_str().unwrap()).await;
    assert_eq!(run["outputs"]["greeting"], "Hello, Grace!!!", "{run}");
}

#[tokio::test]
async fn github_signature_is_verified() {
    let dir = TempDir::new("github");
    let app = app(&dir).await;
    let body = r#"{"action":"closed","sender":{"login":"linus"}}"#;

    let (status, error) = send(&app, delivery("/v1/hooks/github", body, &[])).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(error["error"].as_str().unwrap().contains("missing signature header"), "{error}");

    let forged = format!("sha256={}", graphrt_cli::hex::to_hex(&hmac_sha256(b"wrong", &[body.as_bytes()])));
    let (status, error) = send(&app, delivery("/v1/hooks/github", body, &[("x-hub-signature-256", &forged)])).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(error["error"], "signature does not match");

    // A valid signature over a different body does not verify this one.
    let other = github_signature("{}");
    let (status, _) = send(&app, delivery("/v1/hooks/github", body, &[("x-hub-signature-256", &other)])).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let signature = github_signature(body);
    let (status, accepted) =
        send(&app, delivery("/v1/hooks/github", body, &[("x-hub-signature-256", &signature)])).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    let run = settled(&app, accepted["run_id"].as_str().unwrap()).await;
    assert_eq!(run["outputs"]["text"], "closed by linus");
}

#[tokio::test]
async fn stripe_signature_with_timestamp() {
    let dir = TempDir::new("stripe");
    let app = app(&dir).await;
    let body = r#"{"action":"paid","sender":{"login":"acct_1"}}"#;
    let now = now_secs();
    let mac = hmac_sha256(STRIPE_SECRET.as_bytes(), &[now.to_string().as_bytes(), b".", body.as_bytes()]);
    let header = format!("t={now},v1={}", graphrt_cli::hex::to_hex(&mac));
    let (status, accepted) = send(&app, delivery("/v1/hooks/stripe", body, &[("stripe-signature", &header)])).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");

    // The same signature an hour old is a replay.
    let old = now - 3600;
    let mac = hmac_sha256(STRIPE_SECRET.as_bytes(), &[old.to_string().as_bytes(), b".", body.as_bytes()]);
    let header = format!("t={old},v1={}", graphrt_cli::hex::to_hex(&mac));
    let (status, error) = send(&app, delivery("/v1/hooks/stripe", body, &[("stripe-signature", &header)])).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(error["error"].as_str().unwrap().contains("outside the allowed window"), "{error}");
}

#[tokio::test]
async fn idempotency_key_returns_the_first_run() {
    let dir = TempDir::new("idempotent");
    let app = app(&dir).await;
    let body = r#"{"action":"push","sender":{"login":"ada"}}"#;
    let signature = github_signature(body);
    let headers = [("x-hub-signature-256", signature.as_str()), ("x-github-delivery", "delivery-1")];
    let (status, first) = send(&app, delivery("/v1/hooks/github", body, &headers)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{first}");
    assert_eq!(first["duplicate"], false);
    let run_id = first["run_id"].as_str().unwrap().to_owned();
    settled(&app, &run_id).await;

    let (status, again) = send(&app, delivery("/v1/hooks/github", body, &headers)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{again}");
    assert_eq!(again["run_id"], run_id.as_str());
    assert_eq!(again["duplicate"], true);
    assert_eq!(again["status"], "succeeded");

    let other = [("x-hub-signature-256", signature.as_str()), ("x-github-delivery", "delivery-2")];
    let (_, third) = send(&app, delivery("/v1/hooks/github", body, &other)).await;
    assert_ne!(third["run_id"], run_id.as_str());

    // The default header is `Idempotency-Key`; an empty key is rejected.
    let (status, _) = send(&app, delivery("/v1/hooks/echo", body, &[("idempotency-key", " ")])).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, listed) = send(&app, Request::get("/v1/runs").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["runs"].as_array().unwrap().len(), 2, "one run per distinct key: {listed}");
}

#[tokio::test]
async fn idempotency_survives_a_restart_with_a_durable_store() {
    let dir = TempDir::new("durable");
    let flows = workflows(&dir);
    let store_dir = dir.path().join("runs");
    let body = r#"{"action":"push","sender":{"login":"ada"}}"#;
    let headers = [("idempotency-key", "evt_42")];

    let store: Arc<dyn RunStore> = Arc::new(FileStore::open(&store_dir).unwrap());
    let first_app = router(state_with(&flows, hooks_config(), store, None, false).await, 1 << 20);
    let (_, first) = send(&first_app, delivery("/v1/hooks/echo", body, &headers)).await;
    settled(&first_app, first["run_id"].as_str().unwrap()).await;
    drop(first_app);

    let store: Arc<dyn RunStore> = Arc::new(FileStore::open(&store_dir).unwrap());
    let restarted = router(state_with(&flows, hooks_config(), store, None, false).await, 1 << 20);
    let (status, again) = send(&restarted, delivery("/v1/hooks/echo", body, &headers)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(again["duplicate"], true);
    assert_eq!(again["run_id"], first["run_id"]);
}

#[tokio::test]
async fn body_limit_bad_json_and_unknown_hooks() {
    let dir = TempDir::new("limits");
    let flows = workflows(&dir);
    let app = router(state_with(&flows, hooks_config(), Arc::new(MemoryStore::new()), None, false).await, 64);
    let big = format!(r#"{{"action":"{}","sender":{{"login":"x"}}}}"#, "a".repeat(200));
    let (status, _) = send(&app, delivery("/v1/hooks/echo", &big, &[])).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let (status, error) = send(&app, delivery("/v1/hooks/echo", "not json", &[])).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error["error"].as_str().unwrap().contains("not valid JSON"));
    let (status, _) = send(&app, delivery("/v1/hooks/nope", "{}", &[])).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unsigned_hooks_need_the_api_key_signed_ones_do_not() {
    let dir = TempDir::new("auth");
    let flows = workflows(&dir);
    let state = state_with(&flows, hooks_config(), Arc::new(MemoryStore::new()), Some("api-key"), false).await;
    let app = router(state, 1 << 20);
    let body = r#"{"action":"a","sender":{"login":"b"}}"#;

    let (status, _) = send(&app, delivery("/v1/hooks/echo", body, &[])).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = send(&app, delivery("/v1/hooks/echo", body, &[("authorization", "Bearer api-key")])).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let signature = github_signature(body);
    let (status, _) = send(&app, delivery("/v1/hooks/github", body, &[("x-hub-signature-256", &signature)])).await;
    assert_eq!(status, StatusCode::ACCEPTED, "the signature authenticates the sender");

    // The hook listing is part of the authenticated API and shows no secrets.
    let (status, _) = send(&app, Request::get("/v1/hooks").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let listing = Request::get("/v1/hooks").header("authorization", "Bearer api-key").body(Body::empty()).unwrap();
    let (status, hooks) = send(&app, listing).await;
    assert_eq!(status, StatusCode::OK);
    let github = hooks["hooks"].as_array().unwrap().iter().find(|h| h["id"] == "github").unwrap().clone();
    assert_eq!(github["signature"], "github");
    assert_eq!(github["idempotency_header"], "x-github-delivery");
    assert!(!hooks.to_string().contains(GITHUB_SECRET));
}

#[tokio::test]
async fn hooks_must_name_served_workflows() {
    let dir = TempDir::new("unknown-wf");
    let flows = workflows(&dir);
    let files = discover(&flows).unwrap();
    let options = ServiceOptions {
        plugins: graphrt_cli::extensions::PluginOptions { dirs: Vec::new(), no_default: true, trusted: Vec::new() },
        ..ServiceOptions::default()
    };
    let webhooks = Webhooks::from_value(json!({ "hooks": { "h": { "workflow": "missing" } } }), &[], false).unwrap();
    let state = build_state(&files, &options, Limits::default(), None).await.unwrap();
    let error = state.with_webhooks(webhooks).unwrap_err();
    assert!(error.to_string().contains("`missing`"), "{error}");
}

/// Completion callbacks need the `http` feature (egress policy client).
#[cfg(feature = "http")]
mod callbacks {
    use graphrt_cli::webhooks::signature::sign_graphrt;

    use super::*;

    /// A request received by [`CallbackReceiver`].
    #[derive(Clone, Debug)]
    struct Received {
        headers: axum::http::HeaderMap,
        body: Vec<u8>,
    }

    /// A local HTTP server answering callbacks with the queued statuses (then 200).
    struct CallbackReceiver {
        url: String,
        received: Arc<tokio::sync::Mutex<Vec<Received>>>,
    }

    impl CallbackReceiver {
        async fn start(statuses: Vec<u16>) -> Self {
            let received = Arc::new(tokio::sync::Mutex::new(Vec::<Received>::new()));
            let statuses = Arc::new(tokio::sync::Mutex::new(statuses));
            let sink = Arc::clone(&received);
            let app = axum::Router::new().route(
                "/done",
                axum::routing::post(move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                    let sink = Arc::clone(&sink);
                    let statuses = Arc::clone(&statuses);
                    async move {
                        sink.lock().await.push(Received { headers, body: body.to_vec() });
                        let mut statuses = statuses.lock().await;
                        let code = if statuses.is_empty() { 200 } else { statuses.remove(0) };
                        StatusCode::from_u16(code).unwrap()
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            Self { url: format!("http://{addr}/done"), received }
        }

        async fn wait_for(&self, count: usize) -> Vec<Received> {
            for _ in 0..500 {
                let received = self.received.lock().await.clone();
                if received.len() >= count {
                    return received;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("expected {count} callback request(s)");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn completion_callback_is_signed_and_retried() {
        let receiver = CallbackReceiver::start(vec![500]).await;
        let dir = TempDir::new("callback");
        let flows = workflows(&dir);
        let hooks = json!({ "hooks": { "echo": {
        "workflow": "echo",
        "callback": { "url": receiver.url, "secret_env": "CALLBACK_SECRET", "backoff_ms": 20 },
    } } });
        let app = router(state_with(&flows, hooks, Arc::new(MemoryStore::new()), None, true).await, 1 << 20);
        let (status, accepted) =
            send(&app, delivery("/v1/hooks/echo", r#"{"action":"built","sender":{"login":"ci"}}"#, &[])).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
        let run_id = accepted["run_id"].as_str().unwrap();

        // The first attempt gets a 500 and is retried with the same delivery id.
        let received = receiver.wait_for(2).await;
        let (first, second) = (&received[0], &received[1]);
        assert_eq!(first.headers["x-graphrt-attempt"], "1");
        assert_eq!(second.headers["x-graphrt-attempt"], "2");
        assert_eq!(first.headers["x-graphrt-delivery"], second.headers["x-graphrt-delivery"]);
        assert_eq!(second.headers["x-graphrt-delivery"].to_str().unwrap(), format!("{run_id}.succeeded"));
        assert_eq!(second.headers["x-graphrt-event"], "run.succeeded");

        let payload: Json = serde_json::from_slice(&second.body).unwrap();
        assert_eq!(payload["type"], "run.succeeded");
        assert_eq!(payload["run_id"], run_id);
        assert_eq!(payload["hook"], "echo");
        assert_eq!(payload["workflow"], "echo");
        assert_eq!(payload["outputs"]["text"], "built by ci");

        // Receivers verify `t=<unix>,v1=hex(hmac("<t>.<body>"))`, e.g. with a graphrt
        // hook using `scheme: graphrt`.
        let signature = second.headers["x-graphrt-signature"].to_str().unwrap();
        let timestamp: u64 = signature.strip_prefix("t=").unwrap().split(',').next().unwrap().parse().unwrap();
        assert_eq!(signature, sign_graphrt(b"cb-secret", timestamp, &second.body));
        let verifier = graphrt_cli::webhooks::Verifier::new(
            graphrt_cli::webhooks::SignatureScheme::Graphrt,
            b"cb-secret".to_vec(),
            None,
            None,
        );
        assert!(verifier.verify(&second.headers, &second.body, now_secs()).is_ok());

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(receiver.received.lock().await.len(), 2, "no further attempts after a 2xx");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn callback_gives_up_on_client_errors_and_filters_statuses() {
        let receiver = CallbackReceiver::start(vec![400]).await;
        let dir = TempDir::new("callback-400");
        let flows = workflows(&dir);
        let hooks = json!({ "hooks": {
        "echo": { "workflow": "echo",
                  "callback": { "url": receiver.url, "secret_env": "CALLBACK_SECRET", "backoff_ms": 10 } },
        "only-failures": { "workflow": "echo",
                  "callback": { "url": receiver.url, "secret_env": "CALLBACK_SECRET", "on": ["failed"] } },
    } });
        let app = router(state_with(&flows, hooks, Arc::new(MemoryStore::new()), None, true).await, 1 << 20);
        let body = r#"{"action":"x","sender":{"login":"y"}}"#;
        let (_, accepted) = send(&app, delivery("/v1/hooks/echo", body, &[])).await;
        receiver.wait_for(1).await;
        let (_, filtered) = send(&app, delivery("/v1/hooks/only-failures", body, &[])).await;
        settled(&app, accepted["run_id"].as_str().unwrap()).await;
        settled(&app, filtered["run_id"].as_str().unwrap()).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            receiver.received.lock().await.len(),
            1,
            "400 is final; a succeeded run is not sent to `on: [failed]`"
        );
    }

    #[test]
    fn callbacks_go_through_the_egress_policy() {
        let hooks = json!({ "hooks": { "h": { "workflow": "echo",
        "callback": { "url": "http://10.0.0.5/done", "secret_env": "CALLBACK_SECRET" } } } });
        let error = Webhooks::from_value(hooks.clone(), &vars(), false).unwrap_err();
        assert!(error.contains("not a public address"), "{error}");
        assert!(Webhooks::from_value(hooks, &vars(), true).is_ok(), "`--allow-private-network` opts out");
        let unsigned =
            json!({ "hooks": { "h": { "workflow": "echo", "callback": { "url": "https://example.com/x" } } } });
        assert!(Webhooks::from_value(unsigned, &vars(), false).unwrap_err().contains("secret"));
    }
}
