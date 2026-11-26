//! `runtime: mcp` manifests (MCP server presets) and plugin templates.

#![cfg(feature = "mcp-presets")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use graphrt_core::Registry;
use graphrt_core::tool::ToolRegistry;
use graphrt_plugin::{
    LoadedManifest, McpApprovalDefault, McpAuth, McpTransportKind, PluginHost, PluginManifest, Runtime,
};
use serde_json::{Value, json};

fn upwork() -> Value {
    json!({
        "name": "upwork",
        "version": "1.0.0",
        "description": "Upwork MCP server",
        "runtime": "mcp",
        "permissions": {"network": true},
        "mcp": {
            "url": "https://mcp.upwork.com/mcp",
            "transport": "streamable-http",
            "auth": "oauth",
            "approval": {"default": "ask", "read_only": ["find_*", "get_*", "list_*", "search_*"], "ask_first": ["get_secret"]},
            "guide": "guide.md"
        },
        "templates": [{"id": "morning-jobs", "title": "Morning jobs", "description": "Find jobs", "prompt": "Every morning…\nthen ask me"}]
    })
}

fn parse(value: &Value) -> Result<PluginManifest, String> {
    PluginManifest::from_json(&value.to_string()).map_err(|e| e.to_string())
}

#[test]
fn parses_an_mcp_preset_with_templates() {
    let manifest = parse(&upwork()).unwrap();
    let Runtime::Mcp(runtime) = &manifest.runtime else { panic!("mcp runtime") };
    assert_eq!(manifest.runtime.name(), "mcp");
    assert_eq!(runtime.mcp.url, "https://mcp.upwork.com/mcp");
    assert_eq!(runtime.mcp.transport, McpTransportKind::StreamableHttp);
    assert_eq!(runtime.mcp.auth, McpAuth::Oauth);
    assert_eq!(runtime.mcp.approval.default, McpApprovalDefault::Ask);
    assert!(runtime.mcp.approval.is_declared_read_only("find_jobs"));
    assert!(!runtime.mcp.approval.is_declared_read_only("get_secret"));
    assert!(!runtime.mcp.approval.is_declared_read_only("send_message"));
    assert_eq!(runtime.mcp.guide.as_deref(), Some("guide.md"));
    assert_eq!(manifest.templates[0].id, "morning-jobs");

    let round_trip = serde_json::to_value(&manifest).unwrap();
    assert_eq!(round_trip["runtime"], "mcp");
    assert_eq!(parse(&round_trip).unwrap(), manifest, "serializes to the manifest form");
}

#[test]
fn defaults_are_safe() {
    let manifest = parse(&json!({"name": "x", "runtime": "mcp", "mcp": {"url": "https://x/mcp"}})).unwrap();
    let Runtime::Mcp(runtime) = &manifest.runtime else { panic!("mcp runtime") };
    assert_eq!(runtime.mcp.auth, McpAuth::None);
    assert_eq!(runtime.mcp.approval.default, McpApprovalDefault::Ask, "unlisted tools ask first");
    assert!(manifest.templates.is_empty());
}

#[test]
fn rejects_invalid_mcp_presets() {
    let cases: Vec<(Value, &str)> = vec![
        (json!({"name": "x", "runtime": "mcp"}), "requires `mcp`"),
        (json!({"name": "x", "runtime": "mcp", "mcp": {"url": "http://mcp.example.com/mcp"}}), "https://"),
        (json!({"name": "x", "runtime": "mcp", "mcp": {"url": "ftp://x"}}), "https://"),
        (json!({"name": "x", "runtime": "mcp", "mcp": {"url": "https://x", "auth": "bearer"}}), "bearer_token_env"),
        (
            json!({"name": "x", "runtime": "mcp", "mcp": {"url": "https://x", "auth": "bearer", "bearer_token_env": "T"}}),
            "permissions.env",
        ),
        (json!({"name": "x", "runtime": "mcp", "mcp": {"url": "https://x", "bearer_token_env": "T"}}), "only applies"),
        (json!({"name": "x", "runtime": "mcp", "mcp": {"url": "https://x", "guide": "../g.md"}}), "mcp.guide"),
        (json!({"name": "x", "runtime": "mcp", "mcp": {"url": "https://x", "guide": "g.txt"}}), "mcp.guide"),
        (
            json!({"name": "x", "runtime": "mcp", "mcp": {"url": "https://x", "approval": {"read_only": ["*"]}}}),
            "pattern",
        ),
        (
            json!({"name": "x", "runtime": "mcp", "mcp": {"url": "https://x", "approval": {"default": "never"}}}),
            "unknown variant",
        ),
        (json!({"name": "x", "runtime": "mcp", "mcp": {"url": "https://x", "transport": "sse"}}), "unknown variant"),
        (json!({"name": "x", "runtime": "mcp", "mcp": {"url": "https://x", "typo": 1}}), "unknown field"),
        (
            json!({"name": "x", "runtime": "mcp", "mcp": {"url": "https://x"}, "tools": [{"name": "t", "description": "", "parameters": {}}]}),
            "must be empty",
        ),
        (
            json!({"name": "x", "runtime": "mcp", "mcp": {"url": "https://x"}, "command": "node"}),
            "not valid for runtime `mcp`",
        ),
        (
            json!({"name": "x", "runtime": "process", "command": "node", "mcp": {"url": "https://x"}}),
            "not valid for runtime `process`",
        ),
    ];
    for (value, expected) in cases {
        let error = parse(&value).expect_err(&value.to_string());
        assert!(error.contains(expected), "{value}: {error}");
    }
    let loopback = json!({"name": "x", "runtime": "mcp", "mcp": {"url": "http://127.0.0.1:3000/mcp"}});
    assert!(parse(&loopback).is_ok(), "http to this machine is allowed");
}

#[test]
fn rejects_invalid_templates() {
    let template = |id: &str, prompt: &str| json!({"id": id, "title": "T", "description": "D", "prompt": prompt});
    let with =
        |templates: Value| json!({"name": "acme", "runtime": "process", "command": "node", "templates": templates});
    assert!(parse(&with(json!([template("ok-1", "do it")]))).is_ok(), "any runtime may carry templates");
    for (templates, expected) in [
        (json!([template("Bad Id", "x")]), "template id"),
        (json!([template("a", "x"), template("a", "y")]), "duplicate template id"),
        (json!([template("a", "")]), "`prompt` must be"),
        (json!([template("a", &"x".repeat(4_001))]), "`prompt` must be"),
        (json!([template("a", "bell\u{7}")]), "control character"),
        (json!([{"id": "a", "title": "T", "description": "D", "prompt": "p", "extra": 1}]), "unknown field"),
    ] {
        let error = parse(&with(templates.clone())).expect_err(&templates.to_string());
        assert!(error.contains(expected), "{templates}: {error}");
    }
}

#[test]
fn the_plugin_host_loads_an_mcp_preset_and_registers_nothing() {
    let manifest = parse(&upwork()).unwrap();
    let mut host = PluginHost::new();
    let info = host.add(LoadedManifest { manifest, dir: std::env::temp_dir() }).unwrap();
    assert_eq!(info.name, "upwork");
    let mut registry = Registry::new();
    let mut tools = ToolRegistry::new();
    host.register(&mut registry, &mut tools).expect("no loader needed for an MCP preset");
    assert!(tools.get("upwork").is_none());
}
