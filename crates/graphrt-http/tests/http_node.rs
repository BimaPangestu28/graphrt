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
    assert_eq!(err.kind, "http_status", "{err}");
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

fn redirect(status: &'static str, location: String) -> Reply {
    Reply { status, headers: vec![("location", location)], body: Vec::new() }
}

#[tokio::test]
async fn cross_origin_redirect_strips_credentials() {
    let (target, mut target_rx) = serve(vec![Reply::text("200 OK", "text/plain", "ok")]).await;
    let (origin, mut origin_rx) = serve(vec![redirect("302 Found", format!("{target}/landing"))]).await;
    let out = run_with(
        serde_json::json!({
            "url": format!("{origin}/start"),
            "headers": [
                {"key": "X-Plain", "value": "visible"},
                {"key": "X-Token", "value": "t={{#env.TOKEN#}}"},
                {"key": "Cookie", "value": "session=abc"}
            ],
            "authorization": {"type": "api-key", "config": {"type": "custom", "header": "X-Api-Key", "api_key": "{{#env.TOKEN#}}"}},
        }),
        EgressPolicy::trusted(),
    )
    .await
    .unwrap();
    assert_eq!(out.outputs.get("body"), Some(&Value::from("ok")));

    let first = origin_rx.recv().await.unwrap().to_ascii_lowercase();
    assert!(first.contains("x-api-key: tok-secret-123"), "{first}");
    assert!(first.contains("x-token: t=tok-secret-123"), "{first}");

    let second = target_rx.recv().await.unwrap().to_ascii_lowercase();
    assert!(second.starts_with("get /landing"), "{second}");
    assert!(second.contains("x-plain: visible"), "{second}");
    assert!(!second.contains("tok-secret-123"), "credentials leaked across origins: {second}");
    assert!(!second.contains("x-api-key"), "{second}");
    assert!(!second.contains("cookie"), "{second}");
}

#[tokio::test]
async fn same_origin_redirect_keeps_credentials_and_303_becomes_get() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let replies = [
            format!(
                "HTTP/1.1 303 See Other\r\nlocation: http://{addr}/after\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            ),
            "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 4\r\nconnection: close\r\n\r\ndone"
                .to_owned(),
        ];
        for reply in replies {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let raw = read_request(&mut socket).await;
            let _ = tx.send(raw);
            let _ = socket.write_all(reply.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    let out = run_with(
        serde_json::json!({
            "method": "post",
            "url": format!("http://{addr}/submit"),
            "body": {"type": "raw-text", "data": "payload"},
            "authorization": {"type": "api-key", "config": {"type": "custom", "header": "X-Api-Key", "api_key": "{{#env.TOKEN#}}"}},
        }),
        EgressPolicy::trusted(),
    )
    .await
    .unwrap();
    assert_eq!(out.outputs.get("body"), Some(&Value::from("done")));
    let first = rx.recv().await.unwrap().to_ascii_lowercase();
    assert!(first.starts_with("post /submit"), "{first}");
    let second = rx.recv().await.unwrap().to_ascii_lowercase();
    assert!(second.starts_with("get /after"), "{second}");
    assert!(second.contains("x-api-key: tok-secret-123"), "{second}");
    assert!(!second.contains("payload"), "{second}");
}

#[tokio::test]
async fn redirect_limits() {
    // With redirects disabled the 3xx is the response.
    let (origin, _o) = serve(vec![redirect("302 Found", "http://127.0.0.1:9/x".to_owned())]).await;
    let policy = EgressPolicy { max_redirects: 0, ..EgressPolicy::trusted() };
    let out = run_with(serde_json::json!({ "url": origin, "fail_on_error_status": false }), policy).await.unwrap();
    assert_eq!(out.outputs.get("status_code"), Some(&Value::Int(302)));

    let (c, _c) =
        serve(vec![redirect("302 Found", "/again".to_owned()), redirect("302 Found", "/again".to_owned())]).await;
    let policy = EgressPolicy { max_redirects: 1, ..EgressPolicy::trusted() };
    let err = run_with(serde_json::json!({ "url": c }), policy).await.unwrap_err();
    assert_eq!(err.kind, "too_many_redirects", "{err}");
    assert!(!err.retryable);
}

#[tokio::test]
async fn node_limits_cannot_exceed_policy_caps() {
    let big = "x".repeat(10_000);
    let (base, _rx) = serve(vec![Reply::text("200 OK", "text/plain", &big)]).await;
    let policy = EgressPolicy { max_response_bytes: 1000, ..EgressPolicy::trusted() };
    let err = run_with(serde_json::json!({ "url": base, "max_response_bytes": 1_000_000_000_000u64 }), policy)
        .await
        .unwrap_err();
    assert_eq!(err.kind, "response_too_large");

    // A node timeout of a day is clamped to the policy's total timeout.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((socket, _)) = listener.accept().await {
            tokio::time::sleep(Duration::from_secs(10)).await;
            drop(socket);
        }
    });
    let policy = EgressPolicy { total_timeout: Duration::from_millis(300), ..EgressPolicy::trusted() };
    let started = std::time::Instant::now();
    let err = run_with(serde_json::json!({ "url": format!("http://{addr}"), "timeout": {"read": 86400} }), policy)
        .await
        .unwrap_err();
    assert_eq!(err.kind, "timeout", "{err}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn transport_errors_do_not_leak_url_secrets() {
    // Bind then drop to get a closed port.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let url = format!("http://{addr}/v1?key={}", "{{#env.TOKEN#}}");
    let err = run_with(serde_json::json!({ "url": url }), EgressPolicy::trusted()).await.unwrap_err();
    assert_eq!(err.kind, "connection", "{err}");
    assert!(!err.message.contains("tok-secret-123"), "{}", err.message);
}

