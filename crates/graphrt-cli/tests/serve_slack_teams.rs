//! `graphrt serve --channel slack=...` and `teams=...`: a person writes in
//! Slack or Teams, the workflow runs and the answer goes back, against fake
//! Slack (Web API and a Socket Mode WebSocket) and fake Teams (token server,
//! signing keys and Bot Connector) servers that record every call. No model
//! is involved: the workflows answer from templates.
#![cfg(all(
    feature = "channel-slack",
    feature = "channel-teams",
    feature = "code",
    feature = "jwt",
    feature = "files"
))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json as JsonBody;
use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::{SinkExt, StreamExt};
use graphrt_channel::Secret;
use graphrt_cli::approvals::Approvals;
use graphrt_cli::channels::{Channels, Defaults, config};
use graphrt_cli::conversations::{ConversationStore, Conversations, Limits, MemoryConversations};
use graphrt_cli::serve::{AppState, build_state, discover, router};
use graphrt_cli::setup::ServiceOptions;
use graphrt_core::engine::Limits as EngineLimits;
use http_body_util::BodyExt;
use serde_json::{Value as Json, json};
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;

const API_KEY: &str = "server-only-key";
const SLACK_BOT_TOKEN: &str = "xoxb-1111-2222-fakebottoken";
const SLACK_APP_TOKEN: &str = "xapp-1-A111-2222-fakeapptoken";
const SLACK_SIGNING_SECRET: &str = "fake-slack-signing-secret";
const TEAMS_APP_ID: &str = "00000000-1111-2222-3333-444444444444";
const TEAMS_PASSWORD: &str = "fake-teams-client-secret";
const TEAMS_TENANT: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
const TEAMS_TOKEN: &str = "fake-connector-access-token";

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("graphrt-slack-teams-{tag}-{}", std::process::id()));
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

async fn until(what: &str, ready: impl Fn() -> bool) {
    for _ in 0..500 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for: {what}");
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(400)).await;
}

async fn listen(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ended = axum::serve(listener, app).await;
    });
    address
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

// ---------------------------------------------------------------- fake Slack

#[derive(Default)]
struct FakeSlack {
    /// Every Web API call: the method and its JSON (or `{"form": ...}`).
    calls: Mutex<Vec<(String, Json)>>,
    /// Envelopes the Socket Mode server still has to send.
    envelopes: Mutex<Vec<Json>>,
    /// Envelope ids the app acknowledged.
    acks: Mutex<Vec<String>>,
    /// Bodies posted to the upload address.
    uploads: Mutex<Vec<Vec<u8>>>,
    socket_url: Mutex<String>,
    base: Mutex<String>,
    next: AtomicU64,
}

impl FakeSlack {
    fn posts(&self) -> Vec<Json> {
        self.calls.lock().unwrap().iter().filter(|(m, _)| m == "chat.postMessage").map(|(_, b)| b.clone()).collect()
    }

    /// The messages posted to `channel` (and `thread`, when given).
    fn posted_to(&self, channel: &str, thread: Option<&str>) -> Vec<Json> {
        self.posts()
            .into_iter()
            .filter(|post| post["channel"] == channel && post["thread_ts"].as_str() == thread)
            .collect()
    }

    fn texts(&self, channel: &str, thread: Option<&str>) -> Vec<String> {
        self.posted_to(channel, thread)
            .iter()
            .map(|post| post["text"].as_str().unwrap_or_default().to_owned())
            .collect()
    }
}

async fn slack_method(
    State(fake): State<Arc<FakeSlack>>,
    UrlPath(method): UrlPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let token = if method == "apps.connections.open" { SLACK_APP_TOKEN } else { SLACK_BOT_TOKEN };
    let authorized = headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {token}"));
    if !authorized {
        return JsonBody(json!({"ok": false, "error": "invalid_auth"})).into_response();
    }
    let params: Json =
        serde_json::from_slice(&body).unwrap_or_else(|_| json!({ "form": String::from_utf8_lossy(&body).to_string() }));
    fake.calls.lock().unwrap().push((method.clone(), params));
    let ts = format!("1700009999.{:06}", fake.next.fetch_add(1, Ordering::Relaxed));
    let answer = match method.as_str() {
        "chat.postMessage" => json!({"ok": true, "ts": ts}),
        "auth.test" => json!({"ok": true, "user_id": "U0BOT", "team": "Acme"}),
        "apps.connections.open" => json!({"ok": true, "url": *fake.socket_url.lock().unwrap()}),
        "files.getUploadURLExternal" => {
            json!({"ok": true, "file_id": "F1", "upload_url": format!("{}/upload/F1", fake.base.lock().unwrap())})
        }
        _ => json!({"ok": true}),
    };
    JsonBody(answer).into_response()
}

async fn slack_upload(State(fake): State<Arc<FakeSlack>>, body: Bytes) -> Response {
    fake.uploads.lock().unwrap().push(body.to_vec());
    "OK".into_response()
}

/// A Socket Mode server: says hello, sends the queued envelopes, records acks.
async fn slack_socket(fake: Arc<FakeSlack>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("ws://{}/link/?ticket=t", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let fake = Arc::clone(&fake);
            tokio::spawn(async move {
                let Ok(socket) = tokio_tungstenite::accept_async(stream).await else { return };
                let (mut sink, mut source) = socket.split();
                let hello = json!({"type": "hello", "num_connections": 1}).to_string();
                if sink.send(Message::Text(hello.into())).await.is_err() {
                    return;
                }
                let reader = {
                    let fake = Arc::clone(&fake);
                    tokio::spawn(async move {
                        while let Some(Ok(Message::Text(text))) = source.next().await {
                            let ack: Json = serde_json::from_str(text.as_str()).unwrap_or(Json::Null);
                            if let Some(id) = ack["envelope_id"].as_str() {
                                fake.acks.lock().unwrap().push(id.to_owned());
                            }
                        }
                    })
                };
                loop {
                    let next = {
                        let mut queued = fake.envelopes.lock().unwrap();
                        if queued.is_empty() { None } else { Some(queued.remove(0)) }
                    };
                    match next {
                        Some(envelope) => {
                            if sink.send(Message::Text(envelope.to_string().into())).await.is_err() {
                                break;
                            }
                        }
                        None => tokio::time::sleep(Duration::from_millis(10)).await,
                    }
                    if reader.is_finished() {
                        break;
                    }
                }
            });
        }
    });
    address
}

