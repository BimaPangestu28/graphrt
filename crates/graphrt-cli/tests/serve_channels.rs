//! `graphrt serve --channel`: a customer writes on Telegram or WhatsApp, the
//! workflow runs and the answer goes back, against fake Telegram and WhatsApp
//! servers that record every call. No model is involved: the workflows answer
//! from templates, so what is checked is the path of a message, not its words.
#![cfg(all(feature = "channel-telegram", feature = "channel-whatsapp", feature = "files", feature = "code"))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json as JsonBody;
use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use graphrt_channel::Secret;
use graphrt_cli::approvals::Approvals;
use graphrt_cli::channels::{Channels, Defaults, config};
use graphrt_cli::conversations::{ConversationStore, Conversations, Limits, MemoryConversations};
use graphrt_cli::serve::{AppState, build_state, discover, router};
use graphrt_cli::setup::{ServiceOptions, WorkspaceOptions};
use graphrt_core::engine::Limits as EngineLimits;
use http_body_util::BodyExt;
use serde_json::{Value as Json, json};
use tower::ServiceExt;

const API_KEY: &str = "server-only-key";
const BOT_TOKEN: &str = "123456:AAE-fake_bot_token";
const WEBHOOK_SECRET: &str = "telegram-webhook-secret_1";
const WA_TOKEN: &str = "EAAG-fake-access-token";
const WA_PHONE_ID: &str = "106540352242922";
const WA_APP_SECRET: &str = "fake-app-secret";
const WA_VERIFY_TOKEN: &str = "fake-verify-token";

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("graphrt-channels-{tag}-{}", std::process::id()));
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

/// Wait until `ready` holds, at most ten seconds.
async fn until(what: &str, ready: impl Fn() -> bool) {
    for _ in 0..500 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// Long enough for a reply that must not come to have come.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(400)).await;
}

// ---------------------------------------------------------------- fake Telegram

#[derive(Default)]
struct FakeTelegram {
    /// Every JSON call: the method and its parameters.
    calls: Mutex<Vec<(String, Json)>>,
    /// Every multipart call: the method and the raw body.
    uploads: Mutex<Vec<(String, Vec<u8>)>>,
    /// Updates `getUpdates` still has to hand out.
    updates: Mutex<Vec<Json>>,
    /// Files by `file_id`.
    files: Mutex<HashMap<String, Vec<u8>>>,
    /// Refuse `parse_mode: HTML`, as Telegram does for markup it cannot read.
    refuse_html: AtomicBool,
    next_id: AtomicU64,
}

impl FakeTelegram {
    fn calls_of(&self, method: &str) -> Vec<Json> {
        self.calls.lock().unwrap().iter().filter(|(name, _)| name == method).map(|(_, body)| body.clone()).collect()
    }

