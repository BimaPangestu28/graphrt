//! End-to-end tests against a real rmcp server: in-process over a duplex
//! stream, and as a child process over stdio (this test binary re-launches
//! itself as the server).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use graphrt_core::dsl::NodeDoc;
use graphrt_core::node::{Node, NodeContext, NodeOutcome, Services};
use graphrt_core::pool::VariablePool;
use graphrt_core::tool::{ToolCallContext, ToolRuntime};
use graphrt_core::value::Value;
use graphrt_mcp::{McpServerConfig, McpToolRuntime, McpTransport, StdioConfig, connect_all};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};

const HELPER_ENV: &str = "GRAPHRT_MCP_TEST_SERVER";
const SECRET_ENV: &str = "GRAPHRT_MCP_TEST_HOST_SECRET";

struct TestServer;

fn schema(required: &[&str]) -> Arc<serde_json::Map<String, serde_json::Value>> {
    let value = serde_json::json!({"type": "object", "required": required});
    match value {
        serde_json::Value::Object(map) => Arc::new(map),
        _ => Arc::new(serde_json::Map::new()),
    }
}

impl ServerHandler for TestServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(vec![
            Tool::new("echo", "Echo text", schema(&["text"])),
            Tool::new("add", "Add two numbers", schema(&["a", "b"])),
            Tool::new("fail", "Always fails", schema(&[])),
            Tool::new("slow", "Sleeps", schema(&[])),
            Tool::new("env_probe", "Reports whether the host secret is visible", schema(&[])),
        ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = request.arguments.unwrap_or_default();
        let result = match request.name.as_ref() {
            "echo" => CallToolResult::success(vec![ContentBlock::text(
                args.get("text").and_then(|t| t.as_str()).unwrap_or(""),
            )]),
            "add" => {
                let a = args.get("a").and_then(serde_json::Value::as_i64).unwrap_or(0);
                let b = args.get("b").and_then(serde_json::Value::as_i64).unwrap_or(0);
                let mut r = CallToolResult::success(vec![ContentBlock::text((a + b).to_string())]);
                r.structured_content = Some(serde_json::json!({"sum": a + b}));
                r
            }
            "fail" => CallToolResult::error(vec![ContentBlock::text("it broke")]),
            "slow" => {
                tokio::time::sleep(Duration::from_secs(5)).await;
                CallToolResult::success(vec![ContentBlock::text("late")])
            }
            "env_probe" => {
                let visible = std::env::var(SECRET_ENV).is_ok();
                let explicit = std::env::var("EXPLICIT").unwrap_or_default();
                CallToolResult::success(vec![ContentBlock::text(format!("secret={visible} explicit={explicit}"))])
            }
            other => return Err(ErrorData::invalid_params(format!("unknown tool {other}"), None)),
        };
        Ok(result.into())
    }
}

fn config(name: &str, timeout_ms: u64) -> McpServerConfig {
    McpServerConfig {
        name: name.to_owned(),
        transport: McpTransport::Stdio(StdioConfig {
            command: String::new(),
            args: Vec::new(),
            env: Default::default(),
            pass_env: Vec::new(),
            inherit_env: false,
            cwd: None,
            show_stderr: false,
        }),
        timeout_ms,
        allowed_tools: None,
    }
}

async fn in_process(cfg: &McpServerConfig) -> McpToolRuntime {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        if let Ok(server) = TestServer.serve(server_io).await {
            let _ = server.waiting().await;
        }
    });
    McpToolRuntime::from_transport(cfg, client_io).await.expect("handshake")
}

#[tokio::test]
async fn lists_and_calls_tools_in_process() {
    let rt = in_process(&config("local", 2_000)).await;
    let tools = rt.list().await.unwrap();
    assert_eq!(tools.len(), 5);
    assert_eq!(tools[0].name, "echo");
    assert_eq!(tools[0].parameters["required"][0], "text");

    let out = rt.call("echo", serde_json::json!({"text": "hello"}), ToolCallContext::empty()).await.unwrap();
    assert_eq!(out.text, "hello");
    assert!(!out.is_error);

    let out = rt.call("add", serde_json::json!({"a": 2, "b": 3}), ToolCallContext::empty()).await.unwrap();
    assert_eq!(out.data.get_path(&["sum"]), Some(Value::Int(5)));

    let out = rt.call("fail", serde_json::json!({}), ToolCallContext::empty()).await.unwrap();
    assert!(out.is_error);

    assert!(rt.call("missing", serde_json::json!({}), ToolCallContext::empty()).await.is_err());
    assert!(rt.call("echo", serde_json::json!("not an object"), ToolCallContext::empty()).await.is_err());
    rt.close().await;
}

