//! Credential sources against a local HTTP server: per-request credentials,
//! 401 -> invalidate -> single retry, token broker issuance and expiry.

#![cfg(all(feature = "openai", feature = "anthropic"))]
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use graphrt_llm::anthropic::{AnthropicConfig, AnthropicModel};
use graphrt_llm::credentials::{
    BrokerAuth, CachedCredentials, Credential, CredentialSource, FnCredentials, StaticCredential, TokenBroker,
    TokenBrokerConfig,
};
use graphrt_llm::openai::{OpenAiConfig, OpenAiModel};
use graphrt_llm::{ChatModel, ChatRequest, LlmError, Message, collect};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A request seen by the test server.
#[derive(Clone, Debug)]
struct Seen {
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

type Handler = Arc<dyn Fn(&Seen) -> (u16, &'static str, String) + Send + Sync>;

/// Serve requests with `handler` until the test ends. Returns the base URL and the log.
async fn serve(handler: Handler) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let log2 = log.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { break };
            let handler = handler.clone();
            let log = log2.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let header_end = loop {
                    let n = socket.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
                let mut lines = head.split("\r\n");
                let request_line = lines.next().unwrap_or_default();
                let path = request_line.split(' ').nth(1).unwrap_or_default().to_owned();
                let headers: Vec<(String, String)> = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
                    .collect();
                let len: usize =
                    headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
                while buf.len() < header_end + len {
                    let n = socket.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let body = String::from_utf8_lossy(&buf[header_end..]).to_string();
                let seen = Seen { path, headers, body };
                let (status, content_type, response) = handler(&seen);
                log.lock().unwrap().push(seen);
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}",
                    response.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (format!("http://{addr}"), log)
}

fn openai_ok() -> (u16, &'static str, String) {
    let body = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    (200, "text/event-stream", body.to_owned())
}

fn anthropic_ok() -> (u16, &'static str, String) {
    let body = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
    );
    (200, "text/event-stream", body.to_owned())
}

fn unauthorized() -> (u16, &'static str, String) {
    (401, "application/json", r#"{"error":{"message":"invalid api key"}}"#.to_owned())
}

fn request() -> ChatRequest {
    ChatRequest::new("m", vec![Message::user("hi")])
}

async fn text(model: &dyn ChatModel) -> Result<String, LlmError> {
    let stream = model.chat(request()).await?;
    Ok(collect(stream, |_| {}).await?.text)
}

/// Token source handing out `token-1`, `token-2`, ... and counting invalidations.
#[derive(Debug, Default)]
struct Rotating {
    issued: AtomicUsize,
    invalidated: AtomicUsize,
}

impl CredentialSource for Rotating {
    fn credential<'a>(&'a self, _provider: &'a str) -> futures::future::BoxFuture<'a, Result<Credential, LlmError>> {
        let n = self.issued.fetch_add(1, Ordering::SeqCst) + 1;
        Box::pin(async move { Ok(Credential::new(format!("token-{n}")).header("x-session", format!("s{n}"))) })
    }

    fn invalidate(&self, _provider: &str) {
        self.invalidated.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn openai_uses_credential_and_retries_once_after_401() {
    // Only `token-2` is accepted: the first attempt fails with 401, the retry succeeds.
    let (url, log) = serve(Arc::new(|seen: &Seen| match seen.header("authorization") {
        Some("Bearer token-2") => openai_ok(),
        _ => unauthorized(),
    }))
    .await;
    let source = Arc::new(Rotating::default());
    let cfg = OpenAiConfig::new(url, Some("static-key-not-used".into())).with_credentials(source.clone());
    let model = OpenAiModel::new(cfg).unwrap();
    assert_eq!(text(&model).await.unwrap(), "hello");
    assert_eq!(source.invalidated.load(Ordering::SeqCst), 1);
    let log = log.lock().unwrap();
    assert_eq!(log.len(), 2);
    assert_eq!(log[1].header("x-session"), Some("s2"));
    assert!(log.iter().all(|s| s.header("authorization") != Some("Bearer static-key-not-used")));
}

#[tokio::test]
async fn retry_happens_only_once() {
    let (url, log) = serve(Arc::new(|_: &Seen| unauthorized())).await;
    let source = Arc::new(Rotating::default());
    let model = OpenAiModel::new(OpenAiConfig::new(url, None).with_credentials(source.clone())).unwrap();
    let err = text(&model).await.unwrap_err();
    assert!(matches!(err, LlmError::Auth(_)), "{err:?}");
    assert_eq!(log.lock().unwrap().len(), 2);
    assert_eq!(source.invalidated.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn static_key_without_credentials_does_not_retry() {
    let (url, log) = serve(Arc::new(|_: &Seen| unauthorized())).await;
    let model = OpenAiModel::new(OpenAiConfig::new(url, Some("k".into()))).unwrap();
    assert!(matches!(text(&model).await, Err(LlmError::Auth(_))));
    assert_eq!(log.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn anthropic_sends_credential_as_x_api_key_and_honours_base_url_override() {
    let (url, log) = serve(Arc::new(|seen: &Seen| match seen.header("x-api-key") {
        Some("user-key") => anthropic_ok(),
        _ => unauthorized(),
    }))
    .await;
    // The configured base URL is unreachable; the credential points at the test server.
    let issued = url.clone();
    let source = FnCredentials::new(move |_p: String| {
        let url = issued.clone();
        async move { Ok(Credential::new("user-key").base_url(url)) }
    });
    let cfg = AnthropicConfig { base_url: "http://127.0.0.1:9".into(), ..AnthropicConfig::default() }
        .with_credentials(Arc::new(source));
    let model = AnthropicModel::new(cfg).unwrap();
    assert_eq!(text(&model).await.unwrap(), "hello");
    let log = log.lock().unwrap();
    assert_eq!(log[0].path, "/v1/messages");
}

#[tokio::test]
async fn token_broker_issues_cached_tokens_and_refreshes_on_401() {
    let counter = Arc::new(AtomicUsize::new(0));
    let accepted = Arc::new(Mutex::new(String::new()));
    let (counter2, accepted2) = (counter.clone(), accepted.clone());
    let (url, log) = serve(Arc::new(move |seen: &Seen| {
        if seen.path == "/token" {
            if seen.header("authorization") != Some("Bearer user-session") {
                return (401, "application/json", "{}".to_owned());
            }
            let n = counter2.fetch_add(1, Ordering::SeqCst) + 1;
            let token = format!("short-{n}");
            // From now on only the newest token is valid (rotation).
            *accepted2.lock().unwrap() = token.clone();
            let body = serde_json::json!({"token": token, "expires_in": 900, "headers": {"x-tenant": "acme"}});
            return (200, "application/json", body.to_string());
        }
        let ok = seen.header("authorization").map(|v| v == format!("Bearer {}", accepted2.lock().unwrap()));
        if ok == Some(true) { openai_ok() } else { unauthorized() }
    }))
    .await;

    let session = Arc::new(StaticCredential::new("user-session"));
    let broker = TokenBroker::cached(
        TokenBrokerConfig::new(format!("{url}/token"))
            .claim("app", serde_json::json!("demo"))
            .auth(BrokerAuth::Bearer(session)),
    )
    .unwrap();
    let broker: Arc<dyn CredentialSource> = Arc::new(broker);
    let model =
        OpenAiModel::new(OpenAiConfig::new(format!("{url}/v1"), None).with_credentials(broker.clone())).unwrap();

    assert_eq!(text(&model).await.unwrap(), "hello");
    assert_eq!(text(&model).await.unwrap(), "hello");
    assert_eq!(counter.load(Ordering::SeqCst), 1, "token is cached between requests");

    // Server-side rotation: the cached token is now rejected -> invalidate -> new token.
    *accepted.lock().unwrap() = "rotated-elsewhere".into();
    counter.store(1, Ordering::SeqCst);
    assert_eq!(text(&model).await.unwrap(), "hello");
    assert_eq!(counter.load(Ordering::SeqCst), 2);

    let log = log.lock().unwrap();
    let token_requests: Vec<&Seen> = log.iter().filter(|s| s.path == "/token").collect();
    assert_eq!(token_requests.len(), 2);
    let body: serde_json::Value = serde_json::from_str(&token_requests[0].body).unwrap();
    assert_eq!(body["provider"], "openai");
    assert_eq!(body["app"], "demo");
    let chat = log.iter().find(|s| s.path == "/v1/chat/completions").unwrap();
    assert_eq!(chat.header("x-tenant"), Some("acme"));
}

#[tokio::test]
async fn expired_tokens_are_refetched() {
    let counter = Arc::new(AtomicUsize::new(0));
    let c2 = counter.clone();
    let (url, _) = serve(Arc::new(move |seen: &Seen| {
        if seen.path == "/token" {
            let n = c2.fetch_add(1, Ordering::SeqCst) + 1;
            // Already inside the refresh window.
            let body = serde_json::json!({"token": format!("t{n}"), "expires_in": 1});
            return (200, "application/json", body.to_string());
        }
        openai_ok()
    }))
    .await;
    let broker = TokenBroker::cached(TokenBrokerConfig::new(format!("{url}/token"))).unwrap();
    let model =
        OpenAiModel::new(OpenAiConfig::new(format!("{url}/v1"), None).with_credentials(Arc::new(broker))).unwrap();
    text(&model).await.unwrap();
    text(&model).await.unwrap();
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn broker_rejecting_the_app_is_an_auth_error() {
    let (url, _) = serve(Arc::new(|_: &Seen| (403, "application/json", "{}".to_owned()))).await;
    let broker = TokenBroker::new(TokenBrokerConfig::new(format!("{url}/token"))).unwrap();
    let err = broker.credential("openai").await.unwrap_err();
    assert!(matches!(err, LlmError::Auth(_)), "{err:?}");
}

#[tokio::test]
async fn cached_wrapper_shares_tokens_across_providers_independently() {
    let cache = CachedCredentials::new(Rotating::default());
    let a = cache.credential("openai").await.unwrap();
    let b = cache.credential("anthropic").await.unwrap();
    let a2 = cache.credential("openai").await.unwrap();
    assert_eq!(a.secret, a2.secret);
    assert_ne!(a.secret, b.secret);
}
