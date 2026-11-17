//! Loading portable bundles through the embedding API.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use graphrt_bundle::{BundleBuilder, generate_keypair, sign_bundle};
use graphrt_ffi::{EventListener, GraphrtError, Runtime};

struct Quiet;

impl EventListener for Quiet {
    fn on_event(&self, _: String) {}
    fn on_answer_chunk(&self, _: String) {}
    fn on_finish(&self, _: String) {}
}

fn repo(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(path)
}

const SLUG: &str = r#"
graph:
  nodes:
    - { id: start, type: start, variables: [{ variable: text, type: string, required: true }] }
    - { id: slug, type: slugify, text: "{{#start.text#}}" }
    - { id: end, type: end, outputs: [{ variable: slug, value_selector: [slug, slug] }] }
  edges:
    - { source: start, target: slug }
    - { source: slug, target: end }
"#;

fn slug_bundle() -> Vec<u8> {
    BundleBuilder::new("slugger", "1.0.0")
        .workflow("workflow.yaml", SLUG)
        .unwrap()
        .plugin_dir(&repo("plugins/examples/wasm-text-tools"))
        .unwrap()
        .build()
        .unwrap()
}

#[test]
fn bundle_with_wasm_plugin_runs_embedded() {
    let rt = Runtime::new("{}".into()).unwrap();
    let wf = rt.clone().load_bundle(slug_bundle(), Vec::new()).unwrap();
    let run = wf.run(r#"{"text":"Hello Phone"}"#.into(), "{}".into(), Arc::new(Quiet)).unwrap();
    let result: serde_json::Value = serde_json::from_str(&run.wait_json(10_000).expect("finished")).unwrap();
    assert_eq!(result["status"], "succeeded", "{result}");
    assert_eq!(result["outputs"]["slug"], "hello-phone");
    assert!(rt.catalog_json().contains("slugify"));
    // Loading the same bundle again reuses the registered plugin.
    assert!(rt.load_bundle(slug_bundle(), Vec::new()).is_ok());
}

#[test]
fn signatures_are_enforced_when_keys_are_trusted() {
    let keys = generate_keypair().unwrap();
    let rt = Runtime::new("{}".into()).unwrap();
    let unsigned = slug_bundle();
    assert!(matches!(
        rt.clone().load_bundle(unsigned.clone(), vec![keys.public_key.clone()]),
        Err(GraphrtError::InvalidWorkflow { .. })
    ));
    let signed = sign_bundle(&unsigned, &keys.secret_key).unwrap();
    assert!(rt.load_bundle(signed, vec![keys.public_key]).is_ok());
}

#[test]
fn process_plugins_and_corrupt_bundles_are_rejected() {
    let rt = Runtime::new("{}".into()).unwrap();
    let with_process = BundleBuilder::new("p", "1")
        .workflow("workflow.yaml", SLUG)
        .unwrap()
        .plugin_dir(&repo("plugins/examples/python-sentiment"))
        .unwrap()
        .build()
        .unwrap();
    match rt.clone().load_bundle(with_process, Vec::new()) {
        Err(GraphrtError::Plugin { message }) => assert!(message.contains("cannot run embedded"), "{message}"),
        Err(other) => panic!("unexpected error: {other}"),
        Ok(_) => panic!("process plugin bundle must be rejected"),
    }
    assert!(matches!(rt.load_bundle(b"not a zip".to_vec(), Vec::new()), Err(GraphrtError::InvalidWorkflow { .. })));
}
