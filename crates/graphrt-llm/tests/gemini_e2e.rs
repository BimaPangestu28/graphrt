//! Gemini provider through the real engine, against an in-test HTTP server.

#![cfg(feature = "gemini")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use futures::StreamExt;
use graphrt_core::Workflow;
use graphrt_core::engine::{Engine, RunInput};
use graphrt_core::event::Event;
use graphrt_core::graph::Graph;
use graphrt_core::node::{Registry, Services};
use graphrt_core::tool::{ToolOutput, ToolRegistry, ToolSpec};
use graphrt_core::tool_node::NativeTools;
use graphrt_core::value::Value;
use graphrt_llm::credentials::{Credential, CredentialSource};
use graphrt_llm::gemini::{GeminiConfig, GeminiModel};
use graphrt_llm::{ChatModel, ChatRequest, LlmError, Message, ModelRegistry, collect};
use serde_json::{Value as Json, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// Serve canned `(status, content-type, body)` replies, one per connection; forward raw requests.
async fn serve(replies: Vec<(u16, &'static str, Vec<u8>)>) -> (String, mpsc::UnboundedReceiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        for (status, content_type, body) in replies {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let raw = read_request(&mut socket).await;
            let _ = tx.send(raw);
            let head = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(&body).await;
            let _ = socket.shutdown().await;
        }
    });
    (format!("http://{addr}"), rx)
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = socket.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf);
        if let Some(idx) = text.find("\r\n\r\n") {
            let len = text
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            if buf.len() >= idx + 4 + len {
                break;
            }
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn sse(chunks: &[Json]) -> Vec<u8> {
    chunks.iter().map(|c| format!("data: {c}\r\n\r\n")).collect::<String>().into_bytes()
}

fn body_of(raw: &str) -> Json {
    serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap()).unwrap()
}

fn graph(doc: Json) -> Arc<Graph> {
    let mut registry = Registry::with_builtins();
    graphrt_llm::register(&mut registry);
    Arc::new(Graph::build(Workflow::from_value(doc).unwrap(), &registry).unwrap())
}

fn model(base_url: &str) -> Arc<GeminiModel> {
    let mut cfg = GeminiConfig::new("AIza-test-key");
    cfg.base_url = base_url.to_owned();
    Arc::new(GeminiModel::new(cfg).unwrap())
}

fn outputs(events: &[Event]) -> indexmap::IndexMap<String, Value> {
    match events.last() {
        Some(Event::RunSucceeded { outputs, .. }) => outputs.clone(),
        other => panic!("expected RunSucceeded, got {other:?}"),
    }
}

#[tokio::test]
async fn llm_node_streams_gemini_tokens() {
    let stream = sse(&[
        json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "Hel"}]}}]}),
        json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "lo"}]}, "finishReason": "STOP"}],
               "usageMetadata": {"promptTokenCount": 4, "candidatesTokenCount": 2, "totalTokenCount": 6}}),
    ]);
    let (url, mut requests) = serve(vec![(200, "text/event-stream", stream)]).await;
    let g = graph(json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start", "variables": []},
            {"id": "llm", "type": "llm", "model": {"provider": "gemini", "name": "gemini-2.5-flash"},
             "prompt_template": [{"role": "system", "text": "be brief"}, {"role": "user", "text": "{{#sys.query#}}"}]},
            {"id": "out", "type": "answer", "answer": "{{#llm.text#}}"}
        ],
        "edges": [{"source": "start", "target": "llm"}, {"source": "llm", "target": "out"}]
    }}));
    let services = Services::new().with(ModelRegistry::new().with("gemini", model(&url)));
    let events: Vec<Event> =
        Engine::new(g).with_services(services).run(RunInput::new().sys("query", "hi")).collect().await;
    assert_eq!(outputs(&events).get("answer").map(Value::to_text).as_deref(), Some("Hello"));
    let chunks =
        events.iter().filter(|e| matches!(e, Event::NodeChunk { node_id, .. } if node_id.as_ref() == "llm")).count();
    assert_eq!(chunks, 2);

    let raw = requests.recv().await.unwrap();
    assert!(raw.starts_with("POST /models/gemini-2.5-flash:streamGenerateContent?alt=sse "), "{raw}");
    assert!(raw.to_ascii_lowercase().contains("x-goog-api-key: aiza-test-key"));
    assert!(!raw.contains("key=AIza"), "the key must never be in the URL");
    let body = body_of(&raw);
    assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be brief");
    assert_eq!(body["contents"][0]["parts"][0]["text"], "hi");
}

