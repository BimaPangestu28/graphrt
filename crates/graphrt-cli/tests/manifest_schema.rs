//! `docs/schemas/plugin-manifest.schema.json` against the real manifest parser:
//! the example, fixture and scaffolded manifests validate, the schema lists
//! exactly the fields serde accepts, and schema and parser agree on a table of
//! structural cases.
#![cfg(any(feature = "plugins", feature = "wasm"))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use graphrt_plugin::PluginManifest;
use serde_json::{Value, json};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn manifest_schema() -> Value {
    let text =
        std::fs::read_to_string(repo_root().join("docs/schemas/plugin-manifest.schema.json")).expect("schema file");
    serde_json::from_str(&text).expect("schema is JSON")
}

fn validator() -> jsonschema::Validator {
    jsonschema::validator_for(&manifest_schema()).expect("schema compiles")
}

fn schema_errors(validator: &jsonschema::Validator, manifest: &Value) -> Vec<String> {
    validator.iter_errors(manifest).map(|error| format!("{} at {}", error, error.instance_path())).collect()
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("manifest file")).expect("manifest JSON")
}

#[test]
fn schema_is_valid_draft_2020_12() {
    let schema = manifest_schema();
    assert_eq!(schema["$schema"], "https://json-schema.org/draft/2020-12/schema");
    jsonschema::meta::validate(&schema).expect("valid against the 2020-12 meta-schema");
}

/// The example plugins, the plugin test kit and every `graphrt plugin new` template.
fn known_manifests() -> Vec<(String, Value)> {
    let mut manifests = Vec::new();
    for entry in std::fs::read_dir(repo_root().join("plugins/examples")).expect("examples") {
        let manifest_path = entry.expect("entry").path().join("graphrt-plugin.json");
        if manifest_path.is_file() {
            manifests.push((manifest_path.display().to_string(), read_json(&manifest_path)));
        }
    }
    let testkit = repo_root().join("crates/graphrt-plugin/tests/fixtures/testkit/graphrt-plugin.json");
    manifests.push((testkit.display().to_string(), read_json(&testkit)));
    manifests.extend(scaffolded_manifests());
    assert!(manifests.len() >= 8, "expected the examples, the test kit and three templates: {}", manifests.len());
    manifests
}

