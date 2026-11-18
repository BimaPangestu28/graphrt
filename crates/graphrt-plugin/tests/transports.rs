//! HTTP plugins and in-process plugins built with the Rust SDK.

#![cfg(feature = "http")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use graphrt_core::engine::{Engine, RunInput};
use graphrt_core::node::{Services, TokioClock};
use graphrt_core::tool::ToolRegistry;
use graphrt_core::{Event, Graph, Registry, Workflow};
use graphrt_plugin::{IoTransport, LoadedManifest, PluginHost, PluginManifest};
use graphrt_plugin_sdk::{Plugin, RunResult, ToolResult};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn workflow(kind: &str, config: serde_json::Value, out: serde_json::Value) -> serde_json::Value {
    let mut node = json!({"id": "p", "type": kind});
    for (k, v) in config.as_object().cloned().unwrap_or_default() {
        node[k] = v;
    }
    json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start", "variables": [{"variable": "text", "type": "string"}]},
            node,
            {"id": "end", "type": "end", "outputs": out}
        ],
        "edges": [{"source": "start", "target": "p"}, {"source": "p", "target": "end"}]
    }})
}

async fn run(registry: &Registry, tools: &ToolRegistry, doc: serde_json::Value, input: RunInput) -> Vec<Event> {
    let graph = Graph::build(Workflow::from_value(doc).unwrap(), registry).unwrap();
    let engine = Engine::new(Arc::new(graph))
        .with_services(Services::new().with(tools.clone()))
        .with_clock(Arc::new(TokioClock));
    tokio::time::timeout(Duration::from_secs(20), engine.run(input).collect::<Vec<_>>()).await.unwrap()
}

fn succeeded(events: &[Event]) -> serde_json::Value {
    match events.last() {
        Some(Event::RunSucceeded { outputs, .. }) => serde_json::to_value(outputs).unwrap(),
        other => panic!("run did not succeed: {other:?}"),
    }
}

