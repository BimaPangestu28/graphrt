//! Integration tests against a local HTTP/1.1 responder.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use graphrt_core::dsl::NodeDoc;
use graphrt_core::node::{Node, NodeContext, NodeError, NodeOutcome, NodeOutput};
use graphrt_core::{Services, Value, VariablePool};
use graphrt_http::{EgressPolicy, HttpClient, HttpRequestNode, KIND};

/// A canned response.
struct Reply {
    status: &'static str,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn text(status: &'static str, content_type: &'static str, body: &str) -> Self {
        Self { status, headers: vec![("content-type", content_type.to_owned())], body: body.as_bytes().to_vec() }
    }
}

/// Serve one reply per connection; forward each raw request to `requests`.
async fn serve(replies: Vec<Reply>) -> (String, mpsc::UnboundedReceiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        for reply in replies {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let raw = read_request(&mut socket).await;
            let _ = tx.send(raw);
            let mut head =
                format!("HTTP/1.1 {}\r\ncontent-length: {}\r\nconnection: close\r\n", reply.status, reply.body.len());
            for (k, v) in &reply.headers {
                head.push_str(&format!("{k}: {v}\r\n"));
            }
            head.push_str("\r\n");
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(&reply.body).await;
            let _ = socket.shutdown().await;
        }
    });
    (format!("http://{addr}"), rx)
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
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

fn pool() -> VariablePool {
    let mut p = VariablePool::new();
    p.insert("start", "name", Value::from("Ada \"the\" {Countess}"));
    p.insert("start", "n", Value::Int(7));
    p.insert("env", "TOKEN", Value::Secret(Arc::from("tok-secret-123")));
    p
}

async fn run_with(config: serde_json::Value, policy: EgressPolicy) -> Result<NodeOutput, NodeError> {
    let node = HttpRequestNode::from_doc(&NodeDoc::new("h", KIND, config)).unwrap();
    let services = Services::new().with(HttpClient::new(policy).unwrap());
    let ctx = NodeContext::for_test("h", pool()).with_services(Arc::new(services));
    match node.run(ctx).await? {
        NodeOutcome::Completed(o) => Ok(o),
        NodeOutcome::Paused(_) => panic!("unexpected pause"),
    }
}