    /// The texts sent to `chat_id`, in order.
    fn sent_to(&self, chat_id: &str) -> Vec<String> {
        self.calls_of("sendMessage")
            .iter()
            .filter(|call| call["chat_id"] == chat_id)
            .map(|call| call["text"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    fn push(&self, update: Json) {
        self.updates.lock().unwrap().push(update);
    }

    fn next(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// A text message of `user` in their private chat, as a new update.
    fn text_update(&self, user: i64, message_id: u64, text: &str) -> Json {
        json!({
            "update_id": self.next(),
            "message": {
                "message_id": message_id, "date": 1_700_000_000, "text": text,
                "chat": {"id": user, "type": "private"},
                "from": {"id": user, "is_bot": false, "first_name": "Sari"},
            },
        })
    }
}

async fn telegram_method(
    State(fake): State<Arc<FakeTelegram>>,
    UrlPath((bot, method)): UrlPath<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if bot != format!("bot{BOT_TOKEN}") {
        return (
            StatusCode::UNAUTHORIZED,
            JsonBody(json!({"ok": false, "error_code": 401, "description": "Unauthorized"})),
        )
            .into_response();
    }
    let is_json = headers.get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("json"));
    if !is_json {
        fake.uploads.lock().unwrap().push((method, body.to_vec()));
        return JsonBody(json!({"ok": true, "result": {"message_id": fake.next()}})).into_response();
    }
    let params: Json = serde_json::from_slice(&body).unwrap_or(Json::Null);
    fake.calls.lock().unwrap().push((method.clone(), params.clone()));
    let result = match method.as_str() {
        "getWebhookInfo" => json!({"url": ""}),
        "getUpdates" => {
            let offset = params["offset"].as_u64().unwrap_or(0);
            let pending: Vec<Json> = {
                let mut updates = fake.updates.lock().unwrap();
                updates.retain(|update| update["update_id"].as_u64().unwrap_or(0) >= offset);
                updates.clone()
            };
            if pending.is_empty() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Json::Array(pending)
        }
        "sendMessage" => {
            if fake.refuse_html.load(Ordering::Relaxed) && params["parse_mode"] == "HTML" {
                let refusal = json!({"ok": false, "error_code": 400,
                    "description": "Bad Request: can't parse entities: Unsupported start tag"});
                return (StatusCode::BAD_REQUEST, JsonBody(refusal)).into_response();
            }
            json!({"message_id": fake.next()})
        }
        "getFile" => {
            let file_id = params["file_id"].as_str().unwrap_or_default();
            let size = fake.files.lock().unwrap().get(file_id).map(Vec::len).unwrap_or(0);
            json!({"file_id": file_id, "file_size": size, "file_path": format!("documents/{file_id}")})
        }
        _ => json!(true),
    };
    JsonBody(json!({"ok": true, "result": result})).into_response()
}

async fn telegram_file(
    State(fake): State<Arc<FakeTelegram>>,
    UrlPath((bot, _dir, file_id)): UrlPath<(String, String, String)>,
) -> Response {
    let found = fake.files.lock().unwrap().get(&file_id).cloned();
    match found {
        Some(bytes) if bot == format!("bot{BOT_TOKEN}") => bytes.into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Start `app` on a free loopback port and return its address.
async fn listen(app: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ended = axum::serve(listener, app).await;
    });
    address
}

async fn fake_telegram() -> (Arc<FakeTelegram>, String) {
    let fake = Arc::new(FakeTelegram::default());
    let app = axum::Router::new()
        .route("/file/{bot}/{dir}/{file}", get(telegram_file))
        .route("/{bot}/{method}", post(telegram_method))
        .with_state(Arc::clone(&fake));
    (fake, listen(app).await)
}

// ---------------------------------------------------------------- fake WhatsApp

#[derive(Default)]
struct FakeWhatsApp {
    /// Every body posted to `/messages`.
    messages: Mutex<Vec<Json>>,
    /// Every raw body posted to `/media`.
    uploads: Mutex<Vec<Vec<u8>>>,
    /// Media a customer sent, by id.
    media: Mutex<HashMap<String, Vec<u8>>>,
    base: Mutex<String>,
    next_id: AtomicU64,
}

impl FakeWhatsApp {
    /// The bodies of the text messages sent to `to`.
    fn texts_to(&self, to: &str) -> Vec<String> {
        self.messages
            .lock()
            .unwrap()
            .iter()
            .filter(|message| message["to"] == to && message["type"] == "text")
            .map(|message| message["text"]["body"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    fn of_type(&self, kind: &str) -> Vec<Json> {
        self.messages.lock().unwrap().iter().filter(|message| message["type"] == kind).cloned().collect()
    }
}

fn wa_authorized(headers: &HeaderMap) -> bool {
    headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {WA_TOKEN}"))
}

async fn wa_messages(State(fake): State<Arc<FakeWhatsApp>>, headers: HeaderMap, body: Bytes) -> Response {
    if !wa_authorized(&headers) {
        return (StatusCode::UNAUTHORIZED, JsonBody(json!({"error": {"message": "Invalid token", "code": 190}})))
            .into_response();
    }
    let message: Json = serde_json::from_slice(&body).unwrap_or(Json::Null);
    fake.messages.lock().unwrap().push(message);
    let id = fake.next_id.fetch_add(1, Ordering::Relaxed);
    JsonBody(json!({"messaging_product": "whatsapp", "messages": [{"id": format!("wamid.OUT{id}")}]})).into_response()
}

async fn wa_upload(State(fake): State<Arc<FakeWhatsApp>>, body: Bytes) -> Response {
    fake.uploads.lock().unwrap().push(body.to_vec());
    JsonBody(json!({"id": "7001"})).into_response()
}

async fn wa_media_info(
    State(fake): State<Arc<FakeWhatsApp>>,
    UrlPath((_version, media_id)): UrlPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let size = fake.media.lock().unwrap().get(&media_id).map(Vec::len);
    match size {
        Some(size) if wa_authorized(&headers) => {
            let url = format!("{}/download/{media_id}", fake.base.lock().unwrap());
            JsonBody(json!({"url": url, "mime_type": "image/png", "file_size": size, "id": media_id})).into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn wa_download(
    State(fake): State<Arc<FakeWhatsApp>>,
    UrlPath(media_id): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    let found = fake.media.lock().unwrap().get(&media_id).cloned();
    match found {
        Some(bytes) if wa_authorized(&headers) => bytes.into_response(),
        _ => StatusCode::UNAUTHORIZED.into_response(),
    }
}

async fn fake_whatsapp() -> (Arc<FakeWhatsApp>, String) {
    let fake = Arc::new(FakeWhatsApp::default());
    let app = axum::Router::new()
        .route("/{version}/{phone}/messages", post(wa_messages))
        .route("/{version}/{phone}/media", post(wa_upload))
        .route("/{version}/{media}", get(wa_media_info))
        .route("/download/{media}", get(wa_download))
        .with_state(Arc::clone(&fake));
    let base = listen(app).await;
    *fake.base.lock().unwrap() = base.clone();
    (fake, base)
}

/// A WhatsApp delivery with these `messages` (or `statuses`) for the test number.
fn wa_delivery(field: &str, items: Json) -> Vec<u8> {
    let mut value = json!({
        "messaging_product": "whatsapp",
        "metadata": {"display_phone_number": "15550001111", "phone_number_id": WA_PHONE_ID},
        "contacts": [{"profile": {"name": "Budi"}, "wa_id": "6281234567890"}],
    });
    value[field] = items;
    let payload = json!({"object": "whatsapp_business_account",
        "entry": [{"id": "biz", "changes": [{"field": "messages", "value": value}]}]});
    serde_json::to_vec(&payload).unwrap()
}

fn wa_text(id: &str, text: &str) -> Vec<u8> {
    wa_delivery(
        "messages",
        json!([{"from": "6281234567890", "id": id, "timestamp": "1700000000", "type": "text", "text": {"body": text}}]),
    )
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
    - { id: done, type: answer, answer: "You said: {{#sys.query#}} [{{#sys.user_id#}} {{#sys.conversation_id#}} {{#sys.channel#}}]" }
  edges:
    - { source: start, target: done }
"#,
    );
    // Three times the message, with Markdown around it: long answers and formatting.
    write(
        "long",
        r#"version: "1"
name: long
graph:
  nodes:
    - { id: start, type: start }
    - { id: done, type: answer, answer: "**Summary** of <your> text\n\n{{#sys.query#}}\n\n{{#sys.query#}}\n\n{{#sys.query#}}" }
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
    // Slow enough for further messages to arrive while it runs.
    write(
        "slow",
        r#"version: "1"
name: slow
graph:
  nodes:
    - { id: start, type: start }
    - id: wait
      type: code
      code_language: javascript
      code: |
        function main({ query }) {
          const end = Date.now() + 400;
          while (Date.now() < end) {}
          return { said: query };
        }
      variables: [{ variable: query, value_selector: [sys, query] }]
      outputs: { said: { type: string } }
    - { id: done, type: answer, answer: "Heard: {{#wait.said#}}" }
  edges:
    - { source: start, target: wait }
    - { source: wait, target: done }
"#,
    );
    // Says what files came in.
    write(
        "inbox",
        r#"version: "1"
name: inbox
graph:
  nodes:
    - { id: start, type: start }
    - id: look
      type: code
      code_language: javascript
      code: |
        function main({ files }) {
          const list = Array.isArray(files) ? files : [];
          return { summary: list.length + " file(s): " + list.map((f) => f.name + " " + f.mime_type).join(", ") };
        }
      variables: [{ variable: files, value_selector: [sys, files] }]
      outputs: { summary: { type: string } }
    - { id: done, type: answer, answer: "{{#look.summary#}}" }
  edges:
    - { source: start, target: look }
    - { source: look, target: done }
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
    flows
}

/// The server under test.
struct Server {
    state: Arc<AppState>,
    app: axum::Router,
    conversations: Arc<MemoryConversations>,
    _dir: TempDir,
}

#[derive(Default)]
struct Setup {
    /// The channels file, with `{TELEGRAM}` and `{WHATSAPP}` for the fakes' addresses.
    channels: String,
    customer_answers: bool,
    workspace: bool,
    public_url: Option<String>,
}

fn lookup(name: &str) -> Option<String> {
    let value = match name {
        "TELEGRAM_BOT_TOKEN" => BOT_TOKEN,
        "TELEGRAM_WEBHOOK_SECRET" => WEBHOOK_SECRET,
        "WHATSAPP_ACCESS_TOKEN" => WA_TOKEN,
        "WHATSAPP_PHONE_NUMBER_ID" => WA_PHONE_ID,
        "WHATSAPP_APP_SECRET" => WA_APP_SECRET,
        "WHATSAPP_VERIFY_TOKEN" => WA_VERIFY_TOKEN,
        _ => return None,
    };
    Some(value.to_owned())
}

async fn start(tag: &str, setup: Setup, telegram: &str, whatsapp: &str) -> Server {
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
        workspace: setup
            .workspace
            .then(|| WorkspaceOptions { dir: dir.path().join("workspace"), ..WorkspaceOptions::default() }),
        ..ServiceOptions::default()
    };
    let channels_file = dir.path().join("channels.yaml");
    std::fs::write(&channels_file, setup.channels.replace("{TELEGRAM}", telegram).replace("{WHATSAPP}", whatsapp))
        .unwrap();
    let configs = config::load(&[], Some(&channels_file), Defaults::default(), &lookup).expect("channels file");
    let state = build_state(&files, &options, EngineLimits::default(), Some(API_KEY.into())).await.expect("state");
    let channels = Channels::new(configs, |name| state.serves(name), setup.public_url, setup.customer_answers)
        .expect("channels")
        .map(Arc::new);
    let conversations = Arc::new(MemoryConversations::default());
    let store: Arc<dyn ConversationStore> = conversations.clone();
    let state = Arc::new(
        state
            .with_conversations(Some(Conversations::with_store(store, Limits::default())))
            .with_approvals(Some(Approvals { expire: false }))
            .with_channels(channels),
    );
    state.channels().expect("channels").start(&state);
    Server { app: router(Arc::clone(&state), 1 << 20), state, conversations, _dir: dir }
}

impl Server {
    async fn request(&self, request: Request<Body>) -> (StatusCode, Vec<u8>) {
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        (status, response.into_body().collect().await.unwrap().to_bytes().to_vec())
    }

    async fn post(&self, path: &str, headers: &[(&str, &str)], body: Vec<u8>) -> (StatusCode, Vec<u8>) {
        let mut request = Request::post(path).header("content-type", "application/json");
        for (name, value) in headers {
            request = request.header(*name, *value);
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

    /// Post a signed WhatsApp delivery.
    async fn whatsapp(&self, body: Vec<u8>) -> StatusCode {
        let signature = graphrt_channel::whatsapp::sign(&body, &Secret::new(WA_APP_SECRET));
        self.post("/v1/channels/whatsapp/webhook", &[("x-hub-signature-256", &signature)], body).await.0
    }

    /// Post a Telegram update with the webhook secret.
    async fn telegram(&self, update: &Json) -> StatusCode {
        let headers = [("x-telegram-bot-api-secret-token", WEBHOOK_SECRET)];
        self.post("/v1/channels/telegram/webhook", &headers, update.to_string().into_bytes()).await.0
    }

    async fn conversation(&self, id: &str) -> Json {
        self.api("GET", &format!("/v1/conversations/{id}"), Json::Null).await.1
    }
}

const TELEGRAM_POLLING: &str =
    "channels:\n  - { kind: telegram, workflow: echo, mode: polling, api_base: \"{TELEGRAM}\" }\n";
const WHATSAPP_ECHO: &str = "channels:\n  - { kind: whatsapp, workflow: echo, api_base: \"{WHATSAPP}\" }\n";

fn telegram_channel(workflow: &str, more: &str) -> String {
    format!("channels:\n  - kind: telegram\n    workflow: {workflow}\n    api_base: \"{{TELEGRAM}}\"\n{more}")
}

// ---------------------------------------------------------------- Telegram

#[tokio::test(flavor = "multi_thread")]
async fn a_telegram_message_is_answered_in_the_customers_own_conversation() {
    let (telegram, base) = fake_telegram().await;
    let server = start("tg-poll", Setup { channels: TELEGRAM_POLLING.into(), ..Setup::default() }, &base, "").await;
    telegram.push(telegram.text_update(42, 1, "Halo"));
    until("the first answer", || telegram.sent_to("42").len() == 1).await;
    assert_eq!(telegram.sent_to("42")[0], "You said: Halo [telegram:42 ch-telegram-42 telegram]");
    assert!(!telegram.calls_of("sendChatAction").is_empty(), "the customer saw the bot typing");

    telegram.push(telegram.text_update(42, 2, "Lagi"));
    // Another customer has a conversation of their own.
    telegram.push(telegram.text_update(77, 1, "Hi"));
    until("both answers", || telegram.sent_to("42").len() == 2 && telegram.sent_to("77").len() == 1).await;
    assert!(telegram.sent_to("77")[0].contains("telegram:77 ch-telegram-77"));

    let conversation = server.conversation("ch-telegram-42").await;
    let texts: Vec<&str> =
        conversation["messages"].as_array().unwrap().iter().map(|m| m["content"].as_str().unwrap()).collect();
    assert_eq!(texts.len(), 4, "{conversation}");
    assert_eq!((texts[0], texts[2]), ("Halo", "Lagi"));
    assert_eq!(conversation["channel"]["chat_id"], "42");
    assert_eq!(conversation["channel"]["customer_name"], "Sari");
    assert_eq!(conversation["owner"], "telegram:42");

    // Telegram delivers message 2 again under a new update id: it is answered once.
    telegram.push(telegram.text_update(42, 2, "Lagi"));
    settle().await;
    assert_eq!(telegram.sent_to("42").len(), 2);
    // Polling confirmed what it handled.
    let last = telegram.calls_of("getUpdates").pop().unwrap();
    assert!(last["offset"].as_u64().unwrap() >= 4, "{last}");
    assert_eq!(last["allowed_updates"], json!(["message", "callback_query"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_telegram_webhook_takes_only_deliveries_with_its_secret() {
    let (telegram, base) = fake_telegram().await;
    let setup = Setup {
        channels: telegram_channel("echo", "    mode: webhook\n"),
        public_url: Some("https://agent.example.com".into()),
        ..Setup::default()
    };
    let server = start("tg-hook", setup, &base, "").await;
    until("the webhook registration", || !telegram.calls_of("setWebhook").is_empty()).await;
    let registered = &telegram.calls_of("setWebhook")[0];
    assert_eq!(registered["url"], "https://agent.example.com/v1/channels/telegram/webhook");
    assert_eq!(registered["secret_token"], WEBHOOK_SECRET);
    assert!(telegram.calls_of("getUpdates").is_empty(), "a webhook bot does not poll");

    let update = telegram.text_update(42, 1, "Halo");
    let body = update.to_string().into_bytes();
    let path = "/v1/channels/telegram/webhook";
    assert_eq!(server.post(path, &[], body.clone()).await.0, StatusCode::UNAUTHORIZED);
    let wrong = [("x-telegram-bot-api-secret-token", "guess")];
    assert_eq!(server.post(path, &wrong, body.clone()).await.0, StatusCode::UNAUTHORIZED);
    settle().await;
    assert!(telegram.sent_to("42").is_empty(), "an unverified delivery starts nothing");

    assert_eq!(server.telegram(&update).await, StatusCode::OK);
    until("the answer", || telegram.sent_to("42").len() == 1).await;
    // The same delivery again is acknowledged and answered once.
    assert_eq!(server.telegram(&update).await, StatusCode::OK);
    // A group is not answered unless the channel allows groups, nor is another bot.
    let mut group = telegram.text_update(42, 5, "hello all");
    group["message"]["chat"] = json!({"id": -100_500, "type": "supergroup"});
    assert_eq!(server.telegram(&group).await, StatusCode::OK);
    let mut bot = telegram.text_update(99, 1, "beep");
    bot["message"]["from"]["is_bot"] = json!(true);
    assert_eq!(server.telegram(&bot).await, StatusCode::OK);
    settle().await;
    assert_eq!(telegram.calls_of("sendMessage").len(), 1);

    let headers = [("x-telegram-bot-api-secret-token", WEBHOOK_SECRET)];
    assert_eq!(server.post(path, &headers, b"not json".to_vec()).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(server.post("/v1/channels/other/webhook", &headers, body).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_long_answer_is_split_and_formatted_for_telegram() {
    let (telegram, base) = fake_telegram().await;
    let setup = Setup { channels: telegram_channel("long", "    mode: webhook\n"), ..Setup::default() };
    let server = start("tg-long", setup, &base, "").await;
    let paragraph = "Kalimat yang cukup panjang untuk diulang. ".repeat(45);
    assert_eq!(server.telegram(&telegram.text_update(42, 1, &paragraph)).await, StatusCode::OK);
    until("the parts", || telegram.sent_to("42").len() >= 2).await;
    settle().await;
    let calls = telegram.calls_of("sendMessage");
    assert!(calls.len() >= 2, "{} part(s)", calls.len());
    for call in &calls {
        assert!(call["text"].as_str().unwrap().encode_utf16().count() <= 4096);
        assert_eq!(call["parse_mode"], "HTML");
    }
    let first = calls[0]["text"].as_str().unwrap();
    assert!(first.starts_with("<b>Summary</b> of &lt;your&gt; text"), "{first}");
    let all: String = telegram.sent_to("42").join(" ");
    assert_eq!(all.matches("Kalimat yang cukup panjang").count(), 135, "nothing was lost");

    // Telegram refuses the markup: the customer still gets the words, as plain text.
    telegram.refuse_html.store(true, Ordering::Relaxed);
    assert_eq!(server.telegram(&telegram.text_update(42, 2, "short")).await, StatusCode::OK);
    // The refused try is recorded too: wait for the plain one itself.
    let is_plain = |call: &Json| call.get("parse_mode").is_none();
    until("the plain answer", || telegram.calls_of("sendMessage").iter().any(is_plain)).await;
    let plain = telegram.calls_of("sendMessage").into_iter().rfind(|call| call.get("parse_mode").is_none()).unwrap();
    assert!(plain["text"].as_str().unwrap().starts_with("**Summary** of <your> text"), "{plain}");
}

#[tokio::test(flavor = "multi_thread")]
async fn each_customer_and_each_channel_has_a_limit_and_senders_can_be_restricted() {
    let (telegram, base) = fake_telegram().await;
    let setup = Setup {
        channels: telegram_channel("echo", "    mode: webhook\n    rate_limit: 2\n    budget: 3\n"),
        ..Setup::default()
    };
    let server = start("tg-limit", setup, &base, "").await;
    for n in 1..=4 {
        assert_eq!(server.telegram(&telegram.text_update(42, n, &format!("m{n}"))).await, StatusCode::OK);
    }
    until("two answers and one notice", || telegram.sent_to("42").len() == 3).await;
    settle().await;
    let sent = telegram.sent_to("42");
    assert_eq!(sent.len(), 3, "the fourth message gets no second notice: {sent:?}");
    assert_eq!(sent.iter().filter(|text| text.starts_with("You said")).count(), 2);
    assert!(sent.iter().any(|text| text.contains("very fast")), "{sent:?}");
    // The channel's budget: one more customer fits, the next is told to come back.
    assert_eq!(server.telegram(&telegram.text_update(50, 1, "a")).await, StatusCode::OK);
    assert_eq!(server.telegram(&telegram.text_update(51, 1, "b")).await, StatusCode::OK);
    until("the budget notice", || telegram.sent_to("51").len() == 1).await;
    assert!(telegram.sent_to("51")[0].contains("a lot of messages"));
    until("the answer within the budget", || telegram.sent_to("50").len() == 1).await;

    let (restricted, base) = fake_telegram().await;
    let setup =
        Setup { channels: telegram_channel("echo", "    mode: webhook\n    allow: [\"42\"]\n"), ..Setup::default() };
    let server = start("tg-allow", setup, &base, "").await;
    assert_eq!(server.telegram(&restricted.text_update(13, 1, "let me in")).await, StatusCode::OK);
    assert_eq!(server.telegram(&restricted.text_update(42, 1, "hello")).await, StatusCode::OK);
    until("the allowed sender's answer", || restricted.sent_to("42").len() == 1).await;
    settle().await;
    assert!(restricted.sent_to("13").is_empty(), "a sender outside the list hears nothing");
}

#[tokio::test(flavor = "multi_thread")]
async fn messages_that_arrive_during_a_run_wait_and_go_into_one_turn() {
    let (telegram, base) = fake_telegram().await;
    let setup = Setup { channels: telegram_channel("slow", "    mode: webhook\n"), ..Setup::default() };
    let server = start("tg-turns", setup, &base, "").await;
    for (id, text) in [(1, "one"), (2, "two"), (3, "three")] {
        assert_eq!(server.telegram(&telegram.text_update(42, id, text)).await, StatusCode::OK);
    }
    until("both turns", || telegram.sent_to("42").len() == 2).await;
    settle().await;
    assert_eq!(telegram.sent_to("42"), ["Heard: one", "Heard: two\nthree"]);
    let conversation = server.conversation("ch-telegram-42").await;
    assert_eq!(conversation["messages"].as_array().unwrap().len(), 4, "two turns, in order");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_question_to_the_customer_is_answered_by_number_or_by_a_tap() {
    let (telegram, base) = fake_telegram().await;
    let setup = Setup {
        channels: telegram_channel("refund", "    mode: webhook\n"),
        customer_answers: true,
        ..Setup::default()
    };
    let server = start("tg-ask", setup, &base, "").await;
    assert_eq!(server.telegram(&telegram.text_update(42, 1, "I want my money back")).await, StatusCode::OK);
    until("the question", || telegram.sent_to("42").len() == 1).await;
    let question = &telegram.calls_of("sendMessage")[0];
    assert_eq!(question["text"], "Refund 40 EUR to Ana?\n\nReply with a number:\n1. Approve\n2. Reject");
    assert_eq!(question["reply_markup"]["inline_keyboard"][1][0], json!({"text": "Reject", "callback_data": "2"}));
    assert_eq!(
        server.conversation("ch-telegram-42").await["channel"]["waiting_run"].as_str().map(|id| id.len() > 4),
        Some(true)
    );

    // Something that is no choice asks again, and the run still waits.
    assert_eq!(server.telegram(&telegram.text_update(42, 2, "maybe")).await, StatusCode::OK);
    until("the question again", || telegram.sent_to("42").len() == 2).await;
    assert!(telegram.sent_to("42")[1].starts_with("Please reply with one of the numbers."));

    let tap = json!({"update_id": 900, "callback_query": {"id": "cb1", "data": "2",
        "from": {"id": 42, "is_bot": false, "first_name": "Sari"},
        "message": {"message_id": 7, "chat": {"id": 42, "type": "private"}}}});
    assert_eq!(server.telegram(&tap).await, StatusCode::OK);
    until("the answer after the tap", || telegram.sent_to("42").len() == 3).await;
    assert_eq!(telegram.sent_to("42")[2], "rejected");
    assert!(!telegram.calls_of("answerCallbackQuery").is_empty());
    let conversation = server.conversation("ch-telegram-42").await;
    assert!(conversation["channel"]["waiting_run"].is_null(), "{conversation}");
    assert_eq!(conversation["messages"][1]["content"], "rejected");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_for_the_team_is_answered_by_an_approver_or_in_the_inbox() {
    let (telegram, base) = fake_telegram().await;
    let setup = Setup {
        channels: telegram_channel("refund", "    mode: webhook\n    approvers: [\"7\"]\n"),
        ..Setup::default()
    };
    let server = start("tg-approve", setup, &base, "").await;
    assert_eq!(server.telegram(&telegram.text_update(42, 1, "refund please")).await, StatusCode::OK);
    until("the notices", || telegram.sent_to("42").len() == 1 && telegram.sent_to("7").len() == 1).await;
    assert!(telegram.sent_to("42")[0].contains("Someone from our team"), "the customer is not left in silence");
    let request = &telegram.sent_to("7")[0];
    assert!(request.starts_with("<b>Approval needed</b> (refund, customer Sari)"), "{request}");
    assert!(request.ends_with("1. Approve\n2. Reject"), "{request}");
    // The customer cannot answer the team's question.
    assert_eq!(server.telegram(&telegram.text_update(42, 2, "1")).await, StatusCode::OK);
    settle().await;
    assert!(!telegram.sent_to("42").iter().any(|text| text == "refunded"));

    assert_eq!(server.telegram(&telegram.text_update(7, 1, "1")).await, StatusCode::OK);
    until("the released answer", || telegram.sent_to("42").iter().any(|text| text == "refunded")).await;
    until("the approver's receipt", || telegram.sent_to("7").len() == 2).await;
    assert_eq!(telegram.sent_to("7")[1], "Done. The run continues.");

    // The same request answered in the approvals inbox reaches the customer too.
    assert_eq!(server.telegram(&telegram.text_update(55, 1, "refund me too")).await, StatusCode::OK);
    until("the second request", || telegram.sent_to("55").len() == 1).await;
    let (_, inbox) = server.api("GET", "/v1/approvals", Json::Null).await;
    let waiting = inbox["approvals"].as_array().unwrap();
    assert_eq!(waiting.len(), 1, "{inbox}");
    assert_eq!(waiting[0]["session_id"], "ch-telegram-55");
    let run_id = waiting[0]["run_id"].as_str().unwrap();
    let answer = json!({"inputs": {"ask": {"action": "reject", "values": {}}}});
    assert_eq!(server.api("POST", &format!("/v1/approvals/{run_id}"), answer).await.0, StatusCode::ACCEPTED);
    until("the inbox answer", || telegram.sent_to("55").iter().any(|text| text == "rejected")).await;
    // The approver's copy of it is gone: their next message is an ordinary one.
    assert_eq!(server.telegram(&telegram.text_update(7, 2, "1")).await, StatusCode::OK);
    until("a new request of the approver's own", || telegram.sent_to("7").len() >= 3).await;
    assert!(!telegram.sent_to("7").iter().any(|text| text.contains("already answered")));
}

#[tokio::test(flavor = "multi_thread")]
async fn files_go_in_as_sys_files_and_out_as_documents() {
    let (telegram, base) = fake_telegram().await;
    telegram.files.lock().unwrap().insert("doc1".into(), b"%PDF-1.4 fake invoice".to_vec());
    let channels = "channels:\n  - { kind: telegram, name: inbox, workflow: inbox, mode: webhook, api_base: \"{TELEGRAM}\", webhook_secret_env: TELEGRAM_WEBHOOK_SECRET }\n  - { kind: telegram, name: note, workflow: note, mode: webhook, api_base: \"{TELEGRAM}\" }\n";
    let server =
        start("tg-files", Setup { channels: channels.into(), workspace: true, ..Setup::default() }, &base, "").await;
    let mut update = telegram.text_update(42, 1, "");
    update["message"].as_object_mut().unwrap().remove("text");
    update["message"]["caption"] = json!("my invoice");
    update["message"]["document"] =
        json!({"file_id": "doc1", "file_name": "../Invoice July.pdf", "mime_type": "application/pdf", "file_size": 21});
    let headers = [("x-telegram-bot-api-secret-token", WEBHOOK_SECRET)];
    let posted = server.post("/v1/channels/inbox/webhook", &headers, update.to_string().into_bytes()).await;
    assert_eq!(posted.0, StatusCode::OK);
    until("the answer about the file", || telegram.sent_to("42").len() == 1).await;
    assert_eq!(telegram.sent_to("42")[0], "1 file(s): Invoice_July.pdf application/pdf");
    let kept = std::fs::read_dir(server._dir.path().join("workspace/inbox/inbox")).unwrap().count();
    assert_eq!(kept, 1, "the file is in the workspace, under a name of the server's own");

    let posted = server
        .post("/v1/channels/note/webhook", &headers, telegram.text_update(42, 2, "order 1042").to_string().into_bytes())
        .await;
    assert_eq!(posted.0, StatusCode::OK);
    until("the document", || !telegram.uploads.lock().unwrap().is_empty()).await;
    let (method, body) = telegram.uploads.lock().unwrap()[0].clone();
    let body = String::from_utf8_lossy(&body).into_owned();
    assert_eq!(method, "sendDocument");
    assert!(body.contains("filename=\"summary.md\""), "{body}");
    assert!(body.contains("Summary for the customer: order 1042"), "{body}");
    assert!(body.contains("name=\"chat_id\"\r\n\r\n42"), "{body}");
    assert!(telegram.sent_to("42").contains(&"Here is your note.".to_owned()));
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_workspace_a_file_is_declined_politely() {
    let (telegram, base) = fake_telegram().await;
    telegram.files.lock().unwrap().insert("doc1".into(), b"data".to_vec());
    let setup = Setup { channels: telegram_channel("echo", "    mode: webhook\n"), ..Setup::default() };
    let server = start("tg-nofiles", setup, &base, "").await;
    let mut update = telegram.text_update(42, 1, "");
    update["message"].as_object_mut().unwrap().remove("text");
    update["message"]["document"] = json!({"file_id": "doc1", "file_name": "a.pdf"});
    assert_eq!(server.telegram(&update).await, StatusCode::OK);
    until("the notice", || telegram.sent_to("42").len() == 1).await;
    assert!(telegram.sent_to("42")[0].contains("cannot take files"));
    assert!(telegram.calls_of("getFile").is_empty(), "nothing is fetched that cannot be kept");
    // A location says nothing an agent can read.
    let mut location = telegram.text_update(42, 2, "");
    location["message"].as_object_mut().unwrap().remove("text");
    location["message"]["location"] = json!({"latitude": 1.0, "longitude": 2.0});
    assert_eq!(server.telegram(&location).await, StatusCode::OK);
    until("the second notice", || telegram.sent_to("42").len() == 2).await;
    assert!(telegram.sent_to("42")[1].contains("only read text"));
}

#[tokio::test(flavor = "multi_thread")]
async fn telegram_commands_greet_and_start_over() {
    let (telegram, base) = fake_telegram().await;
    let setup = Setup {
        channels: telegram_channel(
            "echo",
            "    mode: webhook\n    messages: { welcome: \"Halo! Ada yang bisa dibantu?\" }\n",
        ),
        ..Setup::default()
    };
    let server = start("tg-commands", setup, &base, "").await;
    assert_eq!(server.telegram(&telegram.text_update(42, 1, "/start")).await, StatusCode::OK);
    until("the welcome", || telegram.sent_to("42").len() == 1).await;
    assert_eq!(telegram.sent_to("42")[0], "Halo! Ada yang bisa dibantu?");
    assert_eq!(server.telegram(&telegram.text_update(42, 2, "hi")).await, StatusCode::OK);
    until("the answer", || telegram.sent_to("42").len() == 2).await;
    assert_eq!(server.conversation("ch-telegram-42").await["messages"].as_array().unwrap().len(), 2);
    assert_eq!(server.telegram(&telegram.text_update(42, 3, "/new")).await, StatusCode::OK);
    until("the confirmation", || telegram.sent_to("42").len() == 3).await;
    assert_eq!(server.api("GET", "/v1/conversations/ch-telegram-42", Json::Null).await.0, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------- WhatsApp

#[tokio::test(flavor = "multi_thread")]
async fn a_whatsapp_webhook_is_verified_signed_and_answered_once() {
    let (whatsapp, base) = fake_whatsapp().await;
    let server = start("wa-echo", Setup { channels: WHATSAPP_ECHO.into(), ..Setup::default() }, "", &base).await;
    let path = "/v1/channels/whatsapp/webhook";
    // Meta checks the address before it delivers anything.
    let check = |token: &str| {
        Request::get(format!("{path}?hub.mode=subscribe&hub.verify_token={token}&hub.challenge=1158201444"))
            .body(Body::empty())
            .unwrap()
    };
    let (status, challenge) = server.request(check(WA_VERIFY_TOKEN)).await;
    assert_eq!((status, challenge.as_slice()), (StatusCode::OK, b"1158201444".as_slice()));
    assert_eq!(server.request(check("guess")).await.0, StatusCode::FORBIDDEN);

    let body = wa_text("wamid.IN1", "Halo");
    assert_eq!(server.post(path, &[], body.clone()).await.0, StatusCode::UNAUTHORIZED);
    let forged = graphrt_channel::whatsapp::sign(&body, &Secret::new("another-secret"));
    assert_eq!(server.post(path, &[("x-hub-signature-256", &forged)], body.clone()).await.0, StatusCode::UNAUTHORIZED);
    // A valid signature of another body does not cover this one.
    let other = graphrt_channel::whatsapp::sign(&wa_text("wamid.IN1", "Halo!"), &Secret::new(WA_APP_SECRET));
    assert_eq!(server.post(path, &[("x-hub-signature-256", &other)], body.clone()).await.0, StatusCode::UNAUTHORIZED);
    settle().await;
    assert!(whatsapp.messages.lock().unwrap().is_empty(), "an unsigned delivery starts nothing");

    assert_eq!(server.whatsapp(body.clone()).await, StatusCode::OK);
    until("the answer", || whatsapp.texts_to("6281234567890").len() == 1).await;
    assert_eq!(
        whatsapp.texts_to("6281234567890")[0],
        "You said: Halo [whatsapp:6281234567890 ch-whatsapp-6281234567890 whatsapp]"
    );
    // The customer's message was marked as read, with the typing sign.
    let read = whatsapp.messages.lock().unwrap().iter().find(|m| m["status"] == "read").cloned().unwrap();
    assert_eq!(
        (read["message_id"].as_str(), read["typing_indicator"]["type"].as_str()),
        (Some("wamid.IN1"), Some("text"))
    );

    // Meta delivers again what it is not sure arrived, and reports deliveries.
    assert_eq!(server.whatsapp(body).await, StatusCode::OK);
    let statuses =
        json!([{"id": "wamid.OUT0", "status": "delivered", "timestamp": "1", "recipient_id": "6281234567890"}]);
    assert_eq!(server.whatsapp(wa_delivery("statuses", statuses)).await, StatusCode::OK);
    // A delivery for another number of the same app is not this channel's.
    let foreign = String::from_utf8(wa_text("wamid.IN9", "wrong number")).unwrap().replace(WA_PHONE_ID, "999");
    assert_eq!(server.whatsapp(foreign.into_bytes()).await, StatusCode::OK);
    settle().await;
    assert_eq!(whatsapp.texts_to("6281234567890").len(), 1);
    let conversation = server.conversation("ch-whatsapp-6281234567890").await;
    assert_eq!(conversation["messages"].as_array().unwrap().len(), 2);
    assert_eq!(conversation["channel"]["customer_name"], "Budi");
}

#[tokio::test(flavor = "multi_thread")]
async fn whatsapp_gets_its_own_formatting_split_answers_and_media() {
    let (whatsapp, base) = fake_whatsapp().await;
    whatsapp.media.lock().unwrap().insert("9001".into(), b"\x89PNG\r\n\x1a\nfake".to_vec());
    let channels = "channels:\n  - { kind: whatsapp, workflow: long, api_base: \"{WHATSAPP}\" }\n  - { kind: whatsapp, name: inbox, workflow: inbox, api_base: \"{WHATSAPP}\" }\n  - { kind: whatsapp, name: note, workflow: note, api_base: \"{WHATSAPP}\" }\n";
    let server =
        start("wa-long", Setup { channels: channels.into(), workspace: true, ..Setup::default() }, "", &base).await;
    let paragraph = "Kalimat yang cukup panjang untuk diulang. ".repeat(45);
    assert_eq!(server.whatsapp(wa_text("wamid.L1", &paragraph)).await, StatusCode::OK);
    until("the parts", || whatsapp.texts_to("6281234567890").len() >= 2).await;
    settle().await;
    let parts = whatsapp.texts_to("6281234567890");
    assert!(parts.iter().all(|part| part.chars().count() <= 4096));
    assert!(parts[0].starts_with("*Summary* of <your> text"), "{}", parts[0]);
    assert_eq!(parts.join(" ").matches("Kalimat yang cukup panjang").count(), 135);

    // A photo comes in through the media endpoints, with the access token.
    let image = wa_delivery(
        "messages",
        json!([{"from": "6281234567890", "id": "wamid.M1", "timestamp": "1700000000", "type": "image",
                "image": {"id": "9001", "mime_type": "image/png", "sha256": "x", "caption": "look"}}]),
    );
    let signature = graphrt_channel::whatsapp::sign(&image, &Secret::new(WA_APP_SECRET));
    let posted = server.post("/v1/channels/inbox/webhook", &[("x-hub-signature-256", &signature)], image).await;
    assert_eq!(posted.0, StatusCode::OK);
    until("the answer about the photo", || whatsapp.texts_to("6281234567890").iter().any(|t| t.contains("file(s)")))
        .await;
    assert!(whatsapp.texts_to("6281234567890").iter().any(|text| text == "1 file(s): file.png image/png"));

    // A file the run wrote goes out as a document.
    let note = wa_text("wamid.N1", "order 1042");
    let signature = graphrt_channel::whatsapp::sign(&note, &Secret::new(WA_APP_SECRET));
    let posted = server.post("/v1/channels/note/webhook", &[("x-hub-signature-256", &signature)], note).await;
    assert_eq!(posted.0, StatusCode::OK);
    until("the document", || !whatsapp.of_type("document").is_empty()).await;
    let document = &whatsapp.of_type("document")[0];
    assert_eq!(
        (document["document"]["id"].as_str(), document["document"]["filename"].as_str()),
        (Some("7001"), Some("summary.md"))
    );
    let upload = String::from_utf8_lossy(&whatsapp.uploads.lock().unwrap()[0]).into_owned();
    assert!(upload.contains("Summary for the customer: order 1042"), "{upload}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_whatsapp_question_uses_reply_buttons_and_a_late_answer_needs_a_template() {
    let (whatsapp, base) = fake_whatsapp().await;
    let channels = "channels:\n  - kind: whatsapp\n    workflow: refund\n    api_base: \"{WHATSAPP}\"\n    reengage_template: { name: follow_up, language: en_US }\n";
    let setup = Setup { channels: channels.into(), customer_answers: true, ..Setup::default() };
    let server = start("wa-ask", setup, "", &base).await;
    assert_eq!(server.whatsapp(wa_text("wamid.Q1", "refund please")).await, StatusCode::OK);
    until("the question", || !whatsapp.of_type("interactive").is_empty()).await;
    let question = &whatsapp.of_type("interactive")[0]["interactive"];
    assert_eq!(question["body"]["text"], "Refund 40 EUR to Ana?\n\nReply with a number:\n1. Approve\n2. Reject");
    assert_eq!(question["action"]["buttons"][0]["reply"], json!({"id": "1", "title": "Approve"}));

    // The customer's last message is now more than a day old: free text is no longer taken.
    let id = "ch-whatsapp-6281234567890";
    server
        .conversations
        .update(id, Box::new(|conversation| conversation.channel.as_mut().unwrap().last_inbound_ms = 1_000))
        .await
        .unwrap();
    let run_id = server.conversation(id).await["channel"]["waiting_run"].as_str().unwrap().to_owned();
    let answer = json!({"inputs": {"ask": {"action": "approve", "values": {}}}});
    assert_eq!(server.api("POST", &format!("/v1/approvals/{run_id}"), answer).await.0, StatusCode::ACCEPTED);
    until("the template", || !whatsapp.of_type("template").is_empty()).await;
    assert_eq!(whatsapp.of_type("template")[0]["template"]["name"], "follow_up");
    settle().await;
    assert!(whatsapp.texts_to("6281234567890").is_empty(), "no free text outside the window");
    // The answer is not lost: it is in the conversation.
    assert_eq!(server.conversation(id).await["messages"][1]["content"], "refunded");

    // The customer taps a button of a new question: a tap is an answer.
    assert_eq!(server.whatsapp(wa_text("wamid.Q2", "again")).await, StatusCode::OK);
    until("the second question", || whatsapp.of_type("interactive").len() == 2).await;
    let tap = wa_delivery(
        "messages",
        json!([{"from": "6281234567890", "id": "wamid.Q3", "timestamp": "1700000100", "type": "interactive",
                "interactive": {"type": "button_reply", "button_reply": {"id": "2", "title": "Reject"}}}]),
    );
    assert_eq!(server.whatsapp(tap).await, StatusCode::OK);
    until("the answer after the tap", || whatsapp.texts_to("6281234567890").contains(&"rejected".to_owned())).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_person_of_the_team_can_take_a_conversation_over() {
    let (whatsapp, base) = fake_whatsapp().await;
    let server = start("wa-human", Setup { channels: WHATSAPP_ECHO.into(), ..Setup::default() }, "", &base).await;
    let customer = "6281234567890";
    let message = json!({"to": customer, "text": "Hello from the team"});
    // A channel only answers who wrote first.
    assert_eq!(server.api("POST", "/v1/channels/whatsapp/messages", message.clone()).await.0, StatusCode::CONFLICT);
    assert_eq!(server.whatsapp(wa_text("wamid.H1", "Halo")).await, StatusCode::OK);
    until("the first answer", || whatsapp.texts_to(customer).len() == 1).await;

    let handoff = json!({"to": customer, "human": true});
    assert_eq!(server.api("POST", "/v1/channels/whatsapp/handoff", handoff).await.0, StatusCode::NO_CONTENT);
    assert_eq!(server.whatsapp(wa_text("wamid.H2", "I need a person")).await, StatusCode::OK);
    settle().await;
    assert_eq!(whatsapp.texts_to(customer).len(), 1, "the workflow stays quiet");
    assert_eq!(server.api("POST", "/v1/channels/whatsapp/messages", message).await.0, StatusCode::NO_CONTENT);
    until("the team's message", || whatsapp.texts_to(customer).len() == 2).await;
    let conversation = server.conversation("ch-whatsapp-6281234567890").await;
    let texts: Vec<&str> =
        conversation["messages"].as_array().unwrap().iter().map(|m| m["content"].as_str().unwrap()).collect();
    assert_eq!(&texts[2..], ["I need a person", "Hello from the team"]);
    assert_eq!(conversation["channel"]["human"], true);

    let back = json!({"to": customer, "human": false});
    assert_eq!(server.api("POST", "/v1/channels/whatsapp/handoff", back).await.0, StatusCode::NO_CONTENT);
    assert_eq!(server.whatsapp(wa_text("wamid.H3", "thanks")).await, StatusCode::OK);
    until("the workflow again", || whatsapp.texts_to(customer).len() == 3).await;

    // The operator routes take the API key only.
    let unauthenticated = Request::post("/v1/channels/whatsapp/handoff")
        .header("content-type", "application/json")
        .body(Body::from(json!({"to": customer, "human": true}).to_string()))
        .unwrap();
    assert_eq!(server.request(unauthenticated).await.0, StatusCode::UNAUTHORIZED);
    let (status, listed) = server.api("GET", "/v1/channels", Json::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["channels"][0]["webhook"], "/v1/channels/whatsapp/webhook");
    let listed = listed.to_string();
    assert!(!listed.contains(WA_TOKEN) && !listed.contains(WA_APP_SECRET), "{listed}");
    let _ = &server.state;
}