#[tokio::test]
async fn agent_tool_loop_over_gemini() {
    let first = sse(&[json!({"candidates": [{"content": {"role": "model", "parts": [
        {"functionCall": {"name": "add", "args": {"a": 2, "b": 3}}}
    ]}, "finishReason": "STOP"}]})]);
    let second = sse(&[json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "The sum is 5."}]},
        "finishReason": "STOP"}]})]);
    let (url, mut requests) = serve(vec![(200, "text/event-stream", first), (200, "text/event-stream", second)]).await;

    let calls = Arc::new(AtomicU32::new(0));
    let counter = calls.clone();
    let spec = ToolSpec {
        name: "add".into(),
        description: "add two numbers".into(),
        parameters: json!({"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}}),
    };
    let tools = NativeTools::new().with(spec, move |args| {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            let n = |k: &str| args.get(k).and_then(Json::as_i64).unwrap_or(0);
            Ok(ToolOutput::text((n("a") + n("b")).to_string()))
        }
    });
    let mut registry = ToolRegistry::new();
    registry.add("calc", Arc::new(tools));

    let g = graph(json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start", "variables": []},
            {"id": "agent", "type": "agent", "model": {"provider": "gemini", "name": "gemini-2.5-pro"},
             "instruction": "You add numbers.", "query": "{{#sys.query#}}", "tools": ["calc/*"]},
            {"id": "out", "type": "answer", "answer": "{{#agent.text#}}"}
        ],
        "edges": [{"source": "start", "target": "agent"}, {"source": "agent", "target": "out"}]
    }}));
    let services = Services::new().with(ModelRegistry::new().with("gemini", model(&url))).with(registry);
    let events: Vec<Event> =
        Engine::new(g).with_services(services).run(RunInput::new().sys("query", "2+3?")).collect().await;
    assert_eq!(outputs(&events).get("answer").map(Value::to_text).as_deref(), Some("The sum is 5."));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let first_body = body_of(&requests.recv().await.unwrap());
    assert_eq!(first_body["tools"][0]["functionDeclarations"][0]["name"], "add");
    assert_eq!(first_body["tools"][0]["functionDeclarations"][0]["parameters"]["type"], "OBJECT");
    let second_body = body_of(&requests.recv().await.unwrap());
    let contents = second_body["contents"].as_array().unwrap();
    let model_turn = contents.iter().find(|c| c["role"] == "model").unwrap();
    assert_eq!(model_turn["parts"][0]["functionCall"]["name"], "add");
    let response = contents.last().unwrap();
    assert_eq!(response["parts"][0]["functionResponse"]["name"], "add");
    assert_eq!(response["parts"][0]["functionResponse"]["response"]["result"], 5);
}

/// A credential source that hands out a stale token first.
#[derive(Debug, Default)]
struct Rotating {
    invalidated: AtomicU32,
}

impl CredentialSource for Rotating {
    fn credential<'a>(&'a self, provider: &'a str) -> futures::future::BoxFuture<'a, Result<Credential, LlmError>> {
        assert_eq!(provider, "gemini");
        let token = if self.invalidated.load(Ordering::SeqCst) == 0 { "stale" } else { "fresh" };
        Box::pin(futures::future::ready(Ok(Credential::new(token))))
    }

    fn invalidate(&self, _provider: &str) {
        self.invalidated.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn credential_source_is_refreshed_once_on_401() {
    let ok = sse(&[json!({"candidates": [{"content": {"parts": [{"text": "ok"}]}, "finishReason": "STOP"}]})]);
    let (url, mut requests) = serve(vec![
        (401, "application/json", br#"{"error":{"code":401,"message":"expired","status":"UNAUTHENTICATED"}}"#.to_vec()),
        (200, "text/event-stream", ok),
    ])
    .await;
    let source = Arc::new(Rotating::default());
    let mut cfg = GeminiConfig::default().with_credentials(source.clone());
    cfg.base_url = url;
    let model = GeminiModel::new(cfg).unwrap();
    let stream = model.chat(ChatRequest::new("m", vec![Message::user("x")])).await.unwrap();
    let response = collect(stream, |_| {}).await.unwrap();
    assert_eq!(response.text, "ok");
    assert_eq!(source.invalidated.load(Ordering::SeqCst), 1);
    assert!(requests.recv().await.unwrap().contains("stale"));
    assert!(requests.recv().await.unwrap().contains("fresh"));
}

#[tokio::test]
async fn credential_base_url_sends_the_proxy_token_to_the_proxy() {
    let ok = sse(&[json!({"candidates": [{"content": {"parts": [{"text": "via proxy"}]}, "finishReason": "STOP"}]})]);
    let (url, mut requests) = serve(vec![(200, "text/event-stream", ok)]).await;
    // Configured for Google; the credential moves the request to the proxy.
    let source = Arc::new(graphrt_llm::credentials::StaticCredential::from_credential(
        Credential::new("proxy-token").base_url(&url),
    ));
    let mut cfg = GeminiConfig::default().with_credentials(source);
    cfg.headers = vec![("x-operator-secret".into(), "do-not-leak".into())];
    let model = GeminiModel::new(cfg).unwrap();
    let stream = model.chat(ChatRequest::new("m", vec![Message::user("x")])).await.unwrap();
    assert_eq!(collect(stream, |_| {}).await.unwrap().text, "via proxy");
    let raw = requests.recv().await.unwrap();
    assert!(raw.contains("proxy-token"), "{raw}");
    assert!(!raw.contains("do-not-leak"), "{raw}");
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let (url, _requests) = serve(vec![(307, "application/json", b"{}".to_vec())]).await;
    let Err(err) = model(&url).chat(ChatRequest::new("m", vec![Message::user("x")])).await else {
        panic!("expected an error");
    };
    assert!(matches!(err, LlmError::BadRequest(ref m) if m.contains("redirect")), "{err:?}");
}

#[tokio::test]
async fn rate_limit_reports_retry_delay() {
    let body = br#"{"error":{"code":429,"message":"Quota exceeded","status":"RESOURCE_EXHAUSTED",
        "details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"2s"}]}}"#
        .to_vec();
    let (url, _requests) = serve(vec![(429, "application/json", body)]).await;
    let Err(err) = model(&url).chat(ChatRequest::new("m", vec![Message::user("x")])).await else {
        panic!("expected an error");
    };
    assert_eq!(err, LlmError::RateLimit { message: "Quota exceeded".into(), retry_after_ms: Some(2000) });
}