#[tokio::test]
async fn latin1_text_is_decoded() {
    let reply = Reply {
        status: "200 OK",
        headers: vec![("content-type", "text/html; charset=ISO-8859-1".to_owned())],
        body: b"caf\xe9".to_vec(),
    };
    let (base, _rx) = serve(vec![reply]).await;
    let out = run_with(serde_json::json!({ "url": base }), EgressPolicy::trusted()).await.unwrap();
    assert_eq!(out.outputs.get("body"), Some(&Value::from("café")));
    assert_eq!(out.outputs.get("files").and_then(Value::as_array).map(<[Value]>::len), Some(0));
}

#[tokio::test]
async fn undecodable_text_is_lossy_not_binary() {
    let reply = Reply {
        status: "200 OK",
        headers: vec![("content-type", "text/plain".to_owned())],
        body: b"ok \xff\xfe end".to_vec(),
    };
    let (base, _rx) = serve(vec![reply]).await;
    let out = run_with(serde_json::json!({ "url": base }), EgressPolicy::trusted()).await.unwrap();
    let body = out.outputs.get("body").and_then(Value::as_str).unwrap().to_owned();
    assert!(body.starts_with("ok ") && body.ends_with(" end"), "{body}");
    assert_eq!(out.metadata.get("body_lossy"), Some(&Value::Bool(true)));
}

#[tokio::test]
async fn invalid_json_body_is_reported() {
    let (base, _rx) = serve(vec![Reply::text("200 OK", "application/json", "{not json")]).await;
    let out = run_with(serde_json::json!({ "url": base }), EgressPolicy::trusted()).await.unwrap();
    assert_eq!(out.outputs.get("json"), Some(&Value::Null));
    assert_eq!(out.outputs.get("body"), Some(&Value::from("{not json")));
    assert!(out.metadata.contains_key("json_error"));

    let (base, _rx) = serve(vec![Reply::text("200 OK", "application/json", "{not json")]).await;
    let err = run_with(serde_json::json!({ "url": base, "parse_json": "strict" }), EgressPolicy::trusted())
        .await
        .unwrap_err();
    assert_eq!(err.kind, "invalid_response");
}

#[tokio::test]
async fn large_error_page_is_a_status_error() {
    let big = "e".repeat(10_000);
    let (base, _rx) = serve(vec![Reply::text("503 Service Unavailable", "text/html", &big)]).await;
    let policy = EgressPolicy { max_response_bytes: 1000, ..EgressPolicy::trusted() };
    let err = run_with(serde_json::json!({ "url": base }), policy).await.unwrap_err();
    assert_eq!(err.kind, "http_status", "{err}");
    assert!(err.retryable);
}

#[tokio::test]
async fn header_key_rendering_empty_is_an_error() {
    let (base, _rx) = serve(vec![]).await;
    let err = run_with(
        serde_json::json!({ "url": base, "headers": [{"key": "{{#start.missing#}}", "value": "v"}] }),
        EgressPolicy::trusted(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind, "invalid_input", "{err}");
}

#[test]
fn inputs_cover_body_and_auth() {
    let node = HttpRequestNode::from_doc(&NodeDoc::new(
        "h",
        KIND,
        serde_json::json!({
            "url": "https://x/{{#a.path#}}",
            "authorization": {"type": "api-key", "config": {"type": "bearer", "api_key": "{{#env.K#}}"}},
            "body": {"type": "json", "data": "{\"q\": \"{{#b.q#}}\"}"},
        }),
    ))
    .unwrap();
    let names: Vec<String> = node.inputs().iter().map(ToString::to_string).collect();
    assert!(names.contains(&"a.path".to_owned()));
    assert!(names.contains(&"env.K".to_owned()));
    assert!(names.contains(&"b.q".to_owned()));
}

/// Send one request with `config` and `sys.run_id = run-7`; return the raw request.
async fn raw_request_with_run_id(config: serde_json::Value) -> String {
    let (base, mut rx) = serve(vec![Reply::text("200 OK", "text/plain", "ok")]).await;
    let mut config = config;
    config["url"] = serde_json::Value::from(format!("{base}/x"));
    let node = HttpRequestNode::from_doc(&NodeDoc::new("h", KIND, config)).unwrap();
    let services = Services::new().with(HttpClient::new(EgressPolicy::trusted()).unwrap());
    let mut pool = pool();
    pool.insert("sys", "run_id", Value::from("run-7"));
    let ctx = NodeContext::for_test("h", pool).with_services(Arc::new(services));
    node.run(ctx).await.unwrap();
    rx.recv().await.unwrap().to_ascii_lowercase()
}

#[tokio::test]
async fn side_effecting_requests_carry_an_idempotency_key() {
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let raw = raw_request_with_run_id(serde_json::json!({ "method": method })).await;
        // `<run_id>:<exec_id>`: stable across retries and re-execution after a resume.
        assert!(raw.contains("idempotency-key: run-7:h-exec\r\n"), "{method}: {raw}");
    }
    for method in ["GET", "HEAD", "OPTIONS"] {
        let raw = raw_request_with_run_id(serde_json::json!({ "method": method })).await;
        assert!(!raw.contains("idempotency-key"), "{method}: {raw}");
    }
}

#[tokio::test]
async fn the_workflow_can_set_or_disable_the_idempotency_key() {
    let own = serde_json::json!({ "method": "POST", "headers": [{"key": "Idempotency-Key", "value": "mine"}] });
    let raw = raw_request_with_run_id(own).await;
    assert!(raw.contains("idempotency-key: mine\r\n") && !raw.contains("run-7:h-exec"), "{raw}");
    let off = serde_json::json!({ "method": "POST", "idempotency_key": false });
    assert!(!raw_request_with_run_id(off).await.contains("idempotency-key"));
}
