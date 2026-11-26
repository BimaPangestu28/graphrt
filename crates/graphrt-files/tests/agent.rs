//! The real `agent` node, a scripted model and the `files` tools on a real
//! directory: what a deployed agent does, end to end.

#![allow(clippy::unwrap_used)]

mod support;

use std::sync::Arc;

use graphrt_core::dsl::NodeDoc;
use graphrt_core::node::{NodeOutcome, Services};
use graphrt_core::tool::{ToolRegistry, ToolRuntime};
use graphrt_core::value::Value;
use graphrt_core::{Node, NodeContext, VariablePool};
use graphrt_files::{FileTools, PROVIDER, Workspace, WorkspaceLimits, WorkspaceResolver};
use graphrt_llm::mock::MockModel;
use graphrt_llm::nodes::agent::AgentNode;
use graphrt_llm::{ChatModel, ModelRegistry};
use serde_json::json;
use support::{TempDir, path};

fn agent(extra: serde_json::Value) -> AgentNode {
    let mut config = json!({
        "model": {"provider": "mock", "name": "scripted"},
        "tools": ["files/*"],
    });
    if let (Some(config), serde_json::Value::Object(extra)) = (config.as_object_mut(), extra) {
        config.extend(extra);
    }
    AgentNode::from_doc(&NodeDoc::new("agent", "agent", config)).unwrap()
}

fn context(model: &Arc<MockModel>, tools: FileTools, query: &str) -> NodeContext {
    let mut registry = ToolRegistry::new();
    registry.add(PROVIDER, Arc::new(tools) as Arc<dyn ToolRuntime>);
    let models = ModelRegistry::new().with("mock", Arc::clone(model) as Arc<dyn ChatModel>);
    let mut pool = VariablePool::new();
    pool.insert("sys", "query", Value::from(query));
    pool.insert("start", "user", Value::from("user-42"));
    NodeContext::for_test("agent", pool).with_services(Arc::new(Services::new().with(models).with(registry)))
}

fn tool_messages(model: &MockModel) -> Vec<String> {
    let requests = model.requests();
    let last = requests.last().unwrap();
    last.messages.iter().filter(|m| m.role == graphrt_llm::Role::Tool).map(graphrt_llm::Message::text).collect()
}