#[tokio::test]
async fn enforces_timeout_and_allow_list() {
    let mut cfg = config("local", 200);
    cfg.allowed_tools = Some(vec!["echo".into(), "slow".into()]);
    let rt = in_process(&cfg).await;
    let names: Vec<String> = rt.list().await.unwrap().into_iter().map(|t| t.name).collect();
    assert_eq!(names, ["echo", "slow"]);
    let err = rt.call("add", serde_json::json!({"a": 1, "b": 1}), ToolCallContext::empty()).await.unwrap_err();
    assert!(err.message.contains("allowed"), "{err}");
    let err = rt.call("slow", serde_json::json!({}), ToolCallContext::empty()).await.unwrap_err();
    assert!(err.message.contains("timed out"), "{err}");
}

#[tokio::test]
async fn tool_node_calls_mcp_tool() {
    let mut registry = graphrt_core::tool::ToolRegistry::new();
    graphrt_mcp::register(&mut registry, in_process(&config("calc", 2_000)).await);
    assert!(registry.get("calc").is_some());

    let doc = NodeDoc::new(
        "t",
        "tool",
        serde_json::json!({
            "provider_id": "mcp:calc",
            "tool_name": "add",
            "tool_parameters": {
                "a": {"type": "variable", "value": ["start", "a"]},
                "b": {"type": "constant", "value": 40}
            }
        }),
    );
    let node = graphrt_core::tool_node::ToolNode::from_doc(&doc).unwrap();
    let mut pool = VariablePool::new();
    pool.insert("start", "a", Value::Int(2));
    let ctx = NodeContext::for_test("t", pool).with_services(Arc::new(Services::new().with(registry)));
    match node.run(ctx).await.unwrap() {
        NodeOutcome::Completed(out) => {
            assert_eq!(out.outputs["text"], Value::from("42"));
            assert_eq!(out.outputs["json"].get_path(&["sum"]), Some(Value::Int(42)));
        }
        other => panic!("unexpected {other:?}"),
    }
}

fn stdio_config(name: &str) -> McpServerConfig {
    let exe = std::env::current_exe().expect("test exe").to_string_lossy().into_owned();
    let mut cfg = config(name, 10_000);
    cfg.transport = McpTransport::Stdio(StdioConfig {
        command: exe,
        args: ["--exact", "stdio_server_helper", "--quiet", "--nocapture", "--test-threads", "1"]
            .map(String::from)
            .to_vec(),
        env: [(HELPER_ENV.to_owned(), "1".to_owned()), ("EXPLICIT".to_owned(), "yes".to_owned())].into_iter().collect(),
        pass_env: Vec::new(),
        inherit_env: false,
        cwd: None,
        show_stderr: false,
    });
    cfg
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_child_process_with_clean_environment() {
    // The host must hold a secret that the MCP server must not see. `set_var` is
    // unsafe in edition 2024 (and unsafe code is forbidden), so re-run this test
    // in a fresh process that has the secret in its environment.
    if std::env::var(SECRET_ENV).is_err() {
        let status = std::process::Command::new(std::env::current_exe().expect("exe"))
            .args(["--exact", "stdio_child_process_with_clean_environment", "--quiet", "--test-threads", "1"])
            .env(SECRET_ENV, "top-secret")
            .status()
            .expect("rerun");
        assert!(status.success(), "test with host secret set failed");
        return;
    }
    let registry = connect_all(&[stdio_config("child")]).await.expect("connect stdio server");
    let rt = registry.get("mcp:child").expect("registered").clone();
    let out = rt.call("echo", serde_json::json!({"text": "over stdio"}), ToolCallContext::empty()).await.unwrap();
    assert_eq!(out.text, "over stdio");
    let probe = rt.call("env_probe", serde_json::json!({}), ToolCallContext::empty()).await.unwrap();
    assert_eq!(probe.text, "secret=false explicit=yes");
}

#[tokio::test]
async fn missing_command_is_spawn_error() {
    let mut cfg = stdio_config("broken");
    if let McpTransport::Stdio(s) = &mut cfg.transport {
        s.command = "/nonexistent/graphrt-mcp-server".into();
    }
    let err = McpToolRuntime::connect(&cfg).await.unwrap_err();
    assert!(matches!(err, graphrt_mcp::McpError::Spawn { .. }), "{err}");
}

/// Runs only when launched by the stdio test: serves [`TestServer`] on stdin/stdout.
#[tokio::test]
async fn stdio_server_helper() {
    if std::env::var(HELPER_ENV).is_err() {
        return;
    }
    if let Ok(server) = TestServer.serve(rmcp::transport::stdio()).await {
        let _ = server.waiting().await;
    }
}
