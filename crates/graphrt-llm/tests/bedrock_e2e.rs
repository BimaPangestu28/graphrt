//! Bedrock provider through the real engine, against an in-test HTTP server
//! returning `application/vnd.amazon.eventstream` bodies.

#![cfg(feature = "bedrock")]
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
use graphrt_llm::bedrock::{AwsCredentials, BedrockAuth, BedrockConfig, BedrockModel};
use graphrt_llm::{ChatModel, ChatRequest, LlmError, Message, ModelRegistry, collect};
use serde_json::{Value as Json, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

const EVENTSTREAM: &str = "application/vnd.amazon.eventstream";

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

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { 0xEDB8_8320 ^ (crc >> 1) } else { crc >> 1 };
        }
    }
    !crc
}

/// Encode one event-stream message with string headers.
fn frame(event_type: &str, payload: &Json) -> Vec<u8> {
    let payload = payload.to_string().into_bytes();
    let mut headers = Vec::new();
    for (name, value) in
        [(":event-type", event_type), (":content-type", "application/json"), (":message-type", "event")]
    {
        headers.push(name.len() as u8);
        headers.extend_from_slice(name.as_bytes());
        headers.push(7);
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    }
    let total = (12 + headers.len() + payload.len() + 4) as u32;
    let mut out = Vec::new();
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    let prelude_crc = crc32(&out);
    out.extend_from_slice(&prelude_crc.to_be_bytes());
    out.extend_from_slice(&headers);
    out.extend_from_slice(&payload);
    let crc = crc32(&out);
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

fn stream(events: &[(&str, Json)]) -> Vec<u8> {
    events.iter().flat_map(|(kind, payload)| frame(kind, payload)).collect()
}

fn text_reply(text: &str) -> Vec<u8> {
    stream(&[
        ("messageStart", json!({"role": "assistant"})),
        ("contentBlockDelta", json!({"contentBlockIndex": 0, "delta": {"text": text}})),
        ("contentBlockStop", json!({"contentBlockIndex": 0})),
        ("messageStop", json!({"stopReason": "end_turn"})),
        ("metadata", json!({"usage": {"inputTokens": 3, "outputTokens": 2, "totalTokens": 5}})),
    ])
}

fn graph(doc: Json) -> Arc<Graph> {
    let mut registry = Registry::with_builtins();
    graphrt_llm::register(&mut registry);
    Arc::new(Graph::build(Workflow::from_value(doc).unwrap(), &registry).unwrap())
}

fn sigv4_model(endpoint: &str) -> Arc<BedrockModel> {
    let creds = AwsCredentials {
        access_key_id: "AKIDEXAMPLE".into(),
        secret_access_key: "aws-secret".into(),
        session_token: None,
    };
    let mut cfg = BedrockConfig::new("us-east-1", BedrockAuth::SigV4(creds));
    cfg.endpoint = Some(endpoint.to_owned());
    Arc::new(BedrockModel::new(cfg).unwrap())
}

fn body_of(raw: &str) -> Json {
    serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap()).unwrap()
}

fn outputs(events: &[Event]) -> indexmap::IndexMap<String, Value> {
    match events.last() {
        Some(Event::RunSucceeded { outputs, .. }) => outputs.clone(),
        other => panic!("expected RunSucceeded, got {other:?}"),
    }
}

#[tokio::test]
async fn llm_node_streams_bedrock_tokens_with_sigv4() {
    let body = stream(&[
        ("messageStart", json!({"role": "assistant"})),
        ("contentBlockDelta", json!({"contentBlockIndex": 0, "delta": {"text": "Hel"}})),
        ("contentBlockDelta", json!({"contentBlockIndex": 0, "delta": {"text": "lo"}})),
        ("contentBlockStop", json!({"contentBlockIndex": 0})),
        ("messageStop", json!({"stopReason": "end_turn"})),
        ("metadata", json!({"usage": {"inputTokens": 3, "outputTokens": 2, "totalTokens": 5}})),
    ]);
    let (url, mut requests) = serve(vec![(200, EVENTSTREAM, body)]).await;
    let g = graph(json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start", "variables": []},
            {"id": "llm", "type": "llm", "model": {"provider": "bedrock", "name": "anthropic.claude-3-haiku-20240307-v1:0"},
             "prompt_template": [{"role": "user", "text": "{{#sys.query#}}"}]},
            {"id": "out", "type": "answer", "answer": "{{#llm.text#}}"}
        ],
        "edges": [{"source": "start", "target": "llm"}, {"source": "llm", "target": "out"}]
    }}));
    let services = Services::new().with(ModelRegistry::new().with("bedrock", sigv4_model(&url)));
    let events: Vec<Event> =
        Engine::new(g).with_services(services).run(RunInput::new().sys("query", "hi")).collect().await;
    assert_eq!(outputs(&events).get("answer").map(Value::to_text).as_deref(), Some("Hello"));
    let chunks =
        events.iter().filter(|e| matches!(e, Event::NodeChunk { node_id, .. } if node_id.as_ref() == "llm")).count();
    assert_eq!(chunks, 2);

    let raw = requests.recv().await.unwrap();
    assert!(raw.starts_with("POST /model/anthropic.claude-3-haiku-20240307-v1%3A0/converse-stream "), "{raw}");
    let lower = raw.to_ascii_lowercase();
    assert!(lower.contains("authorization: aws4-hmac-sha256 credential=akidexample/"), "{raw}");
    assert!(lower.contains("x-amz-content-sha256: "));
    assert!(!raw.contains("aws-secret"));
    assert_eq!(body_of(&raw)["messages"][0]["content"][0]["text"], "hi");
}

