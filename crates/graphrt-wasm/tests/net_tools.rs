//! Guest tools (`graphrt_tool_call`) and outbound HTTP (`graphrt::http`) with a recording egress.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use graphrt_core::tool::ToolCallContext;
use graphrt_wasm::{HttpEgress, HttpError, HttpRequest, HttpResponse, WasmError, WasmHost, WasmLimits, WasmPlugin};

/// Answers every request with `reply` and records what it was asked.
#[derive(Debug)]
struct RecordingEgress {
    reply: Result<HttpResponse, HttpError>,
    seen: Mutex<Vec<HttpRequest>>,
}

impl RecordingEgress {
    fn new(reply: Result<HttpResponse, HttpError>) -> Arc<Self> {
        Arc::new(Self { reply, seen: Mutex::new(Vec::new()) })
    }
}

impl HttpEgress for RecordingEgress {
    fn send(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        self.seen.lock().unwrap().push(request);
        self.reply.clone()
    }
}

const MANIFEST: &str =
    r#"{"name":"net","tools":[{"name":"fetch","description":"Fetch","parameters":{"type":"object"}}]}"#;
const REQUEST: &str = r#"{"method":"post","url":"https://api.example.test/rpc","headers":{"ApiKey":"k"},"body":"{}","timeout_ms":999999}"#;

/// A guest whose tool calls `graphrt::http` `calls` times with [`REQUEST`] and returns the
/// last HTTP result verbatim as its tool response (so `{error}` becomes a refused call).
fn http_tool_module(calls: u32) -> Vec<u8> {
    let manifest = MANIFEST.replace('"', "\\\"");
    let request = REQUEST.replace('"', "\\\"");
    let text = format!(
        r#"(module
  (import "graphrt" "http" (func $http (param i32 i32) (result i64)))
  (memory (export "memory") 2)
  (global $bump (mut i32) (i32.const 65536))
  (data (i32.const 1024) "{manifest}")
  (data (i32.const 4096) "{request}")
  (func (export "graphrt_abi_version") (result i32) (i32.const 1))
  (func (export "graphrt_alloc") (param $len i32) (result i32) (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get $len)))
    (local.get $p))
  (func (export "graphrt_describe") (result i64)
    (i64.or (i64.shl (i64.const 1024) (i64.const 32)) (i64.const {manifest_len})))
  (func (export "graphrt_run") (param i32 i32) (result i64) (i64.const 0))
  (func (export "graphrt_tool_call") (param $ptr i32) (param $len i32) (result i64)
    (local $i i32) (local $out i64)
    (block $done (loop $again
      (br_if $done (i32.ge_u (local.get $i) (i32.const {calls})))
      (local.set $out (call $http (i32.const 4096) (i32.const {request_len})))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br $again)))
    (local.get $out))
)"#,
        manifest_len = MANIFEST.len(),
        request_len = REQUEST.len(),
    );
    wat::parse_str(text).expect("valid wat")
}

fn load(module: &[u8], egress: Option<Arc<RecordingEgress>>, limits: WasmLimits) -> Result<WasmPlugin, WasmError> {
    let mut host = WasmHost::default().with_env(BTreeMap::from([("API_URL".to_owned(), "https://x".to_owned())]));
    if let Some(egress) = egress {
        host = host.with_egress(egress);
    }
    WasmPlugin::load_with_host(module, limits, host)
}

#[tokio::test]
async fn http_requests_are_validated_capped_and_answered_in_guest_memory() {
    let egress = RecordingEgress::new(Err(HttpError::new("egress_denied", "host `x` is not allowed")));
    let plugin = load(&http_tool_module(1), Some(egress.clone()), WasmLimits::default()).expect("load");
    assert!(plugin.uses_network());
    let tools = plugin.tools().expect("tools");
    assert_eq!(tools.list().await.unwrap()[0].name, "fetch");

    // The guest hands the host's `{error}` back, so the round trip through guest memory shows up here.
    let error = tools.call("fetch", serde_json::json!({}), ToolCallContext::empty()).await.unwrap_err();
    assert!(error.message.contains("egress_denied: host `x` is not allowed"), "{error}");
    let seen = egress.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].url, "https://api.example.test/rpc");
    assert_eq!(seen[0].headers, vec![("apikey".to_owned(), "k".to_owned())]);
    assert_eq!(seen[0].timeout.as_millis(), u128::from(WasmLimits::default().http_timeout_ms), "capped");
}