async fn fake_slack() -> (Arc<FakeSlack>, String) {
    let fake = Arc::new(FakeSlack::default());
    let app = axum::Router::new()
        .route("/api/{method}", post(slack_method))
        .route("/upload/{file}", post(slack_upload))
        .with_state(Arc::clone(&fake));
    let base = listen(app).await;
    *fake.base.lock().unwrap() = base.clone();
    *fake.socket_url.lock().unwrap() = slack_socket(Arc::clone(&fake)).await;
    (fake, format!("{base}/api"))
}

fn slack_event(event: Json) -> Json {
    json!({"type": "event_callback", "team_id": "T1", "event_id": format!("Ev{}", event["ts"]), "event": event})
}

fn dm(user: &str, ts: &str, text: &str) -> Json {
    slack_event(
        json!({"type": "message", "channel": format!("D{user}"), "channel_type": "im", "user": user, "text": text, "ts": ts}),
    )
}

fn mention(channel: &str, user: &str, ts: &str, thread: Option<&str>, text: &str) -> Json {
    let mut event =
        json!({"type": "app_mention", "channel": channel, "user": user, "text": format!("<@U0BOT> {text}"), "ts": ts});
    if let Some(thread) = thread {
        event["thread_ts"] = json!(thread);
    }
    slack_event(event)
}

/// A tap on button `value` of a message the app posted in `channel`.
fn tap(user: &str, channel: &str, message_ts: &str, value: &str, title: &str) -> Json {
    json!({"type": "block_actions", "user": {"id": user, "name": "rina"}, "channel": {"id": channel},
        "container": {"channel_id": channel}, "message": {"ts": message_ts, "text": "question"},
        "actions": [{"action_id": "graphrt_choice_0", "value": value, "text": {"type": "plain_text", "text": title},
                     "action_ts": format!("{}.1", now_secs())}]})
}

// ---------------------------------------------------------------- fake Teams

#[derive(Default)]
struct FakeTeams {
    /// `(conversation, activity)` of every activity the bot sent.
    activities: Mutex<Vec<(String, Json)>>,
    /// Bodies of conversations the bot opened.
    opened: Mutex<Vec<Json>>,
    /// `(conversation, activity id, activity)` of every update.
    updates: Mutex<Vec<(String, String, Json)>>,
    /// `(Content-Range, bytes)` of every OneDrive upload.
    uploads: Mutex<Vec<(String, Vec<u8>)>>,
    token_requests: AtomicU64,
    base: Mutex<String>,
    next: AtomicU64,
}

impl FakeTeams {
    fn sent_to(&self, conversation: &str) -> Vec<Json> {
        self.activities.lock().unwrap().iter().filter(|(c, _)| c == conversation).map(|(_, a)| a.clone()).collect()
    }

    fn texts_to(&self, conversation: &str) -> Vec<String> {
        self.sent_to(conversation)
            .iter()
            .filter(|a| a["type"] == "message")
            .filter_map(|a| a["text"].as_str().map(str::to_owned))
            .collect()
    }

    fn cards_to(&self, conversation: &str) -> Vec<Json> {
        self.sent_to(conversation)
            .iter()
            .flat_map(|a| a["attachments"].as_array().cloned().unwrap_or_default())
            .filter(|a| a["contentType"] == "application/vnd.microsoft.card.adaptive")
            .map(|a| a["content"].clone())
            .collect()
    }
}

fn connector_authorized(headers: &HeaderMap) -> bool {
    headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {TEAMS_TOKEN}"))
}

async fn teams_token(State(fake): State<Arc<FakeTeams>>, UrlPath(tenant): UrlPath<String>, body: String) -> Response {
    fake.token_requests.fetch_add(1, Ordering::Relaxed);
    let good = tenant == TEAMS_TENANT
        && body.contains(&format!("client_id={TEAMS_APP_ID}"))
        && body.contains(&format!("client_secret={TEAMS_PASSWORD}"))
        && body.contains("grant_type=client_credentials");
    if !good {
        return (StatusCode::UNAUTHORIZED, JsonBody(json!({"error": "invalid_client"}))).into_response();
    }
    JsonBody(json!({"token_type": "Bearer", "expires_in": 3599, "access_token": TEAMS_TOKEN})).into_response()
}

async fn teams_openid(State(fake): State<Arc<FakeTeams>>) -> Response {
    JsonBody(
        json!({"issuer": "https://api.botframework.com", "jwks_uri": format!("{}/keys", fake.base.lock().unwrap())}),
    )
    .into_response()
}

fn key_pair() -> ring::signature::RsaKeyPair {
    ring::signature::RsaKeyPair::from_der(include_bytes!("../../graphrt-channel/fixtures/teams-test-key.der")).unwrap()
}

async fn teams_keys() -> Response {
    let public = ring::signature::RsaPublicKeyComponents::<Vec<u8>>::from(key_pair().public());
    JsonBody(json!({"keys": [{"kty": "RSA", "use": "sig", "kid": "test-key",
        "n": URL_SAFE_NO_PAD.encode(&public.n), "e": URL_SAFE_NO_PAD.encode(&public.e), "endorsements": ["msteams"]}]}))
    .into_response()
}

