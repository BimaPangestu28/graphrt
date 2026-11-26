//! Dynamic tokens over Streamable HTTP against a minimal mock MCP server that
//! only accepts `Authorization: Bearer fresh`.

#![cfg(feature = "http")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use graphrt_core::tool::ToolRuntime;
use graphrt_mcp::{
    HttpConfig, McpError, McpServerConfig, McpToolRuntime, McpTransport, TokenError, TokenProvider, TokenSource,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

const GOOD_TOKEN: &str = "fresh";

/// Every `Authorization` header the server received.
type Seen = Arc<Mutex<Vec<String>>>;

/// Serves MCP over plain HTTP/1.1 (one request per connection) on 127.0.0.1.
async fn mock_server() -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let seen: Seen = Arc::default();
    let log = seen.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let log = log.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stream);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).await.is_err() {
                    return;
                }
                let mut auth = String::new();
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(value) = lower.strip_prefix("authorization:") {
                        auth = line[line.len() - value.len()..].trim().to_owned();
                    }
                    if let Some(value) = lower.strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0; length];
                let _ = reader.read_exact(&mut body).await;
                log.lock().unwrap().push(auth.clone());
                let response = respond(&request_line, &auth, &body);
                let _ = reader.get_mut().write_all(response.as_bytes()).await;
                let _ = reader.get_mut().shutdown().await;
            });
        }
    });
    (url, seen)
}

fn http(status: &str, extra_headers: &str, body: &str) -> String {
    format!("HTTP/1.1 {status}\r\n{extra_headers}content-length: {}\r\nconnection: close\r\n\r\n{body}", body.len())
}

fn respond(request_line: &str, auth: &str, body: &[u8]) -> String {
    if auth != format!("Bearer {GOOD_TOKEN}") {
        return http(
            "401 Unauthorized",
            "www-authenticate: Bearer resource_metadata=\"http://127.0.0.1/.well-known/oauth-protected-resource\"\r\n",
            "",
        );
    }
    if !request_line.starts_with("POST") {
        return http("405 Method Not Allowed", "", "");
    }
    let message: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
    let Some(id) = message.get("id").cloned() else {
        return http("202 Accepted", "", "");
    };
    let result = match message.get("method").and_then(|m| m.as_str()) {
        Some("initialize") => serde_json::json!({
            "protocolVersion": message["params"]["protocolVersion"],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "mock", "version": "1"}
        }),
        Some("tools/list") => serde_json::json!({
            "tools": [{"name": "whoami", "description": "Who is signed in", "inputSchema": {"type": "object"}}]
        }),
        _ => serde_json::json!({}),
    };
    let reply = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
    http("200 OK", "content-type: application/json\r\n", &reply)
}

/// Hands out `current`; `refresh` swaps in `next` (or fails when it is `None`).
struct Rotating {
    current: Mutex<String>,
    next: Option<String>,
    refreshed: Mutex<Vec<String>>,
}

impl Rotating {
    fn new(current: &str, next: Option<&str>) -> Arc<Self> {
        Arc::new(Self { current: Mutex::new(current.into()), next: next.map(Into::into), refreshed: Mutex::default() })
    }
}

impl TokenProvider for Rotating {
    fn access_token(&self) -> BoxFuture<'_, Result<String, TokenError>> {
        Box::pin(async move { Ok(self.current.lock().unwrap().clone()) })
    }

    fn refresh(&self, rejected: String) -> BoxFuture<'_, Result<String, TokenError>> {
        Box::pin(async move {
            self.refreshed.lock().unwrap().push(rejected);
            let next = self.next.clone().ok_or_else(|| TokenError::SignInRequired("refresh token revoked".into()))?;
            *self.current.lock().unwrap() = next.clone();
            Ok(next)
        })
    }
}

fn config(url: &str, source: Option<TokenSource>, bearer: Option<&str>) -> McpServerConfig {
    McpServerConfig {
        name: "remote".into(),
        transport: McpTransport::Http(HttpConfig {
            url: url.into(),
            headers: Default::default(),
            bearer_token: bearer.map(Into::into),
            token_source: source,
        }),
        timeout_ms: 5_000,
        allowed_tools: None,
    }
}

#[tokio::test]
async fn a_rejected_token_is_refreshed_and_the_request_retried_once() {
    let (url, seen) = mock_server().await;
    let provider = Rotating::new("stale", Some(GOOD_TOKEN));
    let rt = McpToolRuntime::connect(&config(&url, Some(TokenSource::new(provider.clone())), None))
        .await
        .expect("connects after one refresh");
    let tools = rt.list().await.unwrap();
    assert_eq!(tools[0].name, "whoami");
    assert_eq!(*provider.refreshed.lock().unwrap(), ["stale"], "refreshed exactly once");
    let headers = seen.lock().unwrap().clone();
    assert_eq!(headers[0], "Bearer stale");
    assert!(headers[1..].iter().all(|h| h == "Bearer fresh"), "{headers:?}");
    rt.close().await;
}

#[tokio::test]
async fn a_failed_refresh_surfaces_the_sign_in_error() {
    let (url, _seen) = mock_server().await;
    let provider = Rotating::new("stale", None);
    let err = McpToolRuntime::connect(&config(&url, Some(TokenSource::new(provider.clone())), None))
        .await
        .expect_err("cannot connect without a valid token");
    assert!(err.to_string().contains("sign-in required: refresh token revoked"), "{err}");
    assert_eq!(provider.refreshed.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn the_same_token_after_refresh_is_not_retried() {
    let (url, seen) = mock_server().await;
    let provider = Rotating::new("stale", Some("stale"));
    let err = McpToolRuntime::connect(&config(&url, Some(TokenSource::new(provider)), None)).await.expect_err("401");
    assert!(matches!(err, McpError::Initialize { .. }), "{err}");
    assert_eq!(seen.lock().unwrap().len(), 1, "no second request with the same token");
}

#[tokio::test]
async fn the_provider_is_asked_before_every_request() {
    let (url, seen) = mock_server().await;
    let provider = Rotating::new(GOOD_TOKEN, None);
    let rt = McpToolRuntime::connect(&config(&url, Some(TokenSource::new(provider.clone())), None)).await.unwrap();
    rt.refresh().await.unwrap();
    rt.refresh().await.unwrap();
    assert!(seen.lock().unwrap().len() >= 4, "initialize, initialized and two lists");
    assert!(provider.refreshed.lock().unwrap().is_empty());
    rt.close().await;
}

#[tokio::test]
async fn a_static_token_and_a_token_source_are_exclusive() {
    let provider = Rotating::new(GOOD_TOKEN, None);
    let err = McpToolRuntime::connect(&config("http://127.0.0.1:9/mcp", Some(TokenSource::new(provider)), Some("t")))
        .await
        .expect_err("ambiguous auth");
    assert!(matches!(err, McpError::Config(ref m) if m.contains("not both")), "{err}");
}

#[test]
fn debug_never_shows_the_provider() {
    let provider = Rotating::new("secret-token", None);
    let shown = format!("{:?}", config("https://x/mcp", Some(TokenSource::new(provider)), None));
    assert!(!shown.contains("secret-token") && shown.contains("TokenSource(..)"), "{shown}");
}
