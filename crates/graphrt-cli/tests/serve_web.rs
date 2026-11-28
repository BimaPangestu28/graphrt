//! What a browser needs from `graphrt serve`: a user follows, stops and (when
//! allowed) resumes their own runs, conversations kept by the server, and the
//! chat page with guest sessions. Everything a user reaches is their own.
#![cfg(all(feature = "web-chat", feature = "conversations", feature = "scores"))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use graphrt_cli::args::{ChatAccess, ChatArgs};
use graphrt_cli::conversations::{Conversations, Limits};
use graphrt_cli::header_inputs::HeaderInputSpec;
use graphrt_cli::serve::{Chat, ChatContext, build_state, discover, router};
use graphrt_cli::setup::ServiceOptions;
use graphrt_cli::user_auth::UserAuth;
use graphrt_cli::user_auth::token::{ClaimRules, VerifyKey};
use graphrt_core::engine::Limits as EngineLimits;
use http_body_util::BodyExt;
use serde_json::{Value as Json, json};
use tower::ServiceExt;

const SECRET: &[u8] = b"test-jwt-secret-with-at-least-32-bytes";
const HEADER: &str = "X-User-Authorization";
const API_KEY: &str = "server-only-key";

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("graphrt-web-{tag}-{}", std::process::id()));
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

/// `chat` and `confirm` are built for signed-in users (they take the verified
/// token), `support` is offered to chat guests, `internal` to nobody but the key.
/// `shop` answers with the page context (`sys.context`) and the user.
fn write_workflows(dir: &TempDir) -> PathBuf {
    let flows = dir.path().join("flows");
    std::fs::create_dir_all(&flows).unwrap();
    std::fs::write(
        flows.join("chat.yaml"),
        r#"version: "1"
name: chat
conversation:
  - { name: turns, type: number, value: 0 }
graph:
  nodes:
    - id: start
      type: start
      variables:
        - { variable: user_jwt, type: secret, required: false }
    - id: count
      type: assigner
      items:
        - { variable_selector: [conversation, turns], operation: "+=", input_type: constant, value: 1 }
    - id: turns
      type: template-transform
      template: "{{ turns }}"
      variables:
        - { variable: turns, value_selector: [conversation, turns] }
    - id: done
      type: answer
      answer: "turn {{#turns.output#}}: {{#sys.query#}} after {{#sys.history#}}"
  edges:
    - { source: start, target: count }
    - { source: count, target: turns }
    - { source: turns, target: done }
"#,
    )
    .unwrap();
    std::fs::write(
        flows.join("confirm.yaml"),
        r#"version: "1"
name: confirm
graph:
  nodes:
    - id: start
      type: start
      variables:
        - { variable: user_jwt, type: secret, required: false }
    - id: ask
      type: human-input
      prompt: "Send it?"
      actions: [{ id: "yes", title: "Yes" }]
    - { id: done, type: answer, answer: "sent" }
  edges:
    - { source: start, target: ask }
    - { source: ask, target: done, handle: "yes" }
"#,
    )
    .unwrap();
    std::fs::write(
        flows.join("support.yaml"),
        r#"version: "1"
name: support
graph:
  nodes:
    - id: start
      type: start
      variables:
        - { variable: message, type: text-input, required: true }
    - { id: done, type: answer, answer: "You said {{#start.message#}} as {{#sys.user_id#}}" }
  edges:
    - { source: start, target: done }
"#,
    )
    .unwrap();
    std::fs::write(
        flows.join("shop.yaml"),
        r#"version: "1"
name: shop
graph:
  nodes:
    - id: start
      type: start
      variables:
        - { variable: user_jwt, type: secret, required: false }
    - { id: done, type: answer, answer: "{{#sys.query#}} about {{#sys.context.product_id#}} for {{#sys.user_id#}}" }
  edges:
    - { source: start, target: done }
"#,
    )
    .unwrap();
    let examples = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    std::fs::copy(examples.join("hello.yaml"), flows.join("internal.yaml")).unwrap();
    flows
}

/// How the server under test is set up.
#[derive(Default)]
struct Setup {
    user_resume: bool,
    conversations: Option<Limits>,
    chat: Option<ChatAccess>,
    guest_rate_limit: u32,
    /// A file for `--chat-mermaid`.
    mermaid: Option<PathBuf>,
    /// `--chat` values; empty: `support=message`.
    chat_specs: Vec<&'static str>,
}