async fn teams_activity(
    State(fake): State<Arc<FakeTeams>>,
    UrlPath(conversation): UrlPath<String>,
    headers: HeaderMap,
    JsonBody(activity): JsonBody<Json>,
) -> Response {
    if !connector_authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    fake.activities.lock().unwrap().push((conversation, activity));
    JsonBody(json!({"id": format!("act-{}", fake.next.fetch_add(1, Ordering::Relaxed))})).into_response()
}

async fn teams_update(
    State(fake): State<Arc<FakeTeams>>,
    UrlPath((conversation, id)): UrlPath<(String, String)>,
    headers: HeaderMap,
    JsonBody(activity): JsonBody<Json>,
) -> Response {
    if !connector_authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    fake.updates.lock().unwrap().push((conversation, id, activity));
    JsonBody(json!({"id": "updated"})).into_response()
}

async fn teams_open(
    State(fake): State<Arc<FakeTeams>>,
    headers: HeaderMap,
    JsonBody(body): JsonBody<Json>,
) -> Response {
    if !connector_authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let member = body["members"][0]["id"].as_str().unwrap_or_default().to_owned();
    fake.opened.lock().unwrap().push(body);
    JsonBody(json!({"id": format!("a:opened-{member}")})).into_response()
}

async fn onedrive_upload(State(fake): State<Arc<FakeTeams>>, headers: HeaderMap, body: Bytes) -> Response {
    // The session address authorizes itself: a bearer token here would be a leak.
    if headers.contains_key("authorization") {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let range = headers.get("content-range").and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
    fake.uploads.lock().unwrap().push((range, body.to_vec()));
    (StatusCode::CREATED, JsonBody(json!({"id": "drive-item"}))).into_response()
}

async fn fake_teams() -> (Arc<FakeTeams>, String) {
    let fake = Arc::new(FakeTeams::default());
    let app = axum::Router::new()
        .route("/login/{tenant}/oauth2/v2.0/token", post(teams_token))
        .route("/openid", get(teams_openid))
        .route("/keys", get(teams_keys))
        .route("/v3/conversations", post(teams_open))
        .route("/v3/conversations/{conversation}/activities", post(teams_activity))
        .route("/v3/conversations/{conversation}/activities/{id}", put(teams_update))
        .route("/upload/{id}", put(onedrive_upload))
        .with_state(Arc::clone(&fake));
    let base = listen(app).await;
    *fake.base.lock().unwrap() = base.clone();
    (fake, base)
}

/// A Bot Connector token for `service_url`, signed with the test key.
fn connector_token(service_url: &str, audience: &str) -> String {
    let header = json!({"alg": "RS256", "kid": "test-key", "typ": "JWT"});
    let claims = json!({"iss": "https://api.botframework.com", "aud": audience, "serviceurl": service_url,
        "exp": now_secs() + 3600, "nbf": now_secs() - 60});
    let signed =
        format!("{}.{}", URL_SAFE_NO_PAD.encode(header.to_string()), URL_SAFE_NO_PAD.encode(claims.to_string()));
    let pair = key_pair();
    let mut signature = vec![0; pair.public().modulus_len()];
    pair.sign(&ring::signature::RSA_PKCS1_SHA256, &ring::rand::SystemRandom::new(), signed.as_bytes(), &mut signature)
        .unwrap();
    format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature))
}

fn teams_message(service_url: &str, conversation: Json, from: &str, id: &str, text: &str) -> Json {
    json!({"type": "message", "id": id, "channelId": "msteams", "serviceUrl": service_url,
        "from": {"id": format!("29:{from}"), "aadObjectId": from, "name": "Rina"},
        "conversation": conversation, "recipient": {"id": format!("28:{TEAMS_APP_ID}")}, "text": text,
        "channelData": {"tenant": {"id": TEAMS_TENANT}}})
}

fn personal(id: &str) -> Json {
    json!({"id": id, "conversationType": "personal", "tenantId": TEAMS_TENANT})
}

// ---------------------------------------------------------------- the server under test

fn write_workflows(dir: &TempDir) -> PathBuf {
    let flows = dir.path().join("flows");
    std::fs::create_dir_all(&flows).unwrap();
    let write = |name: &str, yaml: &str| std::fs::write(flows.join(format!("{name}.yaml")), yaml).unwrap();
    write(
        "echo",
        r#"version: "1"
name: echo
graph:
  nodes:
    - { id: start, type: start }
    - { id: done, type: answer, answer: "You said: **{{#sys.query#}}** [{{#sys.user_id#}} {{#sys.conversation_id#}} {{#sys.channel#}}]" }
  edges:
    - { source: start, target: done }
"#,
    );
    write(
        "refund",
        r#"version: "1"
name: refund
graph:
  nodes:
    - { id: start, type: start }
    - id: ask
      type: human-input
      prompt: "Refund 40 EUR to Ana?"
      actions:
        - { id: approve, title: Approve }
        - { id: reject, title: Reject }
    - { id: paid, type: answer, answer: "refunded" }
    - { id: kept, type: answer, answer: "rejected" }
  edges:
    - { source: start, target: ask }
    - { source: ask, target: paid, handle: approve }
    - { source: ask, target: kept, handle: reject }
"#,
    );
    // Answers with an Adaptive Card.
    write(
        "card",
        r#"version: "1"
name: card
graph:
  nodes:
    - { id: start, type: start }
    - id: make
      type: code
      code_language: javascript
      code: |
        function main() {
          return { card: { "$ui": "adaptive_card", version: 1, card: { type: "AdaptiveCard", version: "1.5",
            body: [ { type: "TextBlock", text: "Order #1042", size: "large" },
                    { type: "FactSet", facts: [ { title: "Total", value: "Rp 250.000" } ] } ],
            actions: [ { type: "Action.Submit", id: "track", title: "Track it" },
                       { type: "Action.OpenUrl", title: "Receipt", url: "https://shop.example/r/1042" } ] } } };
        }
      outputs: { card: { type: object } }
    - id: end
      type: end
      outputs: [{ variable: card, value_selector: [make, card] }]
  edges:
    - { source: start, target: make }
    - { source: make, target: end }
"#,
    );
    // Writes a file into the workspace and answers with it.
    write(
        "note",
        r#"version: "1"
name: note
graph:
  nodes:
    - { id: start, type: start }
    - id: write
      type: tool
      provider_id: files
      tool_name: write
      tool_parameters:
        path: { type: constant, value: "notes/summary.md" }
        content: { type: mixed, value: "Summary for the customer: {{#sys.query#}}" }
        overwrite: { type: constant, value: true }
    - { id: done, type: answer, answer: "Here is your note." }
  edges:
    - { source: start, target: write }
    - { source: write, target: done }
"#,
    );
    write(
        "broken",
        r#"version: "1"
name: broken
graph:
  nodes:
    - { id: start, type: start }
    - id: fail
      type: code
      code_language: javascript
      code: |
        function main() { throw new Error("the warehouse API is down"); }
      outputs: { x: { type: string } }
    - { id: done, type: answer, answer: "never" }
  edges:
    - { source: start, target: fail }
    - { source: fail, target: done }
"#,
    );
    flows
}

