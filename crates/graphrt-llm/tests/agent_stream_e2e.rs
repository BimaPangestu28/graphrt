//! Agent streaming over the OpenAI-compatible and Anthropic providers, against
//! an in-test HTTP server: every turn streams its text per provider token,
//! tool-call JSON never reaches the text stream, and the ordered `answer`
//! response sees the same tokens. (Gemini and Bedrock are covered in their own
//! e2e files.)

#![cfg(any(feature = "openai", feature = "anthropic"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;

use futures::StreamExt;
use graphrt_core::Workflow;
use graphrt_core::engine::{Engine, RunInput};
use graphrt_core::event::Event;
use graphrt_core::graph::Graph;
use graphrt_core::node::{Registry, Services};
use graphrt_core::response::ResponseAssembler;
use graphrt_core::tool::{ToolOutput, ToolRegistry, ToolSpec};
use graphrt_core::tool_node::NativeTools;
use graphrt_llm::{ChatModel, ModelRegistry};
use serde_json::{Value as Json, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serve one canned SSE body per connection.
async fn serve(bodies: Vec<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        for body in bodies {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            drain_request(&mut socket).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(body.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    format!("http://{addr}")
}

async fn drain_request(socket: &mut tokio::net::TcpStream) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = socket.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf);
        let Some(idx) = text.find("\r\n\r\n") else { continue };
        let len = text
            .lines()
            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
            .unwrap_or(0usize);
        if buf.len() >= idx + 4 + len {
            return;
        }
    }
}

fn add_tools() -> ToolRegistry {
    let spec = ToolSpec {
        name: "add".into(),
        description: "add two numbers".into(),
        parameters: json!({"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}}),
    };
    let tools = NativeTools::new().with(spec, |args| async move {
        let n = |k: &str| args.get(k).and_then(Json::as_i64).unwrap_or(0);
        Ok(ToolOutput::text((n("a") + n("b")).to_string()))
    });
    let mut registry = ToolRegistry::new();
    registry.add("calc", Arc::new(tools));
    registry
}

/// What a run streamed: agent `text` chunks and the ordered `answer` text pieces.
struct Streamed {
    chunks: Vec<String>,
    responses: Vec<String>,
    answer: String,
}

async fn run_agent(provider: &str, model: Arc<dyn ChatModel>) -> Streamed {
    let mut registry = Registry::with_builtins();
    graphrt_llm::register(&mut registry);
    let doc = json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start", "variables": []},
            {"id": "agent", "type": "agent", "model": {"provider": provider, "name": "m"},
             "instruction": "You add numbers.", "tools": ["calc/*"]},
            {"id": "out", "type": "answer", "answer": "{{#agent.text#}}"}
        ],
        "edges": [{"source": "start", "target": "agent"}, {"source": "agent", "target": "out"}]
    }});
    let graph = Arc::new(Graph::build(Workflow::from_value(doc).unwrap(), &registry).unwrap());
    let services = Services::new().with(ModelRegistry::new().with(provider, model)).with(add_tools());
    let mut assembler = ResponseAssembler::new(&graph);
    let events: Vec<Event> =
        Engine::new(graph).with_services(services).run(RunInput::new().sys("query", "2+3?")).collect().await;
    let mut streamed = Streamed { chunks: Vec::new(), responses: Vec::new(), answer: String::new() };
    for event in &events {
        if let Event::NodeChunk { node_id, field, chunk, .. } = event
            && node_id.as_ref() == "agent"
            && field.as_ref() == "text"
        {
            streamed.chunks.push(chunk.clone());
        }
        streamed.responses.extend(assembler.push(event).into_iter().filter(|c| !c.is_final).map(|c| c.text));
    }
    match events.last() {
        Some(Event::RunSucceeded { outputs, .. }) => streamed.answer = outputs["answer"].to_text(),
        other => panic!("expected RunSucceeded, got {other:?}"),
    }
    streamed
}