#[tokio::test]
async fn a_successful_response_reaches_the_guest() {
    let reply = HttpResponse { status: 200, headers: Vec::new(), body: "[]".into() };
    let plugin = load(&http_tool_module(1), Some(RecordingEgress::new(Ok(reply))), WasmLimits::default()).unwrap();
    // `{status, headers, body}` read back as a tool response has no `error`: an empty, successful result.
    let output = plugin.tools().unwrap().call("fetch", serde_json::json!({}), ToolCallContext::empty()).await.unwrap();
    assert!(!output.is_error);
}

#[tokio::test]
async fn requests_per_call_are_capped() {
    let egress = RecordingEgress::new(Ok(HttpResponse { status: 200, headers: Vec::new(), body: String::new() }));
    let limits = WasmLimits { max_http_requests: 2, ..WasmLimits::default() };
    let plugin = load(&http_tool_module(3), Some(egress.clone()), limits).unwrap();
    let error =
        plugin.tools().unwrap().call("fetch", serde_json::json!({}), ToolCallContext::empty()).await.unwrap_err();
    assert!(error.message.contains("more than 2 HTTP requests"), "{error}");
    assert_eq!(egress.seen.lock().unwrap().len(), 2, "the third request never reached the egress");
}

#[test]
fn http_import_needs_network_access() {
    let error = load(&http_tool_module(1), None, WasmLimits::default()).unwrap_err();
    assert!(matches!(&error, WasmError::InvalidModule(m) if m.contains("no network access")), "{error}");
}

#[tokio::test]
async fn tool_context_needs_trust_and_never_leaks_back() {
    let egress = RecordingEgress::new(Err(HttpError::new("http_error", "401 for token eyJ.user.sig")));
    let mut context = ToolCallContext::default();
    context.secrets.insert("user_jwt".into(), "eyJ.user.sig".into());

    let untrusted = load(&http_tool_module(1), Some(egress.clone()), WasmLimits::default()).unwrap();
    let tools = untrusted.tools().unwrap();
    assert!(!tools.accepts_secrets());
    let refused = tools.call("fetch", serde_json::json!({}), &context).await.unwrap_err();
    assert!(refused.message.contains("may not receive a tool call context"), "{refused}");
    assert!(egress.seen.lock().unwrap().is_empty());

    let trusted = load(&http_tool_module(1), Some(egress), WasmLimits::default().with_expose_secrets(true)).unwrap();
    let tools = trusted.tools().unwrap();
    assert!(tools.accepts_secrets());
    let error = tools.call("fetch", serde_json::json!({}), &context).await.unwrap_err();
    assert!(error.message.contains("401 for token ******") && !error.message.contains("eyJ.user.sig"), "{error}");
    let unknown = tools.call("other", serde_json::json!({}), ToolCallContext::empty()).await.unwrap_err();
    assert!(unknown.message.contains("has no tool `other`"), "{unknown}");
}

#[test]
fn tools_need_the_tool_call_export() {
    let manifest = MANIFEST.replace('"', "\\\"");
    let module = wat::parse_str(format!(
        r#"(module
  (memory (export "memory") 1)
  (data (i32.const 1024) "{manifest}")
  (func (export "graphrt_abi_version") (result i32) (i32.const 1))
  (func (export "graphrt_alloc") (param i32) (result i32) (i32.const 32768))
  (func (export "graphrt_describe") (result i64)
    (i64.or (i64.shl (i64.const 1024) (i64.const 32)) (i64.const {len})))
  (func (export "graphrt_run") (param i32 i32) (result i64) (i64.const 0))
)"#,
        len = MANIFEST.len()
    ))
    .unwrap();
    let error = WasmPlugin::load(&module, WasmLimits::default()).unwrap_err();
    assert!(matches!(&error, WasmError::MissingExport(m) if m.contains("graphrt_tool_call")), "{error}");
}
