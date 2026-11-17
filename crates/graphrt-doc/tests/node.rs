//! `document-extractor` node tests.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::Arc;

use futures::future::BoxFuture;
use graphrt_core::dsl::NodeDoc;
use graphrt_core::file::{FileBytes, FileError, FileResolver, base64_encode};
use graphrt_core::node::{NodeContext, NodeOutcome, Registry};
use graphrt_core::value::File;
use graphrt_core::{Selector, Services, Value, VariablePool};
use graphrt_doc::{DocLimits, KIND, register};

fn data_file(name: &str, mime: &str, body: &[u8]) -> Value {
    Value::from(File {
        name: name.into(),
        url: Some(format!("data:{mime};base64,{}", base64_encode(body))),
        ..File::default()
    })
}

fn node(config: serde_json::Value) -> Arc<dyn graphrt_core::Node> {
    let mut registry = Registry::new();
    register(&mut registry);
    registry.build(&NodeDoc::new("doc", KIND, config)).unwrap()
}

fn pool_with(value: Value) -> VariablePool {
    let mut pool = VariablePool::new();
    pool.insert("start", "file", value);
    pool
}

async fn run(
    config: serde_json::Value,
    value: Value,
    services: Services,
) -> Result<NodeOutcome, graphrt_core::NodeError> {
    let node = node(config);
    let ctx = NodeContext::for_test("doc", pool_with(value)).with_services(Arc::new(services));
    node.run(ctx).await
}

fn text_of(outcome: NodeOutcome) -> Value {
    match outcome {
        NodeOutcome::Completed(out) => out.outputs.get("text").cloned().unwrap(),
        other => panic!("unexpected {other:?}"),
    }
}

fn cfg() -> serde_json::Value {
    serde_json::json!({ "variable_selector": ["start", "file"] })
}

#[tokio::test]
async fn single_file_from_data_url() {
    let out = run(cfg(), data_file("notes.txt", "text/plain", b"hello doc\n"), Services::new()).await.unwrap();
    assert_eq!(text_of(out), Value::from("hello doc"));
}

#[tokio::test]
async fn file_list_preserves_order_and_markdown_option() {
    let list = Value::array([
        data_file("a.csv", "text/csv", b"h1,h2\n1,2\n"),
        data_file("b.html", "text/html", b"<p>second</p>"),
    ]);
    let config = serde_json::json!({ "variable_selector": ["start", "file"], "output_format": "markdown" });
    let out = run(config, list, Services::new()).await.unwrap();
    assert_eq!(
        text_of(out),
        Value::array([Value::from("| h1 | h2 |\n| --- | --- |\n| 1 | 2 |"), Value::from("second")])
    );
}

#[derive(Debug)]
struct MapResolver;

impl FileResolver for MapResolver {
    fn read<'a>(&'a self, file: &'a File, max_bytes: u64) -> BoxFuture<'a, Result<FileBytes, FileError>> {
        Box::pin(async move {
            match file.reference.as_deref() {
                Some("doc-1") => {
                    let bytes = b"stored body".to_vec();
                    if bytes.len() as u64 > max_bytes {
                        return Err(FileError::TooLarge { name: file.name.clone(), limit: max_bytes });
                    }
                    Ok(FileBytes { bytes, mime_type: Some("text/plain".into()) })
                }
                Some("flaky") => Err(FileError::Io { name: file.name.clone(), message: "reset".into() }),
                _ => Err(FileError::NotFound { name: file.name.clone() }),
            }
        })
    }
}

fn stored(reference: &str) -> Value {
    Value::from(File { name: "stored.bin".into(), reference: Some(reference.into()), ..File::default() })
}

fn with_resolver() -> Services {
    Services::new().with::<Arc<dyn FileResolver>>(Arc::new(MapResolver))
}

#[tokio::test]
async fn host_resolver_is_used() {
    let out = run(cfg(), stored("doc-1"), with_resolver()).await.unwrap();
    assert_eq!(text_of(out), Value::from("stored body"));
}

#[tokio::test]
async fn resolver_errors_map_to_node_errors() {
    let err = run(cfg(), stored("missing"), with_resolver()).await.unwrap_err();
    assert_eq!(err.kind, "file_unavailable");
    assert!(!err.retryable);
    let err = run(cfg(), stored("flaky"), with_resolver()).await.unwrap_err();
    assert_eq!(err.kind, "file_io");
    assert!(err.retryable);
    // Without any resolver only data URLs are readable.
    let err = run(cfg(), stored("doc-1"), Services::new()).await.unwrap_err();
    assert_eq!(err.kind, "missing_service");
}

#[tokio::test]
async fn host_limits_cap_config() {
    // Config asks for more than the host allows; the host limit wins.
    let config = serde_json::json!({ "variable_selector": ["start", "file"], "max_bytes": 1_000_000 });
    let limits = DocLimits { max_file_bytes: 4, ..DocLimits::default() };
    let services = with_resolver().with(limits);
    let err = run(config, stored("doc-1"), services).await.unwrap_err();
    assert_eq!(err.kind, "file_too_large");
}

#[tokio::test]
async fn invalid_inputs() {
    let err = run(cfg(), Value::Null, Services::new()).await.unwrap_err();
    assert_eq!(err.kind, "invalid_input");
    let err = run(cfg(), Value::from("not a file"), Services::new()).await.unwrap_err();
    assert_eq!(err.kind, "invalid_input");
    let err = run(cfg(), Value::array([Value::Int(1)]), Services::new()).await.unwrap_err();
    assert!(err.message.contains("item 0"), "{}", err.message);
    let err =
        run(cfg(), data_file("x.bin", "application/octet-stream", &[0, 1, 2, 0]), Services::new()).await.unwrap_err();
    assert_eq!(err.kind, "unsupported_format");
    assert!(err.message.contains("x.bin"), "{}", err.message);
}

#[test]
fn descriptor_and_inputs() {
    let mut registry = Registry::new();
    register(&mut registry);
    let d = registry.descriptor(KIND).unwrap();
    assert_eq!(d.title, "Document Extractor");
    assert_eq!(d.outputs.first().map(|o| o.name.as_str()), Some("text"));
    assert_eq!(d.config_schema["required"][0], "variable_selector");
    assert_eq!(node(cfg()).inputs(), vec![Selector::parse("start.file").unwrap()]);
    let bad = registry.build(&NodeDoc::new("doc", KIND, serde_json::json!({ "max_bytes": 5 })));
    assert!(bad.is_err());
}

#[test]
fn runs_without_tokio() {
    let node = node(cfg());
    let ctx = NodeContext::for_test("doc", pool_with(data_file("a.md", "text/markdown", b"# Hi")));
    let out = futures::executor::block_on(node.run(ctx)).unwrap();
    assert_eq!(text_of(out), Value::from("# Hi"));
}