#[tokio::test]
async fn get_with_params_headers_and_bearer_auth() {
    let (base, mut rx) = serve(vec![Reply::text("200 OK", "application/json", r#"{"ok":true,"n":3}"#)]).await;
    let out = run_with(
        serde_json::json!({
            "method": "get",
            "url": format!("{base}/items"),
            "params": "q: {{#start.n#}}\nkey: {{#env.TOKEN#}}",
            "headers": [{"key": "X-Name", "value": "{{#start.n#}}"}],
            "authorization": {"type": "api-key", "config": {"type": "bearer", "api_key": "{{#env.TOKEN#}}"}},
        }),
        EgressPolicy::trusted(),
    )
    .await
    .unwrap();

    let raw = rx.recv().await.unwrap();
    assert!(raw.starts_with("GET /items?q=7&key=tok-secret-123 HTTP/1.1"), "{raw}");
    assert!(raw.to_ascii_lowercase().contains("authorization: bearer tok-secret-123"));
    assert!(raw.to_ascii_lowercase().contains("x-name: 7"));

    assert_eq!(out.outputs.get("status_code"), Some(&Value::Int(200)));
    assert_eq!(out.outputs.get("json"), Some(&Value::from_json(serde_json::json!({"ok": true, "n": 3}))));
    let meta = serde_json::to_string(&out.metadata).unwrap();
    assert!(!meta.contains("tok-secret-123"), "secret leaked into metadata: {meta}");
}

#[tokio::test]
async fn post_json_body_is_injection_safe() {
    let (base, mut rx) = serve(vec![Reply::text("201 Created", "text/plain", "created")]).await;
    let out = run_with(
        serde_json::json!({
            "method": "POST",
            "url": format!("{base}/users"),
            "body": {"type": "json", "data": [{"type": "text", "value": "{\"name\": \"{{#start.name#}}\", \"age\": {{#start.n#}}}"}]},
        }),
        EgressPolicy::trusted(),
    )
    .await
    .unwrap();
    let raw = rx.recv().await.unwrap();
    let body = raw.split("\r\n\r\n").nth(1).unwrap();
    let json: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(json, serde_json::json!({"name": "Ada \"the\" {Countess}", "age": 7}));
    assert!(raw.to_ascii_lowercase().contains("content-type: application/json"));
    assert_eq!(out.outputs.get("body"), Some(&Value::from("created")));
}

#[tokio::test]
async fn form_urlencoded_body() {
    let (base, mut rx) = serve(vec![Reply::text("200 OK", "text/plain", "ok")]).await;
    run_with(
        serde_json::json!({
            "method": "post",
            "url": base,
            "body": {"type": "x-www-form-urlencoded", "data": [{"key": "a", "value": "1 2"}, {"key": "b", "value": "{{#start.n#}}"}]},
        }),
        EgressPolicy::trusted(),
    )
    .await
    .unwrap();
    let raw = rx.recv().await.unwrap();
    assert!(raw.ends_with("a=1+2&b=7"), "{raw}");
}

#[tokio::test]
async fn error_status_fails_and_is_retryable_for_5xx_get() {
    let (base, _rx) = serve(vec![Reply::text("503 Service Unavailable", "text/plain", "down")]).await;
    let err = run_with(serde_json::json!({ "url": base }), EgressPolicy::trusted()).await.unwrap_err();
    assert_eq!(err.kind, "http_status");
    assert!(err.retryable);
    assert!(err.message.contains("503"));
}

#[tokio::test]
async fn error_status_on_post_is_not_retryable() {
    let (base, _rx) = serve(vec![Reply::text("500 Internal Server Error", "text/plain", "x")]).await;
    let err =
        run_with(serde_json::json!({ "url": base, "method": "post" }), EgressPolicy::trusted()).await.unwrap_err();
    assert!(!err.retryable);
}

#[tokio::test]
async fn error_status_can_be_returned_as_output() {
    let (base, _rx) = serve(vec![Reply::text("404 Not Found", "text/plain", "nope")]).await;
    let out = run_with(serde_json::json!({ "url": base, "fail_on_error_status": false }), EgressPolicy::trusted())
        .await
        .unwrap();
    assert_eq!(out.outputs.get("status_code"), Some(&Value::Int(404)));
    assert_eq!(out.outputs.get("body"), Some(&Value::from("nope")));
}

#[tokio::test]
async fn response_size_cap() {
    let big = "x".repeat(10_000);
    let (base, _rx) = serve(vec![Reply::text("200 OK", "text/plain", &big)]).await;
    let err = run_with(serde_json::json!({ "url": base, "max_response_bytes": 1000 }), EgressPolicy::trusted())
        .await
        .unwrap_err();
    assert_eq!(err.kind, "response_too_large");
}

#[tokio::test]
async fn binary_response_becomes_inline_file() {
    let reply = Reply {
        status: "200 OK",
        headers: vec![
            ("content-type", "image/png".to_owned()),
            ("content-disposition", "attachment; filename=\"pic.png\"".to_owned()),
        ],
        body: vec![0x89, b'P', b'N', b'G'],
    };
    let (base, _rx) = serve(vec![reply]).await;
    let out = run_with(serde_json::json!({ "url": format!("{base}/x") }), EgressPolicy::trusted()).await.unwrap();
    let files = out.outputs.get("files").and_then(Value::as_array).unwrap();
    let file = files[0].as_file().unwrap();
    assert_eq!(file.name, "pic.png");
    assert_eq!(file.mime_type.as_deref(), Some("image/png"));
    assert!(file.url.as_deref().unwrap().starts_with("data:image/png;base64,"));
    assert_eq!(out.outputs.get("body"), Some(&Value::from("")));
}

#[tokio::test]
async fn private_destination_blocked_by_default() {
    let (base, _rx) = serve(vec![]).await;
    let err = run_with(serde_json::json!({ "url": base }), EgressPolicy::default()).await.unwrap_err();
    assert_eq!(err.kind, "egress_blocked");
    assert!(!err.retryable);
}

#[tokio::test]
async fn localhost_name_blocked_by_resolver() {
    let (base, _rx) = serve(vec![]).await;
    let url = base.replace("127.0.0.1", "localhost");
    let err = run_with(serde_json::json!({ "url": url }), EgressPolicy::default()).await.unwrap_err();
    assert_eq!(err.kind, "egress_blocked", "{err}");
}

#[tokio::test]
async fn redirect_to_blocked_host_is_refused() {
    // Allow private addresses (so the test server is reachable) but deny the redirect target host.
    let (base, _rx) = serve(vec![Reply {
        status: "302 Found",
        headers: vec![("location", "http://metadata.internal.test/latest".to_owned())],
        body: Vec::new(),
    }])
    .await;
    let policy = EgressPolicy { denied_hosts: vec!["*.internal.test".into()], ..EgressPolicy::trusted() };
    let err = run_with(serde_json::json!({ "url": base }), policy).await.unwrap_err();
    assert_eq!(err.kind, "egress_blocked", "{err}");
}

#[tokio::test]
async fn redirect_to_literal_private_ip_is_refused() {
    // The first hop is reachable via an allow-listed host; the redirect points at a literal private IP.
    let (base, _rx) = serve(vec![Reply {
        status: "302 Found",
        headers: vec![("location", "http://169.254.169.254/latest/meta-data".to_owned())],
        body: Vec::new(),
    }])
    .await;
    // Simulate "public origin redirects to private": check the redirect target with the default policy.
    let target = reqwest::Url::parse("http://169.254.169.254/latest/meta-data").unwrap();
    assert!(EgressPolicy::default().check_url(&target).is_err());
    // And with redirects disabled, the 302 is returned as an error status.
    let policy = EgressPolicy { max_redirects: 0, ..EgressPolicy::trusted() };
    let err = run_with(serde_json::json!({ "url": base }), policy).await.unwrap_err();
    assert_eq!(err.kind, "egress_blocked", "{err}");
}

#[tokio::test]
async fn timeout_is_reported() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((socket, _)) = listener.accept().await {
            tokio::time::sleep(Duration::from_secs(5)).await;
            drop(socket);
        }
    });
    let err = run_with(
        serde_json::json!({ "url": format!("http://{addr}"), "timeout": {"read": 0.3} }),
        EgressPolicy::trusted(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind, "timeout", "{err}");
}