fn assert_streamed(streamed: &Streamed, preamble: &[&str], answer: &[&str]) {
    let mut expected: Vec<&str> = preamble.to_vec();
    expected.push("\n\n");
    expected.extend(answer);
    assert_eq!(streamed.chunks, expected, "one chunk per provider token");
    assert_eq!(streamed.responses, expected, "the answer node streams the same tokens live");
    assert_eq!(streamed.answer, expected.concat());
    assert!(streamed.chunks.iter().all(|c| !c.contains('{')), "no tool-call JSON in the text stream");
}

#[cfg(feature = "openai")]
#[tokio::test]
async fn openai_agent_streams_every_turn_per_token() {
    use graphrt_llm::openai::{OpenAiConfig, OpenAiModel};
    let sse = |chunks: &[Json]| -> String {
        let mut out: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
        out.push_str("data: [DONE]\n\n");
        out
    };
    let delta = |d: Json| json!({"choices": [{"index": 0, "delta": d}]});
    let finish = |r: &str| json!({"choices": [{"index": 0, "delta": {}, "finish_reason": r}]});
    let first = sse(&[
        delta(json!({"role": "assistant", "content": "Let me"})),
        delta(json!({"content": " add."})),
        delta(json!({"tool_calls": [{"index": 0, "id": "c1", "type": "function",
                                     "function": {"name": "add", "arguments": "{\"a\":"}}]})),
        delta(json!({"tool_calls": [{"index": 0, "function": {"arguments": "2,\"b\":3}"}}]})),
        finish("tool_calls"),
    ]);
    let second = sse(&[
        delta(json!({"content": "It"})),
        delta(json!({"content": " is"})),
        delta(json!({"content": " 5."})),
        finish("stop"),
    ]);
    let url = serve(vec![first, second]).await;
    let model = Arc::new(OpenAiModel::new(OpenAiConfig::new(url, Some("sk-test".into()))).unwrap());
    let streamed = run_agent("openai", model).await;
    assert_streamed(&streamed, &["Let me", " add."], &["It", " is", " 5."]);
}

#[cfg(feature = "anthropic")]
#[tokio::test]
async fn anthropic_agent_streams_every_turn_per_token() {
    use graphrt_llm::anthropic::{AnthropicConfig, AnthropicModel};
    let sse = |events: &[Json]| -> String {
        events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())).collect()
    };
    let start = json!({"type": "message_start", "message": {"usage": {"input_tokens": 5, "output_tokens": 1}}});
    let text_block = |index: u32| json!({"type": "content_block_start", "index": index, "content_block": {"type": "text", "text": ""}});
    let text = |index: u32, t: &str| json!({"type": "content_block_delta", "index": index, "delta": {"type": "text_delta", "text": t}});
    let stop_block = |index: u32| json!({"type": "content_block_stop", "index": index});
    let end = |reason: &str| {
        [
            json!({"type": "message_delta", "delta": {"stop_reason": reason}, "usage": {"output_tokens": 9}}),
            json!({"type": "message_stop"}),
        ]
    };
    let mut first = vec![start.clone(), text_block(0), text(0, "Let me"), text(0, " add."), stop_block(0)];
    first.extend([
        json!({"type": "content_block_start", "index": 1,
               "content_block": {"type": "tool_use", "id": "tu_1", "name": "add", "input": {}}}),
        json!({"type": "content_block_delta", "index": 1,
               "delta": {"type": "input_json_delta", "partial_json": "{\"a\": 2,"}}),
        json!({"type": "content_block_delta", "index": 1,
               "delta": {"type": "input_json_delta", "partial_json": " \"b\": 3}"}}),
        stop_block(1),
    ]);
    first.extend(end("tool_use"));
    let mut second = vec![start, text_block(0), text(0, "It"), text(0, " is"), text(0, " 5."), stop_block(0)];
    second.extend(end("end_turn"));
    let url = serve(vec![sse(&first), sse(&second)]).await;
    let mut config = AnthropicConfig::new("sk-ant-test");
    config.base_url = url;
    let streamed = run_agent("anthropic", Arc::new(AnthropicModel::new(config).unwrap())).await;
    assert_streamed(&streamed, &["Let me", " add."], &["It", " is", " 5."]);
}