/// A minimal HTTP/1.1 server speaking the plugin protocol: `initialize`
/// answers as JSON, `node.run` streams NDJSON (a chunk notification, then the result).
async fn serve_http_plugin() -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let (head_end, content_len) = loop {
                    let n = sock.read(&mut tmp).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                        assert!(head.contains("authorization: bearer graphrt-plugin"), "missing bearer token: {head}");
                        assert!(head.contains("x-plugin-key: k1"), "missing custom header: {head}");
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                            .unwrap_or(0);
                        break (pos + 4, len);
                    }
                };
                while buf.len() < head_end + content_len {
                    let n = sock.read(&mut tmp).await.unwrap();
                    buf.extend_from_slice(&tmp[..n]);
                }
                let req: serde_json::Value = serde_json::from_slice(&buf[head_end..head_end + content_len]).unwrap();
                let id = req["id"].clone();
                let mut status = "200 OK";
                let (ctype, body) = match req["method"].as_str() {
                    Some("initialize") => (
                        "application/json",
                        json!({"jsonrpc": "2.0", "id": id, "result": {"protocol_version": 1}}).to_string(),
                    ),
                    Some("node.run") if req["params"]["config"]["text"] == "reject" => {
                        // An error sent with a non-2xx status keeps its kind and retryability.
                        status = "422 Unprocessable Entity";
                        let err = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000,
                            "message": "text rejected", "data": {"kind": "bad_text", "retryable": false}}});
                        ("application/json", err.to_string())
                    }
                    Some("node.run") if req["params"]["config"]["text"] == "gateway" => {
                        status = "502 Bad Gateway";
                        ("text/plain", "upstream down".to_owned())
                    }
                    Some("node.run") => {
                        let rid = req["params"]["request_id"].clone();
                        let text = req["params"]["config"]["text"].as_str().unwrap_or_default().to_owned();
                        let chunk = json!({"jsonrpc": "2.0", "method": "node.chunk",
                            "params": {"request_id": rid, "field": "result", "chunk": "rev:"}});
                        let result = json!({"jsonrpc": "2.0", "id": id,
                            "result": {"outputs": {"result": text.chars().rev().collect::<String>()}}});
                        ("application/x-ndjson", format!("{chunk}\n{result}\n"))
                    }
                    Some("tool.call") => (
                        "application/json",
                        json!({"jsonrpc": "2.0", "id": id, "result": {"text": "pong"}}).to_string(),
                    ),
                    _ => (
                        "application/json",
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "nope"}}).to_string(),
                    ),
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_runtime_streams_ndjson() {
    let addr = serve_http_plugin().await;
    // The bearer token is read from a host environment variable; cargo sets
    // CARGO_PKG_NAME for test processes, so use it instead of setting one.
    let manifest = PluginManifest::from_json(
        &json!({
            "name": "remote",
            "runtime": "http",
            "url": format!("http://{addr}/rpc"),
            "headers": {"x-plugin-key": "k1"},
            "bearer_token_env": "CARGO_PKG_NAME",
            "permissions": {"env": ["CARGO_PKG_NAME"]},
            "nodes": [{"kind": "remote.reverse", "title": "Reverse"}],
            "tools": [{"name": "ping", "description": "", "parameters": {}}]
        })
        .to_string(),
    )
    .unwrap();
    // Sending a host variable to a remote endpoint needs the operator's trust.
    let untrusted = PluginHost::new().add(LoadedManifest { manifest: manifest.clone(), dir: PathBuf::from(".") });
    assert!(matches!(untrusted, Err(e) if e.to_string().contains("trust")));
    let mut host = PluginHost::new().trust("remote");
    host.add(LoadedManifest { manifest, dir: PathBuf::from(".") }).unwrap();
    let mut registry = Registry::with_builtins();
    let mut tools = ToolRegistry::new();
    host.register(&mut registry, &mut tools).unwrap();

    let doc = workflow(
        "remote.reverse",
        json!({"text": "{{#start.text#}}"}),
        json!([{"variable": "out", "value_selector": ["p", "result"]}]),
    );
    let events = run(&registry, &tools, doc, RunInput::new().input("text", "abc")).await;
    assert_eq!(succeeded(&events)["out"], "cba");
    assert!(events.iter().any(|e| matches!(e, Event::NodeChunk { chunk, .. } if chunk == "rev:")));
    let pong = tools.get("remote").unwrap().call("ping", json!({})).await.unwrap();
    assert_eq!(pong.text, "pong");

    // Error bodies of non-2xx responses are kept, not discarded.
    let out = json!([{"variable": "out", "value_selector": ["p", "result"]}]);
    let doc = workflow("remote.reverse", json!({"text": "{{#start.text#}}"}), out.clone());
    let events = run(&registry, &tools, doc, RunInput::new().input("text", "reject")).await;
    assert!(
        matches!(events.last(), Some(Event::RunFailed { error_type, error, .. })
            if error_type == "bad_text" && error.contains("text rejected")),
        "{:?}",
        events.last()
    );
    let doc = workflow("remote.reverse", json!({"text": "{{#start.text#}}"}), out);
    let events = run(&registry, &tools, doc, RunInput::new().input("text", "gateway")).await;
    assert!(
        matches!(events.last(), Some(Event::RunFailed { error_type, error, .. })
            if error_type == "plugin_transport" && error.contains("upstream down")),
        "{:?}",
        events.last()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rust_sdk_plugin_in_process() {
    // Wire the SDK's server and the host's transport through an in-memory pipe.
    let (host_side, plugin_side) = tokio::io::duplex(1 << 16);
    let (plugin_r, plugin_w) = tokio::io::split(plugin_side);
    let plugin = Plugin::new("stats")
        .node("stats.words", |ctx, req| async move {
            let text = req.config["text"].as_str().unwrap_or_default().to_owned();
            ctx.chunk("count", "counting");
            ctx.progress("analyzing", json!({"len": text.len()}));
            let words = text.split_whitespace().count();
            Ok(RunResult::new().output("words", words).branch(if words > 2 { "long" } else { "short" }))
        })
        .tool(json!({"name": "echo", "description": "", "parameters": {}}), |call| async move {
            Ok(ToolResult::text(call.arguments["v"].to_string()))
        });
    tokio::spawn(plugin.serve(plugin_r, plugin_w));

    let (host_r, host_w) = tokio::io::split(host_side);
    let transport = Arc::new(IoTransport::new(host_r, host_w, 1 << 20));
    let manifest = PluginManifest::from_json(
        &json!({
            "name": "stats",
            "runtime": "process",
            "command": "in-process",
            "nodes": [{"kind": "stats.words", "title": "Words", "handles": {"kind": "fixed", "handles": ["short", "long"]}}],
            "tools": [{"name": "echo", "description": "", "parameters": {}}]
        })
        .to_string(),
    )
    .unwrap();
    let mut host = PluginHost::new();
    host.add_with_transport(LoadedManifest { manifest, dir: PathBuf::from(".") }, transport).unwrap();
    let mut registry = Registry::with_builtins();
    let mut tools = ToolRegistry::new();
    host.register(&mut registry, &mut tools).unwrap();

    let doc = json!({"graph": {
        "nodes": [
            {"id": "start", "type": "start", "variables": [{"variable": "text", "type": "string"}]},
            {"id": "p", "type": "stats.words", "text": "{{#start.text#}}"},
            {"id": "long", "type": "answer", "answer": "long: {{#p.words#}}"},
            {"id": "short", "type": "answer", "answer": "short: {{#p.words#}}"}
        ],
        "edges": [
            {"source": "start", "target": "p"},
            {"source": "p", "target": "long", "handle": "long"},
            {"source": "p", "target": "short", "handle": "short"}
        ]
    }});
    let events = run(&registry, &tools, doc, RunInput::new().input("text", "one two three four")).await;
    assert_eq!(succeeded(&events)["answer"], "long: 4");
    assert!(events.iter().any(|e| matches!(e, Event::NodeProgress { name, .. } if name == "analyzing")));
    let echoed = tools.get("stats").unwrap().call("echo", json!({"v": 7})).await.unwrap();
    assert_eq!(echoed.text, "7");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugin_tool_images_reach_the_host() {
    let (host_side, plugin_side) = tokio::io::duplex(1 << 16);
    let (plugin_r, plugin_w) = tokio::io::split(plugin_side);
    let plugin = Plugin::new("screen")
        .tool(json!({"name": "shot", "description": "", "parameters": {}}), |_call| async move {
            Ok(ToolResult::text("800x600").with_image_base64("image/png", "aGVsbG8="))
        })
        .tool(json!({"name": "bad", "description": "", "parameters": {}}), |_call| async move {
            Ok(ToolResult::text("odd").with_image_base64("image/bmp", "aGVs"))
        });
    tokio::spawn(plugin.serve(plugin_r, plugin_w));
    let (host_r, host_w) = tokio::io::split(host_side);
    let transport = Arc::new(IoTransport::new(host_r, host_w, 1 << 20));
    let manifest = PluginManifest::from_json(
        &json!({
            "name": "screen",
            "runtime": "process",
            "command": "in-process",
            "tools": [{"name": "shot", "description": "", "parameters": {}}, {"name": "bad", "description": "", "parameters": {}}]
        })
        .to_string(),
    )
    .unwrap();
    let mut host = PluginHost::new();
    host.add_with_transport(LoadedManifest { manifest, dir: PathBuf::from(".") }, transport).unwrap();
    let mut registry = Registry::with_builtins();
    let mut tools = ToolRegistry::new();
    host.register(&mut registry, &mut tools).unwrap();

    let runtime = tools.get("screen").unwrap();
    let shot = runtime.call("shot", json!({})).await.unwrap();
    assert_eq!(shot.text, "800x600");
    assert_eq!(shot.images, vec![graphrt_core::tool::ToolImage::png("aGVsbG8=")]);
    let err = runtime.call("bad", json!({})).await.unwrap_err();
    assert!(err.message.contains("invalid `images`") && err.message.contains("image/bmp"), "{err}");
}