async fn app(dir: &TempDir, setup: Setup) -> axum::Router {
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
    let specs: Vec<HeaderInputSpec> = vec![format!("user_jwt={HEADER}").parse().unwrap()];
    let rules = ClaimRules { issuer: None, audience: None, leeway_secs: 30 };
    let user_auth = UserAuth::with_keys(&specs[0], vec![VerifyKey::hs256(SECRET)], rules, 0);
    let conversations = setup.conversations.map(|limits| Conversations::open(None, limits).expect("conversations"));
    let state = build_state(&files, &options, EngineLimits::default(), Some(API_KEY.into()))
        .await
        .expect("state")
        .with_header_inputs(&specs)
        .expect("header inputs")
        .with_user_auth(Some(user_auth))
        .with_user_resume(setup.user_resume)
        .with_conversations(conversations);
    let chat = setup.chat.map(|access| {
        let specs = if setup.chat_specs.is_empty() { vec!["support=message"] } else { setup.chat_specs.clone() };
        let args = ChatArgs {
            chat: specs.into_iter().map(str::to_owned).collect(),
            chat_access: Some(access),
            chat_title: Some("Support".to_owned()),
            chat_guest_rate_limit: Some(setup.guest_rate_limit),
            chat_guest_budget: Some(0),
            chat_mermaid: setup.mermaid.clone(),
        };
        let context = ChatContext {
            api_key: Some(API_KEY),
            user_token_header: Some(HEADER.to_ascii_lowercase()),
            frame_origins: vec!["https://shop.example".to_owned()],
        };
        Chat::new(&args, context, |name| state.chat_start_inputs(name)).expect("chat").expect("a chat page")
    });
    let state = state.with_chat(chat);
    router(Arc::new(state), 1 << 20)
}

fn token_for(subject: &str) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let header = URL_SAFE_NO_PAD.encode(json!({"alg": "HS256", "typ": "JWT"}).to_string());
    let claims = json!({ "sub": subject, "exp": now + 3600 });
    let input = format!("{header}.{}", URL_SAFE_NO_PAD.encode(claims.to_string()));
    let tag = ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, SECRET), input.as_bytes());
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(tag.as_ref()))
}