struct Server {
    state: Arc<AppState>,
    app: axum::Router,
    _dir: TempDir,
}

fn lookup(name: &str) -> Option<String> {
    let value = match name {
        "SLACK_BOT_TOKEN" => SLACK_BOT_TOKEN,
        "SLACK_SIGNING_SECRET" => SLACK_SIGNING_SECRET,
        "SOCKET_APP_TOKEN" => SLACK_APP_TOKEN,
        "TEAMS_APP_ID" => TEAMS_APP_ID,
        "TEAMS_APP_PASSWORD" => TEAMS_PASSWORD,
        "TEAMS_TENANT_ID" => TEAMS_TENANT,
        _ => return None,
    };
    Some(value.to_owned())
}

/// A server with the channels file `channels` (`{SLACK}` and `{TEAMS}` are the fakes).
async fn start(tag: &str, channels: &str, slack: &str, teams: &str) -> Server {
    start_with(tag, channels, slack, teams, None).await
}

/// [`start`], with a workspace and the public address `public_url`.
async fn start_with(tag: &str, channels: &str, slack: &str, teams: &str, public_url: Option<&str>) -> Server {
    let dir = TempDir::new(tag);
    let flows = write_workflows(&dir);
    let files = discover(&flows).expect("discover");
    let options = ServiceOptions {
        plugins: graphrt_cli::extensions::PluginOptions {
            dirs: Vec::new(),
            no_default: true,
            trusted: Vec::new(),
            pool: Default::default(),
            allow_private_network: false,
        },
        workspace: public_url.map(|_| graphrt_cli::setup::WorkspaceOptions {
            dir: dir.path().join("workspace"),
            ..graphrt_cli::setup::WorkspaceOptions::default()
        }),
        ..ServiceOptions::default()
    };
    let channels_file = dir.path().join("channels.yaml");
    std::fs::write(&channels_file, channels.replace("{SLACK}", slack).replace("{TEAMS}", teams)).unwrap();
    let configs = config::load(&[], Some(&channels_file), Defaults::default(), &lookup).expect("channels file");
    let state = build_state(&files, &options, EngineLimits::default(), Some(API_KEY.into())).await.expect("state");
    let channels =
        Channels::new(configs, |name| state.channel_start_inputs(name), public_url.map(str::to_owned), false)
            .expect("channels")
            .expect("some")
            .into();
    let channels: Arc<Channels> = channels;
    let conversations = Arc::new(MemoryConversations::default());
    let store: Arc<dyn ConversationStore> = conversations;
    let state = Arc::new(
        state
            .with_conversations(Some(Conversations::with_store(store, Limits::default())))
            .with_approvals(Some(Approvals { expire: false }))
            .with_channels(Some(Arc::clone(&channels)))
            .add_notifiers(channels.notifiers(), false),
    );
    channels.start(&state);
    Server { app: router(Arc::clone(&state), 1 << 20), state, _dir: dir }
}

impl Server {
    async fn request(&self, request: Request<Body>) -> (StatusCode, Vec<u8>) {
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        (status, response.into_body().collect().await.unwrap().to_bytes().to_vec())
    }

    async fn post(
        &self,
        path: &str,
        headers: &[(&str, String)],
        content_type: &str,
        body: Vec<u8>,
    ) -> (StatusCode, Vec<u8>) {
        let mut request = Request::post(path).header("content-type", content_type);
        for (name, value) in headers {
            request = request.header(*name, value.as_str());
        }
        self.request(request.body(Body::from(body)).unwrap()).await
    }

    async fn api(&self, method: &str, path: &str, body: Json) -> (StatusCode, Json) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {API_KEY}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let (status, bytes) = self.request(request).await;
        (status, serde_json::from_slice(&bytes).unwrap_or(Json::Null))
    }

    /// Post a signed Events API delivery to the Slack channel `name`.
    async fn slack(&self, name: &str, body: Vec<u8>, content_type: &str) -> (StatusCode, Vec<u8>) {
        let timestamp = now_secs().to_string();
        let signature = graphrt_channel::slack::sign(&body, &timestamp, &Secret::new(SLACK_SIGNING_SECRET));
        let headers = [("x-slack-request-timestamp", timestamp), ("x-slack-signature", signature)];
        self.post(&format!("/v1/channels/{name}/webhook"), &headers, content_type, body).await
    }

    async fn slack_tap(&self, name: &str, payload: &Json) -> StatusCode {
        let encoded: String = payload
            .to_string()
            .bytes()
            .map(|b| if b.is_ascii_alphanumeric() { char::from(b).to_string() } else { format!("%{b:02X}") })
            .collect();
        self.slack(name, format!("payload={encoded}").into_bytes(), "application/x-www-form-urlencoded").await.0
    }

    /// Post an activity to the Teams channel with a token for `service_url`.
    async fn teams(&self, activity: &Json, token: &str) -> StatusCode {
        let headers = [("authorization", format!("Bearer {token}"))];
        self.post("/v1/channels/teams/webhook", &headers, "application/json", activity.to_string().into_bytes()).await.0
    }

    async fn run_status(&self, run_id: &str) -> String {
        let (_, run) = self.api("GET", &format!("/v1/runs/{run_id}"), Json::Null).await;
        run["status"].as_str().unwrap_or_default().to_owned()
    }
}

