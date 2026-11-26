//! The real model nodes with a scripted model: what a provider would receive.

#![cfg(feature = "tools")]
#![allow(clippy::unwrap_used)]

mod support;

use std::sync::Arc;

use graphrt_core::dsl::NodeDoc;
use graphrt_core::file::{ChainResolver, FileResolver, ImagePreparer};
use graphrt_core::node::{NodeOutcome, Services};
use graphrt_core::tool::{ToolRegistry, ToolRuntime};
use graphrt_core::value::Value;
use graphrt_core::{Node, NodeContext, VariablePool};
use graphrt_files::{Workspace, WorkspaceLimits, WorkspacePath, WorkspaceResolver};
use graphrt_image::{ImageLimits, ImageTools, PROVIDER, VisionPreparer, probe};
use graphrt_llm::mock::MockModel;
use graphrt_llm::nodes::agent::AgentNode;
use graphrt_llm::nodes::llm::LlmNode;
use graphrt_llm::{ChatModel, ContentPart, ModelRegistry, Role};
use serde_json::json;
use support::{SECRET, contains, jpeg_with_metadata};

fn path(raw: &str) -> WorkspacePath {
    WorkspacePath::parse(raw).unwrap()
}

fn services(model: &Arc<MockModel>, workspace: &Workspace) -> Services {
    let files: Arc<dyn FileResolver> =
        Arc::new(ChainResolver::with_data_urls().then(Arc::new(WorkspaceResolver::new(workspace.clone()))));
    let preparer: Arc<dyn ImagePreparer> = Arc::new(VisionPreparer::new(Arc::clone(&files), ImageLimits::default()));
    let mut tools = ToolRegistry::new();
    tools.add(PROVIDER, Arc::new(ImageTools::new(workspace.clone())) as Arc<dyn ToolRuntime>);
    Services::new()
        .with(ModelRegistry::new().with("mock", Arc::clone(model) as Arc<dyn ChatModel>))
        .with(files)
        .with(preparer)
        .with(tools)
}

fn image_urls(message: &graphrt_llm::Message) -> Vec<String> {
    message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Image { url, .. } => Some(url.clone()),
            _ => None,
        })
        .collect()
}

fn decoded(url: &str) -> Vec<u8> {
    assert!(url.starts_with("data:image/jpeg;base64,"), "{}", url.get(..40).unwrap_or(url));
    graphrt_core::file::decode_data_url("image", url, u64::MAX).unwrap().bytes
}

/// A photo the user put in the workspace goes to the model as a vision input:
/// read from storage, downscaled, and without its GPS position.
#[tokio::test]
async fn a_stored_photo_reaches_the_model_checked_and_without_metadata() {
    let workspace = Workspace::in_memory(WorkspaceLimits::default());
    let stored =
        workspace.write(&path("uploads/receipt.jpg"), jpeg_with_metadata(2_400, 1_800, 1), false).await.unwrap();
    let model = Arc::new(MockModel::new(vec![MockModel::text("The total is 42.")]));

    let mut pool = VariablePool::new();
    pool.insert("sys", "query", Value::from("What is the total?"));
    pool.insert("sys", "files", Value::array([Value::from(workspace.file_value(&stored))]));
    let ctx = NodeContext::for_test("llm", pool).with_services(Arc::new(services(&model, &workspace)));
    let node = LlmNode::from_doc(&NodeDoc::new(
        "llm",
        "llm",
        json!({
            "model": {"provider": "mock", "name": "vision"},
            "prompt_template": [{"role": "user", "text": "{{#sys.query#}}"}],
            "vision": {"enabled": true},
        }),
    ))
    .unwrap();
    let NodeOutcome::Completed(out) = node.run(ctx).await.unwrap() else { panic!("paused") };
    assert_eq!(out.outputs["text"], Value::from("The total is 42."));

    let requests = model.requests();
    let user = requests[0].messages.iter().find(|m| m.role == Role::User).unwrap();
    let urls = image_urls(user);
    assert_eq!(urls.len(), 1);
    let bytes = decoded(&urls[0]);
    let info = probe(&bytes).unwrap();
    assert_eq!((info.width, info.height, info.has_metadata), (1_568, 1_176, false));
    assert!(!contains(&bytes, SECRET.as_bytes()));
    // Nothing in the whole request carries the metadata or the storage reference.
    let sent = serde_json::to_string(&requests[0]).unwrap();
    assert!(!sent.contains(SECRET) && !sent.contains("workspace:"), "leaked");
}

