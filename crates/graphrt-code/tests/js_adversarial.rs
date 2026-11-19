//! Adversarial tests for the embedded JavaScript sandbox: untrusted workflow code
//! must not hang, crash or exhaust the host, and must not reach host capabilities.

#![cfg(feature = "js")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use graphrt_code::{CodeLimits, CodeNode, KIND};
use graphrt_core::dsl::NodeDoc;
use graphrt_core::node::{Node, NodeContext, NodeError, NodeOutcome, NodeOutput, Services};
use graphrt_core::{Value, VariablePool};

/// Generous upper bound for "the sandbox stopped the code promptly".
const PROMPT: Duration = Duration::from_secs(10);

fn pool() -> VariablePool {
    let mut pool = VariablePool::new();
    pool.insert("start", "text", Value::from("hello"));
    pool.insert("env", "KEY", Value::Secret(Arc::from("sk-live-secret")));
    pool
}

async fn run_with(config: serde_json::Value, host: Option<CodeLimits>) -> Result<NodeOutput, NodeError> {
    let node =
        CodeNode::from_doc(&NodeDoc::new("c", KIND, config)).map_err(|e| NodeError::fatal("config", e.to_string()))?;
    let mut ctx = NodeContext::for_test("c", pool());
    if let Some(host) = host {
        ctx = ctx.with_services(Arc::new(Services::new().with(host)));
    }
    match node.run(ctx).await? {
        NodeOutcome::Completed(output) => Ok(output),
        NodeOutcome::Paused(_) => Err(NodeError::fatal("test", "unexpected pause")),
    }
}

async fn run(config: serde_json::Value) -> Result<NodeOutput, NodeError> {
    run_with(config, None).await
}

/// Run `code` with `limits` and return the error; fails the test if the code
/// succeeded or took longer than [`PROMPT`].
async fn expect_failure(code: &str, limits: serde_json::Value) -> NodeError {
    let started = Instant::now();
    let result = run(serde_json::json!({ "code": code, "limits": limits })).await;
    assert!(started.elapsed() < PROMPT, "took {:?}", started.elapsed());
    match result {
        Ok(output) => panic!("expected a failure, got {output:?}"),
        Err(error) => {
            assert!(!error.retryable, "sandbox limit errors must not be retried: {error:?}");
            error
        }
    }
}

#[tokio::test]
async fn infinite_loop_in_a_promise_job_is_interrupted() {
    let error =
        expect_failure("async function main() { await null; for (;;) {} }", serde_json::json!({ "time_ms": 200 }))
            .await;
    assert_eq!(error.kind, "timeout", "{error:?}");
}

#[tokio::test]
async fn endless_microtask_chain_is_interrupted() {
    let error = expect_failure(
        "function spin() { return Promise.resolve().then(spin); } function main() { return spin(); }",
        serde_json::json!({ "time_ms": 200 }),
    )
    .await;
    assert!(matches!(error.kind.as_str(), "timeout" | "out_of_memory"), "{error:?}");
}

#[tokio::test]
async fn catastrophic_regex_backtracking_is_interrupted() {
    let error = expect_failure(
        "function main() { return { m: /^(a+)+$/.test('a'.repeat(64) + 'b') }; }",
        serde_json::json!({ "time_ms": 200 }),
    )
    .await;
    assert_eq!(error.kind, "timeout", "{error:?}");
}

#[tokio::test]
async fn catching_the_interrupt_does_not_extend_the_budget() {
    let error = expect_failure(
        "function main() { for (;;) { try { for (;;) {} } catch (e) {} } }",
        serde_json::json!({ "time_ms": 200 }),
    )
    .await;
    assert_eq!(error.kind, "timeout", "{error:?}");
}

#[tokio::test]
async fn string_doubling_hits_the_memory_limit() {
    let error = expect_failure(
        "function main() { let s = 'x'; for (;;) { s = s + s; } }",
        serde_json::json!({ "memory_bytes": 16 << 20 }),
    )
    .await;
    assert!(matches!(error.kind.as_str(), "out_of_memory" | "code_error"), "{error:?}");
}

#[tokio::test]
async fn object_graph_bomb_hits_the_memory_limit() {
    let error = expect_failure(
        "function main() { const all = []; for (let i = 0;; i++) { all.push({ i, s: 'v' + i, a: [i, i] }); } }",
        serde_json::json!({ "memory_bytes": 16 << 20 }),
    )
    .await;
    assert!(matches!(error.kind.as_str(), "out_of_memory" | "code_error"), "{error:?}");
}

#[tokio::test]
async fn huge_typed_array_is_refused() {
    let error = expect_failure(
        "function main() { const b = new Uint8Array(1024 * 1024 * 1024); return { n: b.length }; }",
        serde_json::json!({ "memory_bytes": 16 << 20 }),
    )
    .await;
    assert!(matches!(error.kind.as_str(), "out_of_memory" | "code_error"), "{error:?}");
}