const SLACK_SOCKET: &str =
    "channels:\n  - { kind: slack, workflow: echo, app_token_env: SOCKET_APP_TOKEN, api_base: \"{SLACK}\" }\n";

fn slack_webhook(workflow: &str, more: &str) -> String {
    format!(
        "channels:\n  - kind: slack\n    workflow: {workflow}\n    mode: webhook\n    api_base: \"{{SLACK}}\"\n{more}"
    )
}

const TEAMS_CHANNEL: &str = "  - kind: teams\n    workflow: WORKFLOW\n    api_base: \"{TEAMS}\"\n    login_base: \"{TEAMS}/login\"\n    openid_configuration: \"{TEAMS}/openid\"\n";

fn teams_channel(workflow: &str, more: &str) -> String {
    format!("channels:\n{}{more}", TEAMS_CHANNEL.replace("WORKFLOW", workflow))
}

// ---------------------------------------------------------------- Slack

#[tokio::test(flavor = "multi_thread")]
async fn slack_socket_mode_answers_dms_and_mentions_in_their_threads() {
    let (slack, base) = fake_slack().await;
    let server = start("slack-socket", SLACK_SOCKET, &base, "").await;
    assert_eq!(
        server.state.channels().unwrap().describe()[0],
        "channel `slack` (slack) -> echo (Socket Mode, no public address needed)"
    );

    let envelope = |id: &str, payload: Json| json!({"envelope_id": id, "type": "events_api", "payload": payload});
    slack.envelopes.lock().unwrap().push(envelope("e1", dm("U1", "1700000000.000100", "Halo & <https://a.example|a>")));
    until("the DM answer", || slack.posted_to("DU1", None).len() == 1).await;
    assert!(slack.acks.lock().unwrap().contains(&"e1".to_owned()), "the envelope was acknowledged");
    let answer = &slack.posted_to("DU1", None)[0];
    let section = answer["blocks"][0]["text"]["text"].as_str().unwrap();
    assert_eq!(section, "You said: *Halo &amp; a (https://a.example)* [slack:U1 ch-slack-DU1 slack]");

    // A mention in a channel is answered in a thread, and the thread is one conversation.
    slack.envelopes.lock().unwrap().push(envelope("e2", mention("C1", "U2", "1700000100.000100", None, "first")));
    until("the thread answer", || slack.posted_to("C1", Some("1700000100.000100")).len() == 1).await;
    slack
        .envelopes
        .lock()
        .unwrap()
        .push(envelope("e3", mention("C1", "U3", "1700000200.000100", Some("1700000100.000100"), "second")));
    until("the second answer in the thread", || slack.posted_to("C1", Some("1700000100.000100")).len() == 2).await;
    let (_, conversation) = server.api("GET", "/v1/conversations/ch-slack-C1_1700000100_000100", Json::Null).await;
    assert_eq!(conversation["messages"].as_array().map(Vec::len), Some(4), "{conversation}");
    // Plain channel messages are not for the app, and a redelivery is answered once.
    let plain = slack_event(
        json!({"type": "message", "channel": "C1", "channel_type": "channel", "user": "U2", "text": "lunch?", "ts": "1700000300.000100"}),
    );
    slack.envelopes.lock().unwrap().push(envelope("e4", plain));
    slack.envelopes.lock().unwrap().push(envelope("e5", dm("U1", "1700000000.000100", "Halo & <https://a.example|a>")));
    until("both acknowledged", || slack.acks.lock().unwrap().len() >= 5).await;
    settle().await;
    assert_eq!(slack.posts().len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_slack_events_api_takes_only_signed_fresh_deliveries() {
    let (slack, base) = fake_slack().await;
    let server = start("slack-events", &slack_webhook("echo", ""), &base, "").await;
    let challenge = br#"{"type":"url_verification","challenge":"abc123","token":"x"}"#.to_vec();
    let (status, body) = server.slack("slack", challenge.clone(), "application/json").await;
    assert_eq!((status, body.as_slice()), (StatusCode::OK, &b"abc123"[..]));

    let delivery = serde_json::to_vec(&dm("U1", "1700000000.000100", "hi")).unwrap();
    let unsigned = server.post("/v1/channels/slack/webhook", &[], "application/json", delivery.clone()).await.0;
    assert_eq!(unsigned, StatusCode::UNAUTHORIZED);
    let old = (now_secs() - 600).to_string();
    let signature = graphrt_channel::slack::sign(&delivery, &old, &Secret::new(SLACK_SIGNING_SECRET));
    let stale = [("x-slack-request-timestamp", old), ("x-slack-signature", signature)];
    assert_eq!(
        server.post("/v1/channels/slack/webhook", &stale, "application/json", delivery.clone()).await.0,
        StatusCode::UNAUTHORIZED
    );
    settle().await;
    assert!(slack.posts().is_empty(), "nothing unsigned or replayed was answered");

    assert_eq!(server.slack("slack", delivery.clone(), "application/json").await.0, StatusCode::OK);
    // Slack retries when it did not hear back in time: the same message is answered once.
    assert_eq!(server.slack("slack", delivery, "application/json").await.0, StatusCode::OK);
    until("the answer", || slack.posted_to("DU1", None).len() == 1).await;
    settle().await;
    assert_eq!(slack.posts().len(), 1);
    let (_, list) = server.api("GET", "/v1/channels", Json::Null).await;
    assert_eq!(
        (list["channels"][0]["mode"].as_str(), list["channels"][0]["webhook"].as_str()),
        (Some("webhook"), Some("/v1/channels/slack/webhook"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slack_approval_is_a_button_that_resumes_the_run() {
    let (slack, base) = fake_slack().await;
    let server = start("slack-approve", &slack_webhook("refund", "    approvers: [\"U7\"]\n"), &base, "").await;
    let ask = serde_json::to_vec(&mention("C1", "U2", "1700000100.000100", None, "refund please")).unwrap();
    assert_eq!(server.slack("slack", ask, "application/json").await.0, StatusCode::OK);
    until("the notices", || {
        !slack.posted_to("C1", Some("1700000100.000100")).is_empty() && !slack.posted_to("U7", None).is_empty()
    })
    .await;
    assert!(slack.texts("C1", Some("1700000100.000100"))[0].contains("Someone from our team"));
    let request = &slack.posted_to("U7", None)[0];
    let blocks = request["blocks"].as_array().unwrap();
    assert!(blocks[0]["text"]["text"].as_str().unwrap().starts_with("*Approval needed*"), "{request}");
    let actions = blocks.iter().find(|block| block["type"] == "actions").expect("buttons");
    let titles: Vec<&str> =
        actions["elements"].as_array().unwrap().iter().map(|b| b["text"]["text"].as_str().unwrap()).collect();
    assert_eq!(titles, ["Approve", "Reject"]);
    assert_eq!(actions["elements"][0]["value"], "1");

    // Someone who is not an approver cannot answer by tapping a copy.
    assert_eq!(server.slack_tap("slack", &tap("U9", "DU9", "1700009999.000001", "1", "Approve")).await, StatusCode::OK);
    settle().await;
    assert!(!slack.texts("C1", Some("1700000100.000100")).iter().any(|text| text == "refunded"));

    assert_eq!(server.slack_tap("slack", &tap("U7", "DU7", "1700009999.000001", "1", "Approve")).await, StatusCode::OK);
    until("the released answer", || slack.texts("C1", Some("1700000100.000100")).iter().any(|text| text == "refunded"))
        .await;
    until("the approver's receipt", || slack.texts("DU7", None).iter().any(|text| text.contains("The run continues")))
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_card_answer_is_block_kit_in_slack_and_an_adaptive_card_in_teams() {
    let (slack, slack_base) = fake_slack().await;
    let (teams, teams_base) = fake_teams().await;
    let channels = format!("{}{}", slack_webhook("card", ""), TEAMS_CHANNEL.replace("WORKFLOW", "card"));
    let server = start("cards", &channels, &slack_base, &teams_base).await;

    let ask = serde_json::to_vec(&dm("U1", "1700000000.000100", "my order")).unwrap();
    assert_eq!(server.slack("slack", ask, "application/json").await.0, StatusCode::OK);
    until("the Slack card", || !slack.posted_to("DU1", None).is_empty()).await;
    let blocks = slack.posted_to("DU1", None)[0]["blocks"].clone();
    let kinds: Vec<&str> = blocks.as_array().unwrap().iter().filter_map(|b| b["type"].as_str()).collect();
    assert_eq!(kinds, ["header", "section", "actions"], "{blocks}");
    assert_eq!(blocks[0]["text"]["text"], "Order #1042");
    assert_eq!(blocks[2]["elements"][0]["value"], "1");
    assert_eq!(blocks[2]["elements"][1]["url"], "https://shop.example/r/1042");

    let service = format!("{teams_base}/");
    let hello = teams_message(&service, personal("a:1customer"), "aad-customer", "m1", "my order");
    assert_eq!(server.teams(&hello, &connector_token(&service, TEAMS_APP_ID)).await, StatusCode::OK);
    until("the Teams card", || !teams.cards_to("a:1customer").is_empty()).await;
    let card = &teams.cards_to("a:1customer")[0];
    assert_eq!(card["body"][0]["text"], "Order #1042");
    let titles: Vec<&str> = card["actions"].as_array().unwrap().iter().filter_map(|a| a["title"].as_str()).collect();
    assert_eq!(titles, ["Receipt", "Track it"]);
    assert_eq!(card["actions"][1]["data"]["graphrt_choice"], "1");
}

// ---------------------------------------------------------------- Teams

#[tokio::test(flavor = "multi_thread")]
async fn teams_takes_only_activities_with_a_valid_connector_token() {
    let (teams, base) = fake_teams().await;
    let server = start("teams-auth", &teams_channel("echo", ""), "", &base).await;
    let service = format!("{base}/");
    let hello = teams_message(&service, personal("a:1customer"), "aad-customer", "m1", "Halo");

    assert_eq!(server.teams(&hello, "not-a-token").await, StatusCode::UNAUTHORIZED);
    assert_eq!(server.teams(&hello, &connector_token(&service, "another-bot")).await, StatusCode::UNAUTHORIZED);
    // A token for another service address cannot send answers somewhere else.
    let elsewhere = connector_token("https://smba.trafficmanager.net/emea/", TEAMS_APP_ID);
    assert_eq!(server.teams(&hello, &elsewhere).await, StatusCode::UNAUTHORIZED);
    let mut stranger = hello.clone();
    stranger["channelData"]["tenant"]["id"] = json!("99999999-0000-0000-0000-000000000000");
    stranger["conversation"]["tenantId"] = json!("99999999-0000-0000-0000-000000000000");
    assert_eq!(server.teams(&stranger, &connector_token(&service, TEAMS_APP_ID)).await, StatusCode::FORBIDDEN);
    settle().await;
    assert!(teams.activities.lock().unwrap().is_empty(), "nothing unverified was answered");

    let token = connector_token(&service, TEAMS_APP_ID);
    assert_eq!(server.teams(&hello, &token).await, StatusCode::OK);
    assert_eq!(server.teams(&hello, &token).await, StatusCode::OK, "a redelivery");
    until("the answer", || teams.texts_to("a:1customer").len() == 1).await;
    assert_eq!(teams.texts_to("a:1customer")[0], "You said: **Halo** [teams:aad-customer ch-teams-a_1customer teams]");
    assert!(teams.sent_to("a:1customer").iter().any(|a| a["type"] == "typing"), "the person saw the bot typing");
    settle().await;
    assert_eq!(teams.texts_to("a:1customer").len(), 1);
    assert_eq!(teams.token_requests.load(Ordering::Relaxed), 1, "the access token is reused");

    // A mention in a channel is answered in its thread, without the mention.
    let thread = "19:abc@thread.tacv2;messageid=1700000000000";
    let channel = json!({"id": thread, "conversationType": "channel", "tenantId": TEAMS_TENANT});
    let mention = teams_message(&service, channel, "aad-colleague", "m2", "<at>Support Bot</at> where is my order?");
    assert_eq!(server.teams(&mention, &token).await, StatusCode::OK);
    until("the thread answer", || teams.texts_to(thread).len() == 1).await;
    assert!(teams.texts_to(thread)[0].starts_with("You said: **where is my order?**"), "{:?}", teams.texts_to(thread));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_teams_approval_is_an_adaptive_card_and_its_tap_resumes_the_run() {
    let (teams, base) = fake_teams().await;
    let server =
        start("teams-approve", &teams_channel("refund", "    approvers: [\"aad-approver\"]\n"), "", &base).await;
    let service = format!("{base}/");
    let token = connector_token(&service, TEAMS_APP_ID);
    let ask = teams_message(&service, personal("a:1customer"), "aad-customer", "m1", "refund please");
    assert_eq!(server.teams(&ask, &token).await, StatusCode::OK);
    // The approver never wrote to the bot: it opens a chat with them.
    until("the approval card", || !teams.cards_to("a:opened-aad-approver").is_empty()).await;
    assert_eq!(teams.opened.lock().unwrap()[0]["tenantId"], TEAMS_TENANT);
    assert!(teams.texts_to("a:1customer")[0].contains("Someone from our team"));
    let card = &teams.cards_to("a:opened-aad-approver")[0];
    let titles: Vec<&str> = card["actions"].as_array().unwrap().iter().filter_map(|a| a["title"].as_str()).collect();
    assert_eq!(titles, ["Approve", "Reject"]);

    let mut tapped = teams_message(&service, personal("a:opened-aad-approver"), "aad-approver", "m2", "");
    tapped["value"] = card["actions"][1]["data"].clone();
    tapped["replyToId"] = json!("act-1");
    assert_eq!(server.teams(&tapped, &token).await, StatusCode::OK);
    until("the released answer", || teams.texts_to("a:1customer").iter().any(|text| text == "rejected")).await;
    until("the card marked as answered", || !teams.updates.lock().unwrap().is_empty()).await;
    let (conversation, id, update) = teams.updates.lock().unwrap()[0].clone();
    assert_eq!((conversation.as_str(), id.as_str()), ("a:opened-aad-approver", "act-1"));
    assert_eq!(update["text"], "**Reject**");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_teams_file_goes_through_a_consent_card_in_a_chat_and_as_a_link_in_a_channel() {
    let (teams, base) = fake_teams().await;
    let public = "https://agent.example.com";
    let server = start_with("teams-files", &teams_channel("note", ""), "", &base, Some(public)).await;
    let service = format!("{base}/");
    let token = connector_token(&service, TEAMS_APP_ID);

    // A 1:1 chat: the person is asked whether the bot may put the file in their OneDrive.
    let ask = teams_message(&service, personal("a:1customer"), "aad-customer", "m1", "my order");
    assert_eq!(server.teams(&ask, &token).await, StatusCode::OK);
    let consent_card = |teams: &FakeTeams| {
        teams.sent_to("a:1customer").iter().enumerate().find_map(|(index, activity)| {
            let attachment = activity["attachments"][0].clone();
            (attachment["contentType"] == "application/vnd.microsoft.teams.card.file.consent")
                .then_some((index, attachment))
        })
    };
    until("the consent card", || consent_card(&teams).is_some()).await;
    assert!(teams.texts_to("a:1customer").contains(&"Here is your note.".to_owned()));
    let (_, card) = consent_card(&teams).unwrap();
    assert_eq!(card["name"], "summary.md");
    let upload_id = card["content"]["acceptContext"]["graphrt_upload"].as_str().unwrap().to_owned();
    assert_eq!(upload_id.len(), 32);

    let invoke = |action: &str, id: &str| {
        json!({"type": "invoke", "name": "fileConsent/invoke", "id": format!("inv-{action}"), "channelId": "msteams",
            "serviceUrl": service, "replyToId": "act-consent",
            "from": {"id": "29:aad-customer", "aadObjectId": "aad-customer"},
            "conversation": {"id": "a:1customer", "conversationType": "personal", "tenantId": TEAMS_TENANT},
            "channelData": {"tenant": {"id": TEAMS_TENANT}},
            "value": {"type": "fileUpload", "action": action, "context": {"graphrt_upload": id},
                      "uploadInfo": {"name": "summary.md", "uploadUrl": format!("{base}/upload/1"),
                                     "contentUrl": "https://contoso-my.sharepoint.com/summary.md", "uniqueId": "u-1", "fileType": "md"}}})
    };
    // Another chat cannot claim the file by its id.
    let mut foreign = invoke("accept", &upload_id);
    foreign["conversation"]["id"] = json!("a:1someone-else");
    assert_eq!(server.teams(&foreign, &token).await, StatusCode::OK);
    settle().await;
    assert!(teams.uploads.lock().unwrap().is_empty());

    assert_eq!(server.teams(&invoke("accept", &upload_id), &token).await, StatusCode::OK);
    until("the upload", || !teams.uploads.lock().unwrap().is_empty()).await;
    let (range, bytes) = teams.uploads.lock().unwrap()[0].clone();
    assert_eq!(String::from_utf8_lossy(&bytes), "Summary for the customer: my order");
    assert_eq!(range, format!("bytes 0-{}/{}", bytes.len() - 1, bytes.len()));
    until("the file card", || {
        teams
            .sent_to("a:1customer")
            .iter()
            .any(|a| a["attachments"][0]["contentType"] == "application/vnd.microsoft.teams.card.file.info")
    })
    .await;
    let file_card = teams
        .sent_to("a:1customer")
        .into_iter()
        .find(|a| a["attachments"][0]["contentType"] == "application/vnd.microsoft.teams.card.file.info")
        .unwrap();
    assert_eq!(file_card["attachments"][0]["contentUrl"], "https://contoso-my.sharepoint.com/summary.md");
    until("the consent card replaced", || {
        teams
            .updates
            .lock()
            .unwrap()
            .iter()
            .any(|(_, id, update)| id == "act-consent" && update["text"] == "Sent: summary.md")
    })
    .await;

    // A channel takes no file from a bot: it gets a link that works without a key.
    let thread = "19:files@thread.tacv2;messageid=1700000000000";
    let channel = json!({"id": thread, "conversationType": "channel", "tenantId": TEAMS_TENANT});
    let mention = teams_message(&service, channel, "aad-colleague", "m2", "<at>Bot</at> the report");
    assert_eq!(server.teams(&mention, &token).await, StatusCode::OK);
    let link = |teams: &FakeTeams| teams.texts_to(thread).into_iter().find(|text| text.starts_with("summary.md: "));
    until("the link", || link(&teams).is_some()).await;
    let url = link(&teams).unwrap().trim_start_matches("summary.md: ").to_owned();
    let path = url.strip_prefix(public).expect("a link on the public address").to_owned();
    assert!(path.starts_with("/v1/channels/teams/files/"), "{path}");
    let (status, body) = server.request(Request::get(&path).body(Body::empty()).unwrap()).await;
    assert_eq!(
        (status, String::from_utf8_lossy(&body).to_string()),
        (StatusCode::OK, "Summary for the customer: the report".to_owned())
    );
    let (status, _) = server
        .request(Request::get("/v1/channels/teams/files/0123456789abcdef0123456789abcdef").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let other_channel = path.replace("/channels/teams/", "/channels/slack/");
    assert_eq!(
        server.request(Request::get(&other_channel).body(Body::empty()).unwrap()).await.0,
        StatusCode::NOT_FOUND
    );
}

// ---------------------------------------------------------------- notifications

#[tokio::test(flavor = "multi_thread")]
async fn failed_runs_and_approvals_of_any_run_are_announced_in_slack_and_teams() {
    let (slack, slack_base) = fake_slack().await;
    let (teams, teams_base) = fake_teams().await;
    // Channels that run no workflow of their own: they only tell people.
    let channels = format!(
        "{}{}",
        "channels:\n  - kind: slack\n    name: ops\n    mode: webhook\n    api_base: \"{SLACK}\"\n    notify: [\"C0OPS\", \"U7\"]\n    approvers: [\"U7\"]\n",
        "  - kind: teams\n    name: teams\n    api_base: \"{TEAMS}\"\n    login_base: \"{TEAMS}/login\"\n    openid_configuration: \"{TEAMS}/openid\"\n    notify: [\"aad-lead\"]\n"
    );
    let server = start("notify", &channels, &slack_base, &teams_base).await;

    let _failed = server.api("POST", "/v1/workflows/broken/runs", json!({"inputs": {}})).await;
    until("the failure notices", || {
        !slack.texts("C0OPS", None).is_empty() && !teams.texts_to("a:opened-aad-lead").is_empty()
    })
    .await;
    let notice = &slack.texts("C0OPS", None)[0];
    assert!(notice.starts_with("*Run failed: broken*"), "{notice}");
    assert!(notice.contains("warehouse API is down"), "{notice}");
    assert!(teams.texts_to("a:opened-aad-lead")[0].starts_with("**Run failed: broken**"));

    // A run started over the API waits: the approver gets buttons, the others a notice.
    let (_, paused) = server.api("POST", "/v1/workflows/refund/runs", json!({"inputs": {}})).await;
    let run_id = paused["run_id"].as_str().expect("a run id").to_owned();
    until("the approval notices", || slack.texts("C0OPS", None).len() == 2 && !slack.posted_to("U7", None).is_empty())
        .await;
    assert!(slack.texts("C0OPS", None)[1].contains("approvals inbox"));
    let request =
        slack.posted_to("U7", None).iter().find(|post| post["blocks"].to_string().contains("graphrt_choice")).cloned();
    let request = request.expect("the approver's buttons");
    assert!(request["blocks"][0]["text"]["text"].as_str().unwrap().contains("Approval needed: refund"));
    assert_eq!(server.slack_tap("ops", &tap("U7", "DU7", "1700009999.000002", "1", "Approve")).await, StatusCode::OK);
    let mut status = String::new();
    for _ in 0..250 {
        status = server.run_status(&run_id).await;
        if status == "succeeded" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(status, "succeeded", "the tap resumed the run");
    until("the approver's receipt", || slack.texts("DU7", None).iter().any(|text| text.contains("The run continues")))
        .await;
}