#[tokio::test]
async fn a_spoofed_vision_input_stops_the_node_before_the_model_is_called() {
    let workspace = Workspace::in_memory(WorkspaceLimits::default());
    let svg = b"<svg xmlns='http://www.w3.org/2000/svg'><script>alert(1)</script></svg>".to_vec();
    let stored = workspace.write(&path("uploads/logo.png"), svg, false).await.unwrap();
    let mut file = workspace.file_value(&stored);
    // The sender insists it is a PNG.
    file.kind = graphrt_core::value::FileKind::Image;
    file.mime_type = Some("image/png".into());
    let model = Arc::new(MockModel::new(vec![MockModel::text("never asked")]));
    let mut pool = VariablePool::new();
    pool.insert("sys", "query", Value::from("describe"));
    pool.insert("sys", "files", Value::array([Value::from(file)]));
    let ctx = NodeContext::for_test("agent", pool).with_services(Arc::new(services(&model, &workspace)));
    let node = AgentNode::from_doc(&NodeDoc::new(
        "agent",
        "agent",
        json!({"model": {"provider": "mock", "name": "vision"}, "vision": {"enabled": true}}),
    ))
    .unwrap();
    let error = node.run(ctx).await.unwrap_err();
    assert!(error.to_string().contains("image `logo.png` was refused"), "{error}");
    assert!(error.to_string().contains("SVG"), "{error}");
    assert!(model.requests().is_empty(), "the model was called with a refused image");
}

/// An agent looks at an image with `image/view` and makes a smaller copy.
#[tokio::test]
async fn an_agent_views_and_resizes_an_image() {
    let workspace = Workspace::in_memory(WorkspaceLimits::default());
    workspace.write(&path("photo.jpg"), jpeg_with_metadata(2_400, 1_800, 1), false).await.unwrap();
    let model = Arc::new(MockModel::new(vec![
        MockModel::tool_calls(&[("c1", "view", r#"{"path":"photo.jpg"}"#)]),
        MockModel::tool_calls(&[("c2", "resize", r#"{"path":"photo.jpg","max_width":400}"#)]),
        MockModel::text("A gradient. I saved photo-400x300.jpg."),
    ]));
    let mut pool = VariablePool::new();
    pool.insert("sys", "query", Value::from("Describe photo.jpg and save a 400 px wide copy."));
    let ctx = NodeContext::for_test("agent", pool).with_services(Arc::new(services(&model, &workspace)));
    let node = AgentNode::from_doc(&NodeDoc::new(
        "agent",
        "agent",
        json!({"model": {"provider": "mock", "name": "vision"}, "tools": ["image/*"]}),
    ))
    .unwrap();
    let NodeOutcome::Completed(out) = node.run(ctx).await.unwrap() else { panic!("paused") };

    // The second request carries the viewed image in the tool result.
    let requests = model.requests();
    let tool_result = requests[1].messages.iter().find(|m| m.role == Role::Tool).unwrap();
    let urls = image_urls(tool_result);
    assert_eq!(urls.len(), 1);
    let seen = decoded(&urls[0]);
    assert_eq!(probe(&seen).unwrap().width, 1_568);
    assert!(!contains(&seen, SECRET.as_bytes()));

    // The resized copy is in the workspace and in the node's `files` output.
    let files = out.outputs["files"].as_array().unwrap().to_vec();
    let file = files[0].as_file().unwrap();
    assert_eq!((file.name.as_str(), file.mime_type.as_deref()), ("photo-400x300.jpg", Some("image/jpeg")));
    let copy = workspace.read_all(&path("photo-400x300.jpg"), u64::MAX).await.unwrap();
    let info = probe(&copy).unwrap();
    assert_eq!((info.width, info.height, info.has_metadata), (400, 300, false));
}