#[tokio::test]
async fn an_agent_reads_a_document_and_writes_a_summary() {
    let dir = TempDir::new("agent-e2e");
    let workspace = Workspace::dir(dir.path().join("ws"), WorkspaceLimits::default()).unwrap();
    // The user dropped a PDF into the workspace.
    std::fs::write(dir.path().join("ws/invoice.pdf"), support::pdf("Invoice 42 total 120 EUR")).unwrap();

    let model = Arc::new(MockModel::new(vec![
        MockModel::tool_calls(&[("c1", "list", "{}")]),
        MockModel::tool_calls(&[("c2", "read", r#"{"path":"invoice.pdf"}"#)]),
        MockModel::tool_calls(&[(
            "c3",
            "write",
            r##"{"path":"out/summary.md","content":"# Invoice 42\n\nTotal: 120 EUR\n"}"##,
        )]),
        MockModel::text("I wrote out/summary.md."),
    ]));
    let ctx = context(&model, FileTools::new(workspace.clone()), "Summarise invoice.pdf");
    let NodeOutcome::Completed(out) = agent(json!({})).run(ctx).await.unwrap() else { panic!("paused") };

    assert_eq!(out.outputs["text"], Value::from("I wrote out/summary.md."));
    let seen = tool_messages(&model);
    assert!(seen[0].starts_with("invoice.pdf  ("), "{}", seen[0]);
    assert!(seen[1].contains("Invoice 42 total 120 EUR"), "{}", seen[1]);
    assert_eq!(seen[2], "Wrote out/summary.md (29 B, text/markdown).");

    // The file is on disk, and the node hands it to the host as a file value...
    let summary = std::fs::read_to_string(dir.path().join("ws/out/summary.md")).unwrap();
    assert_eq!(summary, "# Invoice 42\n\nTotal: 120 EUR\n");
    let files = out.outputs["files"].as_array().unwrap().to_vec();
    let file = files[0].as_file().unwrap();
    assert_eq!((file.name.as_str(), file.mime_type.as_deref()), ("summary.md", Some("text/markdown")));
    // ...which any other node reads back through the resolver.
    use graphrt_core::file::FileResolver;
    let read = WorkspaceResolver::new(workspace).read(file, 1_000).await.unwrap();
    assert_eq!(read.bytes, summary.as_bytes());
}

#[tokio::test]
async fn a_model_that_tries_to_leave_the_workspace_gets_errors_and_nothing_else() {
    let dir = TempDir::new("agent-escape");
    std::fs::write(dir.path().join("secret.env"), b"API_KEY=sk-live-123").unwrap();
    let workspace = Workspace::dir(dir.path().join("ws"), WorkspaceLimits::default()).unwrap();
    let model = Arc::new(MockModel::new(vec![
        MockModel::tool_calls(&[
            ("c1", "read", r#"{"path":"../secret.env"}"#),
            ("c2", "read", r#"{"path":"/etc/passwd"}"#),
            ("c3", "write", r#"{"path":"../planted.txt","content":"x"}"#),
            ("c4", "search", r#"{"query":"API_KEY","path":".."}"#),
        ]),
        MockModel::text("I could not."),
    ]));
    let ctx = context(&model, FileTools::new(workspace), "read the secrets");
    let NodeOutcome::Completed(out) = agent(json!({})).run(ctx).await.unwrap() else { panic!("paused") };

    let seen = tool_messages(&model);
    assert_eq!(seen.len(), 4);
    for result in &seen {
        assert!(result.starts_with("invalid path"), "{result}");
        assert!(!result.contains("sk-live-123"));
    }
    assert!(
        out.outputs["tool_calls"]
            .as_array()
            .unwrap()
            .iter()
            .all(|call| { call.get_path(&["is_error"]) == Some(Value::Bool(true)) })
    );
    assert_eq!(out.outputs["files"], Value::array([]));
    assert!(!dir.path().join("planted.txt").exists());
}

#[tokio::test]
async fn the_scope_comes_from_the_workflow_not_from_the_model() {
    let workspace = Workspace::in_memory(WorkspaceLimits::default());
    workspace.scoped("user-7").write(&path("diary.md"), b"user 7 only".to_vec(), false).await.unwrap();

    let model = Arc::new(MockModel::new(vec![
        // The model asks for another user's file and tries to pass a scope itself.
        MockModel::tool_calls(&[
            ("c1", "read", r#"{"path":"diary.md"}"#),
            ("c2", "read", r#"{"path":"diary.md","scope":"user-7"}"#),
            ("c3", "write", r#"{"path":"mine.md","content":"user 42"}"#),
        ]),
        MockModel::text("done"),
    ]));
    let tools = FileTools::new(workspace.clone()).scoped_by_context();
    let ctx = context(&model, tools, "read my diary");
    let node = agent(json!({"tool_context": {"files": {"scope": "{{#start.user#}}"}}}));
    let NodeOutcome::Completed(_) = node.run(ctx).await.unwrap() else { panic!("paused") };

    let seen = tool_messages(&model);
    assert!(seen[0].contains("does not exist"), "{}", seen[0]);
    assert!(seen[1].starts_with("invalid arguments"), "{}", seen[1]);
    assert!(!seen.iter().any(|result| result.contains("user 7 only")));
    // The write landed in user 42's scope.
    assert_eq!(workspace.scoped("user-42").read(&path("mine.md"), 0, 100).await.unwrap(), b"user 42");
    assert!(workspace.scoped("user-7").stat(&path("mine.md")).await.unwrap().is_none());
    // The scope never reaches the model.
    let sent = serde_json::to_string(&model.requests()).unwrap();
    assert!(!sent.contains("user-42"), "{sent}");
}

#[tokio::test]
async fn a_scoped_workspace_fails_closed_without_tool_context() {
    let workspace = Workspace::in_memory(WorkspaceLimits::default());
    let model = Arc::new(MockModel::new(vec![
        MockModel::tool_calls(&[("c1", "write", r#"{"path":"a.md","content":"x"}"#)]),
        MockModel::text("done"),
    ]));
    let ctx = context(&model, FileTools::new(workspace.clone()).scoped_by_context(), "write");
    let NodeOutcome::Completed(_) = agent(json!({})).run(ctx).await.unwrap() else { panic!("paused") };
    assert!(tool_messages(&model)[0].contains("this workspace is per user"));
    assert!(workspace.list(None, 10).await.unwrap().entries.is_empty());
    assert_eq!(workspace.recount().await.unwrap().files, 0);
}