#[tokio::test]
async fn deep_recursion_is_a_catchable_error() {
    let error = expect_failure(
        "function f(n) { return f(n + 1) + 1; } function main() { return { v: f(0) }; }",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(error.kind, "code_error", "{error:?}");
}

/// QuickJS treats a zero memory or stack limit as "unlimited". A workflow must not be
/// able to switch the limits off by asking for zero.
#[tokio::test]
async fn zero_stack_limit_does_not_disable_the_stack_check() {
    let error = expect_failure(
        "function f(n) { return f(n + 1) + 1; } function main() { return { v: f(0) }; }",
        serde_json::json!({ "stack_bytes": 0 }),
    )
    .await;
    assert_eq!(error.kind, "code_error", "{error:?}");
}

#[tokio::test]
async fn zero_memory_limit_does_not_disable_the_memory_check() {
    let error = expect_failure(
        "function main() { const a = []; for (;;) { a.push('x'.repeat(1 << 20)); } }",
        serde_json::json!({ "memory_bytes": 0, "time_ms": 5000 }),
    )
    .await;
    assert!(matches!(error.kind.as_str(), "out_of_memory" | "code_error"), "{error:?}");
}

/// An operator ceiling larger than the default thread stack must not crash the host.
#[tokio::test]
async fn large_host_stack_limit_does_not_overflow_the_native_stack() {
    let host = CodeLimits { stack_bytes: 64 << 20, ..CodeLimits::default() };
    let started = Instant::now();
    let error = run_with(
        serde_json::json!({ "code": "function f(n) { return f(n + 1) + 1; } function main() { return { v: f(0) }; }" }),
        Some(host),
    )
    .await
    .unwrap_err();
    assert!(started.elapsed() < PROMPT);
    assert_eq!(error.kind, "code_error", "{error:?}");
}

#[tokio::test]
async fn huge_time_limit_from_the_host_does_not_panic() {
    let host = CodeLimits { time_ms: u64::MAX, ..CodeLimits::default() };
    let output =
        run_with(serde_json::json!({ "code": "function main() { return { ok: true }; }" }), Some(host)).await.unwrap();
    assert_eq!(output.outputs.get("ok"), Some(&Value::Bool(true)));
}

#[tokio::test]
async fn huge_error_messages_are_truncated() {
    let error =
        expect_failure("function main() { throw new Error('e'.repeat(4 * 1024 * 1024)); }", serde_json::json!({}))
            .await;
    assert_eq!(error.kind, "code_error");
    assert!(error.message.len() <= 16 * 1024, "error message is {} bytes", error.message.len());
}

#[tokio::test]
async fn huge_output_is_rejected_before_conversion() {
    let error = expect_failure(
        "function main() { return { s: 'x'.repeat(8 * 1024 * 1024) }; }",
        serde_json::json!({ "max_output_bytes": 1 << 20 }),
    )
    .await;
    assert_eq!(error.kind, "invalid_output", "{error:?}");
}

#[tokio::test]
async fn deeply_nested_output_is_rejected() {
    let error = expect_failure(
        "function main() { let v = {}; for (let i = 0; i < 100000; i++) { v = { v }; } return v; }",
        serde_json::json!({}),
    )
    .await;
    assert!(matches!(error.kind.as_str(), "invalid_output" | "code_error"), "{error:?}");
}

#[tokio::test]
async fn host_capabilities_are_absent() {
    let probes = [
        "typeof require",
        "typeof process",
        "typeof fetch",
        "typeof XMLHttpRequest",
        "typeof WebSocket",
        "typeof setTimeout",
        "typeof setInterval",
        "typeof std",
        "typeof os",
        "typeof Deno",
        "typeof Bun",
        "typeof importScripts",
        "typeof print",
        "typeof console",
        "typeof scriptArgs",
    ];
    let body = probes.iter().enumerate().map(|(i, p)| format!("p{i}: {p}")).collect::<Vec<_>>().join(", ");
    let output =
        run(serde_json::json!({ "code": format!("function main() {{ return {{ {body} }}; }}") })).await.unwrap();
    for (i, probe) in probes.iter().enumerate() {
        assert_eq!(output.outputs.get(&format!("p{i}")), Some(&Value::from("undefined")), "{probe}");
    }
}

#[tokio::test]
async fn module_imports_are_unavailable() {
    let error = expect_failure(
        "async function main() { const m = await import('os'); return { k: Object.keys(m) }; }",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(error.kind, "code_error", "{error:?}");
}

#[tokio::test]
async fn the_function_constructor_reaches_no_host_objects() {
    let output = run(serde_json::json!({
        "code": "function main() { const g = Function('return this')(); \
                 return { names: Object.getOwnPropertyNames(g).filter(n => n.startsWith('__') || /^[a-z]/.test(n)).sort() }; }",
    }))
    .await
    .unwrap();
    let Some(names) = output.outputs.get("names").and_then(Value::as_array) else { panic!("{output:?}") };
    let names: Vec<String> = names.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect();
    // Only standard ECMAScript globals (plus the pure helpers atob/btoa/performance.now)
    // and the driver's own exchange slots.
    let allowed = [
        "__graphrt_code",
        "__graphrt_done",
        "__graphrt_err",
        "__graphrt_input",
        "__graphrt_out",
        "atob",
        "btoa",
        "decodeURI",
        "decodeURIComponent",
        "encodeURI",
        "encodeURIComponent",
        "escape",
        "eval",
        "globalThis",
        "isFinite",
        "isNaN",
        "main",
        "parseFloat",
        "parseInt",
        "performance",
        "queueMicrotask",
        "undefined",
        "unescape",
    ];
    for name in &names {
        assert!(allowed.contains(&name.as_str()), "unexpected global `{name}` in {names:?}");
    }
}

#[tokio::test]
async fn prototype_pollution_does_not_leak_out_of_the_run() {
    // Pollute every builtin prototype, return a `__proto__` key, then check a second run is clean.
    let output = run(serde_json::json!({
        "code": "function main() { Object.prototype.polluted = 'yes'; Array.prototype.polluted = 'yes'; \
                 JSON.parse = () => ({ hacked: true }); \
                 return JSON.parse('{\"__proto__\": {\"admin\": true}, \"x\": 1}'); }",
    }))
    .await
    .unwrap();
    assert_eq!(output.outputs.get("hacked"), Some(&Value::Bool(true)));
    assert!(output.outputs.get("polluted").is_none());

    let output = run(serde_json::json!({
        "code": "function main() { return { p: typeof ({}).polluted, a: typeof [].polluted, j: JSON.parse('{\"k\":1}').k }; }",
    }))
    .await
    .unwrap();
    assert_eq!(output.outputs.get("p"), Some(&Value::from("undefined")));
    assert_eq!(output.outputs.get("a"), Some(&Value::from("undefined")));
    assert_eq!(output.outputs.get("j"), Some(&Value::Int(1)));
}

#[tokio::test]
async fn proto_keys_in_output_stay_plain_data() {
    let output = run(serde_json::json!({
        "code": "function main() { return JSON.parse('{\"__proto__\": {\"admin\": true}, \"constructor\": 1}'); }",
    }))
    .await
    .unwrap();
    assert_eq!(output.outputs.get("__proto__"), Some(&Value::from_json(serde_json::json!({"admin": true}))));
    assert_eq!(output.outputs.get("constructor"), Some(&Value::Int(1)));
}

#[tokio::test]
async fn tampering_with_driver_slots_does_not_replace_the_result() {
    // The driver writes the result after `main` settles, so a script that pre-fills the
    // exchange slots cannot smuggle a different (or oversized) result past the checks.
    let output = run(serde_json::json!({
        "code": "globalThis.__graphrt_out = 'x'.repeat(4 << 20); __graphrt_done = true; \
                 function main() { __graphrt_out = JSON.stringify({ s: 'y'.repeat(4 << 20) }); return { ok: 1 }; }",
        "limits": { "max_output_bytes": 1 << 20 },
    }))
    .await
    .unwrap();
    assert_eq!(output.outputs.len(), 1);
    assert_eq!(output.outputs.get("ok"), Some(&Value::Int(1)));
}

#[tokio::test]
async fn host_env_is_not_visible() {
    let output = run(serde_json::json!({
        "code": "function main() { const g = globalThis; return { e: typeof g.env, p: typeof g.process, h: JSON.stringify(Object.keys(g)).includes('HOME') }; }",
    }))
    .await
    .unwrap();
    assert_eq!(output.outputs.get("e"), Some(&Value::from("undefined")));
    assert_eq!(output.outputs.get("p"), Some(&Value::from("undefined")));
    assert_eq!(output.outputs.get("h"), Some(&Value::Bool(false)));
}

#[tokio::test]
async fn runs_do_not_share_global_state() {
    let first = run(serde_json::json!({ "code": "globalThis.leak = 'from-run-1'; function main() { return {}; }" }))
        .await
        .unwrap();
    assert!(first.outputs.is_empty());
    let second = run(serde_json::json!({ "code": "function main() { return { leak: typeof globalThis.leak }; }" }))
        .await
        .unwrap();
    assert_eq!(second.outputs.get("leak"), Some(&Value::from("undefined")));
}

#[tokio::test]
async fn many_concurrent_runs_complete() {
    let tasks: Vec<_> = (0..32)
        .map(|i| {
            tokio::spawn(run(serde_json::json!({
                "code": format!("function main() {{ return {{ v: {i} }}; }}"),
            })))
        })
        .collect();
    for (i, task) in tasks.into_iter().enumerate() {
        let output = task.await.unwrap().unwrap();
        assert_eq!(output.outputs.get("v"), Some(&Value::Int(i64::try_from(i).unwrap())));
    }
}