fn scaffolded_manifests() -> Vec<(String, Value)> {
    let parent = std::env::temp_dir().join(format!("graphrt-manifest-schema-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&parent);
    std::fs::create_dir_all(&parent).expect("scratch dir");
    ["python", "typescript", "rust"]
        .into_iter()
        .map(|language| {
            let name = format!("scaffold-{language}");
            let output = Command::new(env!("CARGO_BIN_EXE_graphrt"))
                .args(["plugin", "new", &name, "--lang", language, "--dir", parent.to_str().unwrap()])
                .output()
                .expect("spawn graphrt");
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            (
                format!("`graphrt plugin new --lang {language}`"),
                read_json(&parent.join(&name).join("graphrt-plugin.json")),
            )
        })
        .collect()
}

#[test]
fn example_fixture_and_scaffolded_manifests_match_the_schema() {
    let validator = validator();
    for (origin, manifest) in known_manifests() {
        let errors = schema_errors(&validator, &manifest);
        assert!(errors.is_empty(), "{origin}: {errors:#?}");
        assert!(PluginManifest::from_json(&manifest.to_string()).is_ok(), "{origin} must also parse");
    }
}

/// The field names serde lists in its `unknown field ..., expected one of ...` error.
fn fields_the_parser_accepts(manifest_with_unknown_field: &Value) -> BTreeSet<String> {
    let message = PluginManifest::from_json(&manifest_with_unknown_field.to_string()).unwrap_err().to_string();
    let expected_list = message.split("expected one of ").nth(1).unwrap_or_else(|| panic!("no field list: {message}"));
    expected_list.split(", ").filter_map(|item| item.split('`').nth(1)).map(str::to_owned).collect()
}

fn schema_properties(schema_object: &Value) -> BTreeSet<String> {
    schema_object["properties"].as_object().expect("properties").keys().cloned().collect()
}

fn base_manifest() -> Value {
    json!({ "name": "acme", "runtime": "process", "command": "python3" })
}

fn with_node(node: Value) -> Value {
    let mut manifest = base_manifest();
    manifest["nodes"] = json!([node]);
    manifest
}

#[test]
fn schema_lists_exactly_the_fields_the_parser_accepts() {
    let schema = manifest_schema();
    let definitions = &schema["$defs"];
    let mut unknown_top_level = base_manifest();
    unknown_top_level["zz_unknown"] = json!(1);
    let mut unknown_permission = base_manifest();
    unknown_permission["permissions"] = json!({ "zz_unknown": 1 });
    let mut unknown_limit = base_manifest();
    unknown_limit["limits"] = json!({ "zz_unknown": 1 });
    let unknown_node_field = with_node(json!({ "kind": "acme.x", "title": "X", "zz_unknown": 1 }));
    let unknown_output_field = with_node(
        json!({ "kind": "acme.x", "title": "X", "outputs": [{ "name": "o", "type": "string", "zz_unknown": 1 }] }),
    );
    let unknown_mcp_field = json!({ "name": "acme", "runtime": "mcp", "mcp": { "url": "https://x", "zz_unknown": 1 } });
    let unknown_approval_field =
        json!({ "name": "acme", "runtime": "mcp", "mcp": { "url": "https://x", "approval": { "zz_unknown": 1 } } });
    let mut unknown_template_field = base_manifest();
    unknown_template_field["templates"] =
        json!([{ "id": "a", "title": "T", "description": "D", "prompt": "P", "zz_unknown": 1 }]);
    let cases = [
        ("manifest", &unknown_top_level, &schema),
        ("permissions", &unknown_permission, &definitions["permissions"]),
        ("limits", &unknown_limit, &definitions["limits"]),
        ("node", &unknown_node_field, &definitions["node"]),
        ("output", &unknown_output_field, &definitions["output"]),
        ("mcp server", &unknown_mcp_field, &definitions["mcp_server"]),
        ("mcp approval", &unknown_approval_field, &definitions["mcp_approval"]),
        ("template", &unknown_template_field, &definitions["template"]),
    ];
    for (label, manifest, schema_object) in cases {
        assert_eq!(schema_properties(schema_object), fields_the_parser_accepts(manifest), "{label} fields");
        assert_eq!(schema_object["additionalProperties"], json!(false), "{label} uses deny_unknown_fields");
    }
    // `ToolSpec` accepts unknown keys, so its fields are listed by hand.
    let tool = &definitions["tool"];
    assert_eq!(schema_properties(tool), BTreeSet::from(["name".into(), "description".into(), "parameters".into()]));
    assert_eq!(tool["required"], json!(["name", "description", "parameters"]));
}

/// Manifests the schema and the parser must judge the same way.
fn agreement_cases() -> Vec<(&'static str, Value)> {
    let edit = |change: fn(&mut Value)| {
        let mut manifest = base_manifest();
        change(&mut manifest);
        manifest
    };
    vec![
        ("minimal process manifest", base_manifest()),
        ("unknown top-level field", edit(|m| m["comand"] = json!("x"))),
        ("missing runtime", json!({ "name": "acme", "command": "x" })),
        ("unknown runtime", edit(|m| m["runtime"] = json!("docker"))),
        ("process without command", json!({ "name": "acme", "runtime": "process" })),
        ("null command", edit(|m| m["command"] = Value::Null)),
        ("invalid name", edit(|m| m["name"] = json!("Acme Corp"))),
        ("65-character name", edit(|m| m["name"] = json!("a".repeat(65)))),
        ("null version", edit(|m| m["version"] = Value::Null)),
        ("null author", edit(|m| m["author"] = Value::Null)),
        ("protocol_version 2", edit(|m| m["protocol_version"] = json!(2))),
        ("protocol_version 1", edit(|m| m["protocol_version"] = json!(1))),
        ("process with url", edit(|m| m["url"] = json!("https://x.example"))),
        ("process with module", edit(|m| m["module"] = json!("p.wasm"))),
        ("process with empty headers", edit(|m| m["headers"] = json!({}))),
        ("process env and cwd", edit(|m| m["env"] = json!({ "A": "1" }))),
        ("http minimal", json!({ "name": "h", "runtime": "http", "url": "https://p.example/rpc" })),
        ("http without url", json!({ "name": "h", "runtime": "http" })),
        ("http with command", json!({ "name": "h", "runtime": "http", "url": "https://p.example", "command": "x" })),
        ("http with empty args", json!({ "name": "h", "runtime": "http", "url": "https://p.example", "args": [] })),
        ("http with args", json!({ "name": "h", "runtime": "http", "url": "https://p.example", "args": ["a"] })),
        (
            "http with inherit_env false",
            json!({ "name": "h", "runtime": "http", "url": "https://p.example", "inherit_env": false }),
        ),
        (
            "http with inherit_env true",
            json!({ "name": "h", "runtime": "http", "url": "https://p.example", "inherit_env": true }),
        ),
        ("http with ftp url", json!({ "name": "h", "runtime": "http", "url": "ftp://p.example" })),
        ("wasm minimal", json!({ "name": "w", "runtime": "wasm", "module": "plugin.wasm" })),
        ("wasm without module", json!({ "name": "w", "runtime": "wasm" })),
        ("wasm with env", json!({ "name": "w", "runtime": "wasm", "module": "p.wasm", "env": { "A": "1" } })),
        ("wasm module escaping", json!({ "name": "w", "runtime": "wasm", "module": "../p.wasm" })),
        ("wasm absolute module", json!({ "name": "w", "runtime": "wasm", "module": "/p.wasm" })),
        ("permissions", edit(|m| m["permissions"] = json!({ "network": true, "env": ["A"], "secrets": false }))),
        ("permissions.env not strings", edit(|m| m["permissions"] = json!({ "env": [1] }))),
        ("limits", edit(|m| m["limits"] = json!({ "timeout_ms": 1, "max_concurrency": 1, "max_message_bytes": 1024 }))),
        ("limits.timeout_ms 0", edit(|m| m["limits"] = json!({ "timeout_ms": 0 }))),
        ("limits.max_concurrency 0", edit(|m| m["limits"] = json!({ "max_concurrency": 0 }))),
        ("limits.max_message_bytes 1023", edit(|m| m["limits"] = json!({ "max_message_bytes": 1023 }))),
        ("negative limit", edit(|m| m["limits"] = json!({ "timeout_ms": -5 }))),
        ("checksum path escaping", edit(|m| m["checksums"] = json!({ "../evil": "00" }))),
        ("checksum value not a string", edit(|m| m["checksums"] = json!({ "plugin.py": 1 }))),
        ("node", with_node(json!({ "kind": "acme.x", "title": "X", "category": "model", "config_schema": {} }))),
        ("node without title", with_node(json!({ "kind": "acme.x" }))),
        ("node unknown field", with_node(json!({ "kind": "acme.x", "title": "X", "handels": {} }))),
        ("node bad category", with_node(json!({ "kind": "acme.x", "title": "X", "category": "magic" }))),
        ("node kind with space", with_node(json!({ "kind": "acme. x", "title": "X" }))),
        (
            "node fixed handles",
            with_node(json!({ "kind": "acme.x", "title": "X", "handles": { "kind": "fixed", "handles": ["a"] } })),
        ),
        (
            "node fixed handles missing",
            with_node(json!({ "kind": "acme.x", "title": "X", "handles": { "kind": "fixed" } })),
        ),
        (
            "node dynamic handles",
            with_node(json!({ "kind": "acme.x", "title": "X", "handles": { "kind": "dynamic" } })),
        ),
        (
            "node unknown handles kind",
            with_node(json!({ "kind": "acme.x", "title": "X", "handles": { "kind": "many" } })),
        ),
        ("container node", with_node(json!({ "kind": "acme.x", "title": "X", "container": true }))),
        (
            "output aliases",
            with_node(json!({ "kind": "acme.x", "title": "X", "outputs": [{ "name": "o", "type": "json" }] })),
        ),
        (
            "output bad type",
            with_node(json!({ "kind": "acme.x", "title": "X", "outputs": [{ "name": "o", "type": "text" }] })),
        ),
        ("output without type", with_node(json!({ "kind": "acme.x", "title": "X", "outputs": [{ "name": "o" }] }))),
        ("tool", edit(|m| m["tools"] = json!([{ "name": "t", "description": "d", "parameters": {}, "x-extra": 1 }]))),
        ("tool without parameters", edit(|m| m["tools"] = json!([{ "name": "t", "description": "d" }]))),
        ("tool with empty name", edit(|m| m["tools"] = json!([{ "name": " ", "description": "d", "parameters": {} }]))),
        ("allow_unprefixed_kinds", edit(|m| m["allow_unprefixed_kinds"] = json!(true))),
        (
            "mcp preset",
            json!({ "name": "acme", "runtime": "mcp", "mcp": {
                "url": "https://mcp.example.com/mcp", "transport": "streamable-http", "auth": "oauth",
                "approval": { "default": "ask", "read_only": ["get_*", "list_items"], "ask_first": ["get_key"] },
                "guide": "guide.md"
            } }),
        ),
        ("mcp without server", json!({ "name": "acme", "runtime": "mcp" })),
        ("mcp unknown field", json!({ "name": "acme", "runtime": "mcp", "mcp": { "url": "https://x", "zz": 1 } })),
        ("mcp bad auth", json!({ "name": "acme", "runtime": "mcp", "mcp": { "url": "https://x", "auth": "basic" } })),
        (
            "mcp bad transport",
            json!({ "name": "acme", "runtime": "mcp", "mcp": { "url": "https://x", "transport": "sse" } }),
        ),
        (
            "mcp bad pattern",
            json!({ "name": "acme", "runtime": "mcp", "mcp": { "url": "https://x", "approval": { "read_only": ["a*b"] } } }),
        ),
        (
            "mcp with command",
            json!({ "name": "acme", "runtime": "mcp", "command": "x", "mcp": { "url": "https://x" } }),
        ),
        ("mcp on process", edit(|m| m["mcp"] = json!({ "url": "https://x" }))),
        (
            "mcp with tools",
            json!({ "name": "acme", "runtime": "mcp", "mcp": { "url": "https://x" },
                "tools": [{ "name": "t", "description": "d", "parameters": {} }] }),
        ),
        (
            "templates",
            edit(|m| m["templates"] = json!([{ "id": "start-1", "title": "T", "description": "D", "prompt": "P" }])),
        ),
        (
            "template bad id",
            edit(|m| m["templates"] = json!([{ "id": "Start", "title": "T", "description": "D", "prompt": "P" }])),
        ),
        (
            "template missing prompt",
            edit(|m| m["templates"] = json!([{ "id": "a", "title": "T", "description": "D" }])),
        ),
        (
            "template unknown field",
            edit(|m| m["templates"] = json!([{ "id": "a", "title": "T", "description": "D", "prompt": "P", "z": 1 }])),
        ),
    ]
}

#[test]
fn schema_and_parser_agree_on_structural_cases() {
    let validator = validator();
    for (label, manifest) in agreement_cases() {
        let parser_accepts = PluginManifest::from_json(&manifest.to_string());
        let schema_accepts = validator.is_valid(&manifest);
        assert_eq!(
            schema_accepts,
            parser_accepts.is_ok(),
            "{label}: schema errors {:?}, parser result {:?}",
            schema_errors(&validator, &manifest),
            parser_accepts.err().map(|error| error.to_string())
        );
    }
}