/// Who sends a request.
#[derive(Clone, Copy)]
enum As<'a> {
    Nobody,
    Key,
    /// A signed-in user: their JWT in the mapped header.
    User(&'a str),
    /// Any bearer token: a guest token, or a wrong one.
    Bearer(&'a str),
}

fn request(method: &str, uri: &str, who: As<'_>, body: Option<&Json>) -> Request<Body> {
    let mut request = Request::builder().method(method).uri(uri);
    request = match who {
        As::Nobody => request,
        As::Key => request.header("authorization", format!("Bearer {API_KEY}")),
        As::User(token) => request.header(HEADER, format!("Bearer {token}")),
        As::Bearer(token) => request.header("authorization", format!("Bearer {token}")),
    };
    match body {
        Some(body) => request.header("content-type", "application/json").body(Body::from(body.to_string())).unwrap(),
        None => request.body(Body::empty()).unwrap(),
    }
}

async fn send(app: &axum::Router, request: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
    let response = app.clone().oneshot(request).await.expect("response");
    let (status, headers) = (response.status(), response.headers().clone());
    let body = response.into_body().collect().await.expect("body").to_bytes();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

async fn call(app: &axum::Router, method: &str, uri: &str, who: As<'_>, body: Option<&Json>) -> (StatusCode, Json) {
    let (status, _, text) = send(app, request(method, uri, who, body)).await;
    (status, serde_json::from_str(&text).unwrap_or(Json::String(text)))
}

/// The `(event, data)` pairs of an SSE body.
fn sse_events(body: &str) -> Vec<(String, Json)> {
    body.split("\n\n")
        .filter_map(|block| {
            let field = |name: &str| block.lines().find_map(|line| line.strip_prefix(name)).map(str::trim);
            Some((field("event:")?.to_owned(), serde_json::from_str(field("data:")?).ok()?))
        })
        .collect()
}

// ---- A user's own runs -------------------------------------------------------

#[tokio::test]
async fn a_user_reads_and_follows_their_own_run_and_nobody_elses() {
    let dir = TempDir::new("follow");
    let app = app(&dir, Setup::default()).await;
    let (alice, bob) = (token_for("alice"), token_for("bob"));
    let start = json!({ "sys": { "query": "hello" }, "async": true });
    let (status, accepted) = call(&app, "POST", "/v1/workflows/chat/runs", As::User(&alice), Some(&start)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    let run = format!("/v1/runs/{}", accepted["run_id"].as_str().unwrap());
    // The events end with the terminal one, whether the run is still executing or not.
    let (status, _, events) = send(&app, request("GET", &format!("{run}/events"), As::User(&alice), None)).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    let events = sse_events(&events);
    let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names.first(), Some(&"run_started"), "{names:?}");
    assert_eq!(names.last(), Some(&"run_succeeded"), "{names:?}");
    // The answer text arrives as `response` events, and nothing internal does.
    let text: String =
        events.iter().filter(|(name, _)| name == "response").filter_map(|(_, data)| data["text"].as_str()).collect();
    assert_eq!(text, "turn 1: hello after ");
    assert!(names.iter().all(|name| ["run_started", "response", "run_succeeded"].contains(name)), "{names:?}");

    let (status, record) = call(&app, "GET", &run, As::User(&alice), None).await;
    assert_eq!(status, StatusCode::OK, "{record}");
    assert_eq!(record["status"], "succeeded");
    assert_eq!(record["answer"], "turn 1: hello after ");
    for hidden in ["user_id", "fingerprint", "metadata", "inputs", "error"] {
        assert!(record.get(hidden).is_none(), "`{hidden}` reached the user: {record}");
    }
    // The stored events as JSON are cut down the same way.
    let (status, stored) = call(&app, "GET", &format!("{run}/events?stream=false"), As::User(&alice), None).await;
    assert_eq!(status, StatusCode::OK);
    let kinds: Vec<&str> = stored.as_array().unwrap().iter().filter_map(|event| event["type"].as_str()).collect();
    assert_eq!(kinds, ["run_started", "run_succeeded"]);

    // Another user, and nobody, get nothing: the run looks unknown to another user.
    for uri in [run.clone(), format!("{run}/events")] {
        assert_eq!(call(&app, "GET", &uri, As::User(&bob), None).await.0, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(call(&app, "GET", &uri, As::Nobody, None).await.0, StatusCode::UNAUTHORIZED, "{uri}");
    }
    assert_eq!(call(&app, "POST", &format!("{run}/abort"), As::User(&bob), None).await.0, StatusCode::NOT_FOUND);
    // The owner may stop their run; this one has finished already.
    let (status, body) = call(&app, "POST", &format!("{run}/abort"), As::User(&alice), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no active run with this id");
    // Lists and deletes stay with the key.
    assert_eq!(call(&app, "GET", "/v1/runs", As::User(&alice), None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(call(&app, "DELETE", &run, As::User(&alice), None).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_run_started_with_the_key_is_not_a_users_even_when_it_names_them() {
    let dir = TempDir::new("label");
    let app = app(&dir, Setup::default()).await;
    let alice = token_for("alice");
    let start = json!({ "user_id": "alice", "sys": { "query": "hi" } });
    let (status, result) = call(&app, "POST", "/v1/workflows/chat/runs", As::Key, Some(&start)).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let run = format!("/v1/runs/{}", result["run_id"].as_str().unwrap());
    assert_eq!(call(&app, "GET", &run, As::User(&alice), None).await.0, StatusCode::NOT_FOUND);
    // The key reads every run in full.
    let (status, record) = call(&app, "GET", &run, As::Key, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(record["user_id"], "alice");
}

#[tokio::test]
async fn the_key_gets_response_text_on_request_only() {
    let dir = TempDir::new("responses");
    let app = app(&dir, Setup::default()).await;
    let start = json!({ "inputs": { "message": "hi" } });
    let (_, result) = call(&app, "POST", "/v1/workflows/support/runs", As::Key, Some(&start)).await;
    assert_eq!(result["answer"], "You said hi as ");
    let events = format!("/v1/runs/{}/events", result["run_id"].as_str().unwrap());
    let (_, _, plain) = send(&app, request("GET", &events, As::Key, None)).await;
    assert!(sse_events(&plain).iter().all(|(name, _)| name != "response"));
    let (_, _, with_text) = send(&app, request("GET", &format!("{events}?responses=true"), As::Key, None)).await;
    let all = sse_events(&with_text);
    let position = all.iter().position(|(name, _)| name == "response").expect("a response event");
    assert_eq!(all[position].1["text"], result["answer"]);
    assert_eq!(position, all.len() - 2, "right before the terminal event");
    // A stream that does not start at the first event has no stored answer to repeat.
    let (_, _, later) = send(&app, request("GET", &format!("{events}?responses=true&from=1"), As::Key, None)).await;
    assert!(sse_events(&later).iter().all(|(name, _)| name != "response"));
}

#[tokio::test]
async fn users_resume_their_own_runs_only_when_the_operator_allows_it() {
    let answer = json!({ "inputs": { "ask": { "action": "yes" } } });
    let alice = token_for("alice");
    let paused_run = |app: axum::Router, token: String| async move {
        let (status, result) =
            call(&app, "POST", "/v1/workflows/confirm/runs", As::User(&token), Some(&json!({}))).await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["status"], "paused");
        assert!(result.get("snapshot").is_none(), "the snapshot reached the user");
        format!("/v1/runs/{}", result["run_id"].as_str().unwrap())
    };

    let closed_dir = TempDir::new("resume-closed");
    let closed = app(&closed_dir, Setup::default()).await;
    let run = paused_run(closed.clone(), alice.clone()).await;
    let (status, body) = call(&closed, "POST", &format!("{run}/resume"), As::User(&alice), Some(&answer)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    // The user still sees what the run asks.
    let (_, record) = call(&closed, "GET", &run, As::User(&alice), None).await;
    assert_eq!(record["resume_inputs"][0]["key"], "ask");
    // The key resumes it as before.
    let (status, result) = call(&closed, "POST", &format!("{run}/resume"), As::Key, Some(&answer)).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["answer"], "sent");

    let open_dir = TempDir::new("resume-open");
    let open = app(&open_dir, Setup { user_resume: true, ..Setup::default() }).await;
    let run = paused_run(open.clone(), alice.clone()).await;
    let bob = token_for("bob");
    assert_eq!(
        call(&open, "POST", &format!("{run}/resume"), As::User(&bob), Some(&answer)).await.0,
        StatusCode::NOT_FOUND
    );
    // A snapshot is the run's whole state: a user cannot supply one.
    let forged = json!({ "inputs": {}, "snapshot": { "run_id": "x" } });
    let (status, body) = call(&open, "POST", &format!("{run}/resume"), As::User(&alice), Some(&forged)).await;
    assert!(status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY, "{status} {body}");
    let (status, result) = call(&open, "POST", &format!("{run}/resume"), As::User(&alice), Some(&answer)).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["answer"], "sent");
    assert!(result.get("handled_errors").is_none(), "the user got the full result: {result}");
}

// ---- Conversations -----------------------------------------------------------

#[tokio::test]
async fn a_conversation_carries_history_and_variables_between_runs() {
    let dir = TempDir::new("conversation");
    let app = app(&dir, Setup { conversations: Some(Limits::default()), ..Setup::default() }).await;
    let alice = token_for("alice");
    let (status, created) =
        call(&app, "POST", "/v1/conversations", As::User(&alice), Some(&json!({ "workflow": "chat" }))).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert!(created.get("owner").is_none(), "a user is not told their own id");
    let id = created["id"].as_str().unwrap().to_owned();

    let turn = |query: &str| json!({ "conversation_id": id, "sys": { "query": query } });
    let (status, first) = call(&app, "POST", "/v1/workflows/chat/runs", As::User(&alice), Some(&turn("one"))).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["answer"], "turn 1: one after []");
    let (_, second) = call(&app, "POST", "/v1/workflows/chat/runs", As::User(&alice), Some(&turn("two"))).await;
    // The second run got the first turn as history and the variable the first run left.
    let answer = second["answer"].as_str().unwrap();
    assert!(answer.starts_with("turn 2: two after "), "{answer}");
    let history: Json = serde_json::from_str(answer.trim_start_matches("turn 2: two after ")).unwrap();
    assert_eq!(
        history,
        json!([{ "role": "user", "content": "one" }, { "role": "assistant", "content": "turn 1: one after []" }])
    );

    let (status, stored) = call(&app, "GET", &format!("/v1/conversations/{id}"), As::User(&alice), None).await;
    assert_eq!(status, StatusCode::OK, "{stored}");
    assert_eq!(stored["title"], "one");
    assert_eq!(stored["message_count"], 4);
    let roles: Vec<&str> = stored["messages"].as_array().unwrap().iter().filter_map(|m| m["role"].as_str()).collect();
    assert_eq!(roles, ["user", "assistant", "user", "assistant"]);
    assert_eq!(stored["messages"][3]["run_id"], second["run_id"]);
    // The run's record names its conversation, so traces group by it.
    let (_, record) =
        call(&app, "GET", &format!("/v1/runs/{}", second["run_id"].as_str().unwrap()), As::Key, None).await;
    assert_eq!(record["session_id"], id.as_str());
}

#[tokio::test]
async fn conversations_belong_to_their_user() {
    let dir = TempDir::new("conversation-owner");
    let app = app(&dir, Setup { conversations: Some(Limits::default()), ..Setup::default() }).await;
    let (alice, bob) = (token_for("alice"), token_for("bob"));
    let create = json!({ "workflow": "chat" });
    let (_, hers) = call(&app, "POST", "/v1/conversations", As::User(&alice), Some(&create)).await;
    let (_, his) = call(&app, "POST", "/v1/conversations", As::User(&bob), Some(&create)).await;
    let (_, keyed) = call(&app, "POST", "/v1/conversations", As::Key, Some(&create)).await;
    let path = |conversation: &Json| format!("/v1/conversations/{}", conversation["id"].as_str().unwrap());

    // Bob cannot read, continue or delete hers; it looks unknown.
    assert_eq!(call(&app, "GET", &path(&hers), As::User(&bob), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&app, "DELETE", &path(&hers), As::User(&bob), None).await.0, StatusCode::NOT_FOUND);
    let intrude = json!({ "conversation_id": hers["id"], "sys": { "query": "x" } });
    assert_eq!(
        call(&app, "POST", "/v1/workflows/chat/runs", As::User(&bob), Some(&intrude)).await.0,
        StatusCode::NOT_FOUND
    );
    // Nor one the key created.
    assert_eq!(call(&app, "GET", &path(&keyed), As::User(&alice), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&app, "GET", "/v1/conversations", As::Nobody, None).await.0, StatusCode::UNAUTHORIZED);

    let ids = |list: &Json| -> Vec<String> {
        list["conversations"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap().to_owned()).collect()
    };
    let (_, own) = call(&app, "GET", "/v1/conversations", As::User(&alice), None).await;
    assert_eq!(ids(&own), [hers["id"].as_str().unwrap()]);
    // The key sees all of them, with their owners, and can filter by user.
    let (_, all) = call(&app, "GET", "/v1/conversations", As::Key, None).await;
    assert_eq!(ids(&all).len(), 3);
    let (_, of_bob) = call(&app, "GET", "/v1/conversations?user=bob", As::Key, None).await;
    assert_eq!(ids(&of_bob), [his["id"].as_str().unwrap()]);
    assert_eq!(of_bob["conversations"][0]["owner"], "bob");

    assert_eq!(call(&app, "DELETE", &path(&hers), As::User(&alice), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&app, "GET", &path(&hers), As::User(&alice), None).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_user_keeps_only_their_most_recent_conversations() {
    let dir = TempDir::new("conversation-limit");
    let limits = Limits { per_user: 2, ..Limits::default() };
    let app = app(&dir, Setup { conversations: Some(limits), ..Setup::default() }).await;
    let alice = token_for("alice");
    let mut created = Vec::new();
    for _ in 0..3 {
        let body = json!({ "workflow": "chat" });
        let (status, conversation) = call(&app, "POST", "/v1/conversations", As::User(&alice), Some(&body)).await;
        assert_eq!(status, StatusCode::CREATED);
        created.push(conversation["id"].as_str().unwrap().to_owned());
        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    }
    let (_, list) = call(&app, "GET", "/v1/conversations", As::User(&alice), None).await;
    let kept: Vec<&str> = list["conversations"].as_array().unwrap().iter().filter_map(|c| c["id"].as_str()).collect();
    assert_eq!(kept, [created[2].as_str(), created[1].as_str()], "the least recently used one made room");
}

#[tokio::test]
async fn requests_that_do_not_fit_a_conversation_are_refused() {
    let dir = TempDir::new("conversation-refusals");
    let app = app(&dir, Setup { conversations: Some(Limits::default()), ..Setup::default() }).await;
    let runs = "/v1/workflows/chat/runs";
    let (_, created) = call(&app, "POST", "/v1/conversations", As::Key, Some(&json!({ "workflow": "chat" }))).await;
    let id = created["id"].clone();
    let cases = [
        (json!({ "conversation_id": id, "sys": { "query": "x", "history": [] } }), StatusCode::BAD_REQUEST, "not both"),
        (json!({ "conversation_id": id }), StatusCode::BAD_REQUEST, "sys.query"),
        (
            json!({ "conversation_id": id, "session_id": "other", "sys": { "query": "x" } }),
            StatusCode::BAD_REQUEST,
            "session_id",
        ),
        (json!({ "conversation_id": "conv-unknown", "sys": { "query": "x" } }), StatusCode::NOT_FOUND, "unknown"),
        (json!({ "conversation_id": "../../etc", "sys": { "query": "x" } }), StatusCode::NOT_FOUND, "unknown"),
    ];
    for (body, expected, hint) in cases {
        let (status, answer) = call(&app, "POST", runs, As::Key, Some(&body)).await;
        assert_eq!(status, expected, "{body}: {answer}");
        assert!(answer["error"].as_str().unwrap().contains(hint), "{answer}");
    }
    // A conversation of one workflow does not continue in another.
    let body = json!({ "conversation_id": id, "sys": { "query": "x" } });
    assert_eq!(call(&app, "POST", "/v1/workflows/internal/runs", As::Key, Some(&body)).await.0, StatusCode::CONFLICT);
    // Unknown workflows and inputs are refused when the conversation is created.
    for (body, expected) in [
        (json!({ "workflow": "nope" }), StatusCode::NOT_FOUND),
        (json!({ "workflow": "chat", "input": "nope" }), StatusCode::BAD_REQUEST),
        (json!({ "workflow": "chat", "title": "x".repeat(400) }), StatusCode::BAD_REQUEST),
    ] {
        assert_eq!(call(&app, "POST", "/v1/conversations", As::Key, Some(&body)).await.0, expected, "{body}");
    }
    // A user may not open a conversation with a workflow that is not built for users.
    let alice = token_for("alice");
    let internal = json!({ "workflow": "internal" });
    assert_eq!(
        call(&app, "POST", "/v1/conversations", As::User(&alice), Some(&internal)).await.0,
        StatusCode::FORBIDDEN
    );
    // `sys.history` keeps working as before, next to stored conversations.
    let own_history =
        json!({ "sys": { "query": "x", "history": [{ "role": "user", "content": "kept by the client" }] } });
    let (status, result) = call(&app, "POST", runs, As::Key, Some(&own_history)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(result["answer"].as_str().unwrap().contains("kept by the client"), "{result}");
}

#[tokio::test]
async fn a_server_without_conversations_says_so() {
    let dir = TempDir::new("conversation-off");
    let app = app(&dir, Setup::default()).await;
    assert_eq!(call(&app, "GET", "/v1/conversations", As::Key, None).await.0, StatusCode::NOT_FOUND);
    let body = json!({ "conversation_id": "conv-1", "sys": { "query": "x" } });
    let (status, answer) = call(&app, "POST", "/v1/workflows/chat/runs", As::Key, Some(&body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
    assert!(answer["error"].as_str().unwrap().contains("--conversations"), "{answer}");
}

// ---- The chat page and guests --------------------------------------------------

#[tokio::test]
async fn the_chat_page_is_served_with_a_strict_policy() {
    let dir = TempDir::new("chat-page");
    let app = app(&dir, Setup { chat: Some(ChatAccess::Guests), ..Setup::default() }).await;
    let (status, headers, _) = send(&app, request("GET", "/chat", As::Nobody, None)).await;
    assert_eq!(status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(headers["location"], "chat/");
    for uri in ["/chat/", "/chat/support"] {
        let (status, headers, _) = send(&app, request("GET", uri, As::Nobody, None)).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(headers["content-type"].to_str().unwrap().starts_with("text/html"));
        let policy = headers["content-security-policy"].to_str().unwrap();
        assert!(policy.contains("script-src 'self'") && policy.contains("connect-src 'self'"), "{policy}");
        assert!(policy.contains("frame-ancestors 'self' https://shop.example"), "{policy}");
        assert!(!policy.contains("unsafe"), "{policy}");
        assert_eq!(headers["x-content-type-options"], "nosniff");
    }
    // Only the offered workflows have a page.
    assert_eq!(send(&app, request("GET", "/chat/internal", As::Nobody, None)).await.0, StatusCode::NOT_FOUND);
    for script in ["/chat/chat.js", "/chat/embed.js"] {
        let (status, headers, _) = send(&app, request("GET", script, As::Nobody, None)).await;
        assert_eq!(status, StatusCode::OK, "{script}");
        assert!(headers["content-type"].to_str().unwrap().starts_with("text/javascript"));
        let again = Request::get(script).header("if-none-match", &headers["etag"]).body(Body::empty()).unwrap();
        assert_eq!(send(&app, again).await.0, StatusCode::NOT_MODIFIED);
    }
    let (status, config) = call(&app, "GET", "/v1/chat/config", As::Nobody, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        config,
        json!({
            "title": "Support", "workflows": [{ "name": "support", "input": "message" }], "access": "guests",
            "user_token_header": null, "conversations": false, "resume": false, "feedback": true,
            "diagrams": false,
        })
    );
    // Without `--chat-mermaid` there is no Mermaid to serve.
    assert_eq!(send(&app, request("GET", "/chat/mermaid.js", As::Nobody, None)).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_chat_page_puts_the_message_in_the_one_input_a_workflow_requires() {
    let dir = TempDir::new("chat-input");
    let setup = Setup { chat: Some(ChatAccess::Key), chat_specs: vec!["support", "shop"], ..Setup::default() };
    let app = app(&dir, setup).await;
    let (_, config) = call(&app, "GET", "/v1/chat/config", As::Nobody, None).await;
    assert_eq!(
        config["workflows"],
        json!([{ "name": "support", "input": "message" }, { "name": "shop", "input": null }]),
        "`support` requires `message`; `shop` requires nothing and reads sys.query"
    );
}

#[tokio::test]
async fn a_mermaid_build_named_by_the_operator_is_served_from_the_same_origin() {
    let dir = TempDir::new("chat-mermaid");
    let file = dir.path().join("mermaid.min.js");
    std::fs::write(&file, "globalThis.mermaid = {};").unwrap();
    let setup = Setup { chat: Some(ChatAccess::Guests), mermaid: Some(file), ..Setup::default() };
    let app = app(&dir, setup).await;
    let (status, headers, body) = send(&app, request("GET", "/chat/mermaid.js", As::Nobody, None)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers["content-type"].to_str().unwrap().starts_with("text/javascript"));
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(body, "globalThis.mermaid = {};");
    let (_, config) = call(&app, "GET", "/v1/chat/config", As::Nobody, None).await;
    assert_eq!(config["diagrams"], true);
    // The page's policy stays as strict: Mermaid is a script of its own origin.
    let (_, headers, _) = send(&app, request("GET", "/chat/", As::Nobody, None)).await;
    let policy = headers["content-security-policy"].to_str().unwrap();
    assert!(policy.contains("script-src 'self';") && !policy.contains("unsafe"), "{policy}");
}

#[tokio::test]
async fn a_server_without_a_chat_page_serves_none() {
    let dir = TempDir::new("chat-off");
    let app = app(&dir, Setup::default()).await;
    for uri in ["/chat/", "/chat/chat.js", "/v1/chat/config"] {
        assert_eq!(send(&app, request("GET", uri, As::Nobody, None)).await.0, StatusCode::NOT_FOUND, "{uri}");
    }
    assert_eq!(call(&app, "POST", "/v1/chat/guest", As::Nobody, None).await.0, StatusCode::NOT_FOUND);
    // A chat page for key holders opens no guest sessions either.
    let keyed_dir = TempDir::new("chat-key");
    let keyed = self::app(&keyed_dir, Setup { chat: Some(ChatAccess::Key), ..Setup::default() }).await;
    assert_eq!(call(&keyed, "POST", "/v1/chat/guest", As::Nobody, None).await.0, StatusCode::NOT_FOUND);
    let (_, config) = call(&keyed, "GET", "/v1/chat/config", As::Nobody, None).await;
    assert_eq!(config["access"], "key");
    assert_eq!(config["resume"], true, "the key resumes any run");
}

#[tokio::test]
async fn a_guest_chats_with_the_offered_workflow_and_reaches_nothing_else() {
    let dir = TempDir::new("guest");
    let setup = Setup { chat: Some(ChatAccess::Guests), conversations: Some(Limits::default()), ..Setup::default() };
    let app = app(&dir, setup).await;
    let (status, headers, body) = send(&app, request("POST", "/v1/chat/guest", As::Nobody, None)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(headers["cache-control"], "no-store");
    let session: Json = serde_json::from_str(&body).unwrap();
    let token = session["token"].as_str().unwrap().to_owned();
    assert!(session["expires_ms"].as_u64().unwrap() > 0);

    let start = json!({ "inputs": { "message": "hi" } });
    let (status, result) = call(&app, "POST", "/v1/workflows/support/runs", As::Bearer(&token), Some(&start)).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let answer = result["answer"].as_str().unwrap();
    assert!(answer.starts_with("You said hi as guest:"), "{answer}");
    // A guest sees the answer only, like any user.
    assert!(result.get("handled_errors").is_none(), "{result}");
    let run = format!("/v1/runs/{}", result["run_id"].as_str().unwrap());
    assert_eq!(call(&app, "GET", &run, As::Bearer(&token), None).await.0, StatusCode::OK);
    let feedback = json!({ "name": "helpful", "value": true });
    assert_eq!(
        call(&app, "POST", &format!("{run}/feedback"), As::Bearer(&token), Some(&feedback)).await.0,
        StatusCode::CREATED
    );
    // A guest keeps conversations like a signed-in user.
    let create = json!({ "workflow": "support", "input": "message" });
    let (status, conversation) = call(&app, "POST", "/v1/conversations", As::Bearer(&token), Some(&create)).await;
    assert_eq!(status, StatusCode::CREATED, "{conversation}");

    // Nothing but the offered workflow, and nothing of anyone else.
    for workflow in ["internal", "chat", "confirm"] {
        let uri = format!("/v1/workflows/{workflow}/runs");
        let (status, body) = call(&app, "POST", &uri, As::Bearer(&token), Some(&json!({}))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{workflow}: {body}");
    }
    assert_eq!(
        call(&app, "POST", "/v1/conversations", As::Bearer(&token), Some(&json!({ "workflow": "chat" }))).await.0,
        StatusCode::FORBIDDEN
    );
    for uri in ["/v1/runs", "/v1/workflows", "/v1/hooks"] {
        assert_eq!(call(&app, "GET", uri, As::Bearer(&token), None).await.0, StatusCode::UNAUTHORIZED, "{uri}");
    }
    let (_, other) = call(&app, "POST", "/v1/chat/guest", As::Nobody, None).await;
    let other = other["token"].as_str().unwrap();
    assert_eq!(call(&app, "GET", &run, As::Bearer(other), None).await.0, StatusCode::NOT_FOUND);

    // A renewed session is the same guest: it still reaches the run.
    let (status, renewed) = call(&app, "POST", "/v1/chat/guest", As::Bearer(&token), None).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(call(&app, "GET", &run, As::Bearer(renewed["token"].as_str().unwrap()), None).await.0, StatusCode::OK);
}

#[tokio::test]
async fn forged_guest_tokens_and_reserved_subjects_are_refused() {
    let dir = TempDir::new("guest-forged");
    let app = app(&dir, Setup { chat: Some(ChatAccess::Guests), ..Setup::default() }).await;
    let start = json!({ "inputs": { "message": "hi" } });
    let forged = format!("grg1.{}.99999999999.c2lnbmF0dXJl", "ab".repeat(16));
    let (status, body) = call(&app, "POST", "/v1/workflows/support/runs", As::Bearer(&forged), Some(&start)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    // A token of the identity provider cannot claim to be a guest.
    let impostor = token_for(&format!("guest:{}", "ab".repeat(16)));
    let (status, body) = call(&app, "POST", "/v1/workflows/chat/runs", As::User(&impostor), Some(&json!({}))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    // Without guest access, a guest token is just a wrong key.
    let closed_dir = TempDir::new("guest-closed");
    let closed = self::app(&closed_dir, Setup { chat: Some(ChatAccess::Key), ..Setup::default() }).await;
    assert_eq!(
        call(&closed, "POST", "/v1/workflows/support/runs", As::Bearer(&forged), Some(&start)).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn each_guest_is_rate_limited() {
    let dir = TempDir::new("guest-rate");
    let app = app(&dir, Setup { chat: Some(ChatAccess::Guests), guest_rate_limit: 2, ..Setup::default() }).await;
    let (_, session) = call(&app, "POST", "/v1/chat/guest", As::Nobody, None).await;
    let token = session["token"].as_str().unwrap();
    let start = json!({ "inputs": { "message": "hi" } });
    let mut last = String::new();
    for _ in 0..2 {
        let (status, result) = call(&app, "POST", "/v1/workflows/support/runs", As::Bearer(token), Some(&start)).await;
        assert_eq!(status, StatusCode::OK, "{result}");
        last = result["run_id"].as_str().unwrap().to_owned();
    }
    let (status, headers, body) =
        send(&app, request("POST", "/v1/workflows/support/runs", As::Bearer(token), Some(&start))).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert!(headers.contains_key("retry-after"));
    // Reading what they started is not counted.
    for _ in 0..5 {
        assert_eq!(call(&app, "GET", &format!("/v1/runs/{last}"), As::Bearer(token), None).await.0, StatusCode::OK);
    }
}

// ---- The page context of a web chat (`sys.context`) ---------------------------------

#[tokio::test]
async fn the_page_context_reaches_the_workflow_and_is_never_identity() {
    let dir = TempDir::new("context");
    let app = app(&dir, Setup::default()).await;
    let start = json!({ "sys": {
        "query": "Is it in stock?",
        "context": { "product_id": "sku-1", "cart": { "items": [{ "id": "sku-2", "qty": 1 }] } },
    } });
    let (status, result) = call(&app, "POST", "/v1/workflows/shop/runs", As::Key, Some(&start)).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["answer"], "Is it in stock? about sku-1 for ");

    // A context that claims to be someone else changes nothing: the user is the verified token.
    let forged = json!({ "sys": {
        "query": "Refund me",
        "context": { "product_id": "sku-1", "user_id": "admin", "role": "admin", "sub": "admin" },
    } });
    let alice = token_for("alice");
    let (status, result) = call(&app, "POST", "/v1/workflows/shop/runs", As::User(&alice), Some(&forged)).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["answer"], "Refund me about sku-1 for alice");
}

#[tokio::test]
async fn a_page_context_out_of_its_limits_is_refused() {
    let dir = TempDir::new("context-limits");
    let app = app(&dir, Setup::default()).await;
    let mut deep = json!("leaf");
    for _ in 0..6 {
        deep = json!({ "next": deep });
    }
    let refused = [
        (json!("sku-1"), "must be a JSON object"),
        (json!(["sku-1"]), "must be a JSON object"),
        (deep, "nested deeper than 5 levels"),
        (json!({ "notes": "x".repeat(9000) }), "more than 8192"),
    ];
    for (context, reason) in refused {
        let start = json!({ "sys": { "query": "hi", "context": context } });
        let (status, body) = call(&app, "POST", "/v1/workflows/shop/runs", As::Key, Some(&start)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.to_string().contains(reason), "{reason}: {body}");
    }
    // Nothing was started for them.
    let (_, runs) = call(&app, "GET", "/v1/runs", As::Key, None).await;
    assert_eq!(runs["runs"].as_array().map(Vec::len), Some(0), "{runs}");
}