#[tokio::test]
async fn agent_tool_loop_over_bedrock() {
    let first = stream(&[
        ("messageStart", json!({"role": "assistant"})),
        ("contentBlockDelta", json!({"contentBlockIndex": 0, "delta": {"text": "Adding"}})),
        ("contentBlockDelta", json!({"contentBlockIndex": 0, "delta": {"text": "."}})),
        ("contentBlockStop", json!({"contentBlockIndex": 0})),
        (
            "contentBlockStart",
            json!({"contentBlockIndex": 1, "start": {"toolUse": {"toolUseId": "tu_1", "name": "add"}}}),
        ),
        ("contentBlockDelta", json!({"contentBlockIndex": 1, "delta": {"toolUse": {"input": "{\"a\": 4,"}}})),
        ("contentBlockDelta", json!({"contentBlockIndex": 1, "delta": {"toolUse": {"input": " \"b\": 5}"}}})),
        ("contentBlockStop", json!({"contentBlockIndex": 1})),
        ("messageStop", json!({"stopReason": "tool_use"})),
        ("metadata", json!({"usage": {"inputTokens": 10, "outputTokens": 5, "totalTokens": 15}})),
    ]);
    let second = stream(&[
        ("messageStart", json!({"role": "assistant"})),
        ("contentBlockDelta", json!({"contentBlockIndex": 0, "delta": {"text": "4 + 5"}})),
        ("contentBlockDelta", json!({"contentBlockIndex": 0, "delta": {"text": " = 9"}})),
        ("contentBlockStop", json!({"contentBlockIndex": 0})),
        ("messageStop", json!({"stopReason": "end_turn"})),
    ]);
    let (url, mut requests) = serve(vec![(200, EVENTSTREAM, first), (200, EVENTSTREAM, second)]).await;

    let calls = Arc::new(AtomicU32::new(0));
    let counter = calls.clone();
    let spec = ToolSpec {
        name: "add".into(),
        description: "add".into(),
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
            {"id": "agent", "type": "agent", "model": {"provider": "bedrock", "name": "amazon.nova-pro-v1:0"},
             "instruction": "Add numbers.", "query": "{{#sys.query#}}", "tools": ["calc/*"]},
            {"id": "out", "type": "answer", "answer": "{{#agent.text#}}"}
        ],
        "edges": [{"source": "start", "target": "agent"}, {"source": "agent", "target": "out"}]
    }}));
    let services = Services::new().with(ModelRegistry::new().with("bedrock", sigv4_model(&url))).with(registry);
    let events: Vec<Event> =
        Engine::new(g).with_services(services).run(RunInput::new().sys("query", "4+5")).collect().await;
    // Every turn streams per token; the preamble of the tool turn stays in the answer.
    let chunks: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            Event::NodeChunk { node_id, chunk, .. } if node_id.as_ref() == "agent" => Some(chunk.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(chunks, ["Adding", ".", "\n\n", "4 + 5", " = 9"]);
    assert_eq!(outputs(&events).get("answer").map(Value::to_text).as_deref(), Some("Adding.\n\n4 + 5 = 9"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let first_body = body_of(&requests.recv().await.unwrap());
    assert_eq!(first_body["toolConfig"]["tools"][0]["toolSpec"]["name"], "add");
    let second_body = body_of(&requests.recv().await.unwrap());
    let messages = second_body["messages"].as_array().unwrap();
    let assistant = messages.iter().find(|m| m["role"] == "assistant").unwrap();
    assert_eq!(assistant["content"][0]["text"], "Adding.");
    assert_eq!(assistant["content"][1]["toolUse"]["toolUseId"], "tu_1");
    assert_eq!(assistant["content"][1]["toolUse"]["input"]["b"], 5);
    let result = &messages.last().unwrap()["content"][0]["toolResult"];
    assert_eq!(result["toolUseId"], "tu_1");
    assert_eq!(result["content"][0]["text"], "9");
}

#[tokio::test]
async fn bearer_api_key_and_error_mapping() {
    let (url, mut requests) = serve(vec![
        (200, EVENTSTREAM, text_reply("ok")),
        (429, "application/json", br#"{"message":"Too many requests, please wait before trying again."}"#.to_vec()),
        (403, "application/json", br#"{"message":"The security token included in the request is invalid."}"#.to_vec()),
    ])
    .await;
    let mut cfg = BedrockConfig::new("us-east-1", BedrockAuth::ApiKey("bedrock-api-key".into()));
    cfg.endpoint = Some(url);
    let model = BedrockModel::new(cfg).unwrap();
    let request = || ChatRequest::new("m", vec![Message::user("x")]);

    let response = collect(model.chat(request()).await.unwrap(), |_| {}).await.unwrap();
    assert_eq!(response.text, "ok");
    assert_eq!(response.usage.total_tokens, 5);
    let raw = requests.recv().await.unwrap();
    assert!(raw.to_ascii_lowercase().contains("authorization: bearer bedrock-api-key"), "{raw}");

    let Err(err) = model.chat(request()).await else { panic!("expected rate limit") };
    assert!(matches!(err, LlmError::RateLimit { .. }), "{err:?}");
    let Err(err) = model.chat(request()).await else { panic!("expected auth error") };
    assert!(matches!(err, LlmError::Auth(_)), "{err:?}");
}

#[tokio::test]
async fn truncated_stream_is_a_retryable_error() {
    let mut cut = text_reply("partial");
    cut.truncate(cut.len() - 40);
    let (url, _requests) = serve(vec![(200, EVENTSTREAM, cut)]).await;
    let stream = sigv4_model(&url).chat(ChatRequest::new("m", vec![Message::user("x")])).await.unwrap();
    let err = collect(stream, |_| {}).await.unwrap_err();
    assert!(err.is_retryable(), "{err:?}");
}
