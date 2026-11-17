//! Node catalog checks: every node kind is described, and every example
//! workflow's node configs validate against the published config schemas.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;

use graphrt_cli::catalog::catalog;
use graphrt_cli::setup;

fn examples() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

#[test]
fn every_registered_kind_has_a_real_descriptor() {
    let registry = setup::registry();
    let descriptors = catalog();
    assert_eq!(descriptors.len(), registry.kinds().count());
    for d in &descriptors {
        assert!(!d.is_bare(), "node kind `{}` has no descriptor", d.kind);
        assert!(!d.description.is_empty(), "`{}` lacks a description", d.kind);
        assert!(!d.source.is_empty(), "`{}` lacks a source", d.kind);
        assert_eq!(d.config_schema["type"], "object", "`{}` schema must be an object schema", d.kind);
        jsonschema::validator_for(&d.config_schema)
            .unwrap_or_else(|e| panic!("`{}` publishes an invalid JSON Schema: {e}", d.kind));
    }
}

#[test]
fn example_node_configs_match_their_schemas() {
    let registry = setup::registry();
    let mut checked = 0;
    for entry in std::fs::read_dir(examples()).expect("examples dir") {
        let path = entry.expect("entry").path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default();
        if !matches!(ext, "yaml" | "yml") {
            continue;
        }
        let workflow = setup::read_workflow(&path).expect("example parses");
        for node in &workflow.graph.nodes {
            let Some(descriptor) = registry.descriptor(&node.kind) else {
                continue; // node kind not compiled into this build
            };
            let validator = jsonschema::validator_for(&descriptor.config_schema).expect("valid schema");
            let instance = serde_json::Value::Object(node.config.clone());
            let errors: Vec<String> =
                validator.iter_errors(&instance).map(|e| format!("{e} at {}", e.instance_path())).collect();
            assert!(
                errors.is_empty(),
                "{}: node `{}` ({}) does not match its schema: {errors:?}",
                path.display(),
                node.id,
                node.kind
            );
            checked += 1;
        }
    }
    assert!(checked >= 15, "expected to check the example nodes, checked {checked}");
}

#[test]
fn schemas_reject_obviously_wrong_configs() {
    let registry = setup::registry();
    let check = |kind: &str, config: serde_json::Value| -> bool {
        let d = registry.descriptor(kind).expect("kind");
        jsonschema::validator_for(&d.config_schema).expect("schema").is_valid(&config)
    };
    assert!(!check("iteration", serde_json::json!({})), "iteration requires iterator_selector");
    assert!(!check("if-else", serde_json::json!({"cases": [{"conditions": []}]})), "case_id is required");
    assert!(!check("loop", serde_json::json!({"loop_count": 0})), "loop_count >= 1");
    assert!(check("iteration", serde_json::json!({"iterator_selector": ["start", "items"]})));
    assert!(check("iteration", serde_json::json!({"iterator_selector": "start.items"})));
}
