//! End-to-end bundle tests: build, open, verify, run.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use graphrt_bundle::{Bundle, BundleBuilder, BundleError, OpenOptions, generate_keypair, sign_bundle};
use graphrt_core::{Engine, Event, Graph, Registry, RunInput, Value};

fn repo(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(path)
}

fn read(path: &str) -> String {
    std::fs::read_to_string(repo(path)).unwrap()
}

#[cfg(feature = "wasm")]
const SLUG_WORKFLOW: &str = r#"
version: "1"
name: slugger
env:
  - { name: API_TOKEN, type: secret }
graph:
  nodes:
    - id: start
      type: start
      variables: [{ variable: text, type: text-input, required: true }]
    - { id: slug, type: slugify, text: "{{#start.text#}}" }
    - id: end
      type: end
      outputs: [{ variable: slug, value_selector: [slug, slug] }]
  edges:
    - { source: start, target: slug }
    - { source: slug, target: end }
"#;

async fn run(graph: Graph, inputs: &[(&str, Value)]) -> serde_json::Value {
    let engine = Engine::new(Arc::new(graph));
    let mut input = RunInput::new();
    for (k, v) in inputs {
        input.inputs.insert((*k).to_owned(), v.clone());
    }
    let events: Vec<Event> = futures::StreamExt::collect(engine.run(input)).await;
    match events.last() {
        Some(Event::RunSucceeded { outputs, .. }) => serde_json::to_value(outputs).unwrap(),
        other => panic!("run did not succeed: {other:?}"),
    }
}

fn hello_bundle() -> Vec<u8> {
    BundleBuilder::new("hello", "1.0.0")
        .description("greets")
        .created_at("2026-01-01T00:00:00Z")
        .workflow("workflow.yaml", read("examples/hello.yaml"))
        .unwrap()
        .file("assets/readme.txt", b"hi".to_vec())
        .unwrap()
        .build()
        .unwrap()
}

#[tokio::test]
async fn build_open_run_roundtrip() {
    let bytes = hello_bundle();
    // Deterministic: same inputs, same bytes.
    assert_eq!(bytes, hello_bundle());

    let bundle = Bundle::open(&bytes, &OpenOptions::default()).unwrap();
    let m = bundle.manifest();
    assert_eq!(m.name, "hello");
    assert_eq!(m.entry, "workflow.yaml");
    assert!(m.requires.node_kinds.contains(&"template-transform".to_owned()));
    assert!(m.files.contains_key("assets/readme.txt"));
    assert!(bundle.signer().is_none());
    assert_eq!(bundle.file("assets/readme.txt"), Some(&b"hi"[..]));

    let registry = Registry::with_builtins();
    let report = bundle.check(&registry);
    assert!(report.is_compatible(), "{report:?}");

    let graph = Graph::build(bundle.workflow(None).unwrap(), &registry).unwrap();
    let out = run(graph, &[("name", Value::from("Ada"))]).await;
    assert_eq!(out["greeting"], "Hello, Ada.");
    assert!(bundle.workflow(Some("workflow")).is_ok());
    assert!(bundle.workflow(Some("nope")).is_err());
}

#[cfg(feature = "wasm")]
#[tokio::test]
async fn wasm_plugin_bundle_runs() {
    let bytes = BundleBuilder::new("slugger", "0.1.0")
        .workflow("workflow.yaml", SLUG_WORKFLOW)
        .unwrap()
        .plugin_dir(&repo("plugins/examples/wasm-text-tools"))
        .unwrap()
        .build()
        .unwrap();
    let bundle = Bundle::open(&bytes, &OpenOptions::default()).unwrap();
    let req = &bundle.manifest().requires;
    assert_eq!(req.env, vec!["API_TOKEN".to_owned()]);
    assert_eq!(req.plugins.len(), 1);
    assert!(req.plugins[0].portable);

    let mut registry = Registry::with_builtins();
    assert_eq!(bundle.check(&registry).missing_node_kinds, vec!["slugify".to_owned()]);
    let kinds = bundle.register_wasm_plugins(&mut registry, &graphrt_bundle::WasmLimits::default()).unwrap();
    assert!(kinds.contains(&"slugify".to_owned()));
    assert!(bundle.check(&registry).is_compatible());
    // Registering again skips plugins whose kinds already exist (never replaces them).
    assert!(bundle.register_wasm_plugins(&mut registry, &graphrt_bundle::WasmLimits::default()).unwrap().is_empty());

    let graph = Graph::build(bundle.workflow(None).unwrap(), &registry).unwrap();
    let out = run(graph, &[("text", Value::from("Hello Bundle World"))]).await;
    assert_eq!(out["slug"], "hello-bundle-world");
}

/// Rewrite one entry of a zip, keeping the others.
fn rewrite(bytes: &[u8], replace: &str, data: &[u8], extra: Option<(&str, &[u8])>) -> Vec<u8> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    for i in 0..archive.len() {
        let mut f = archive.by_index(i).unwrap();
        let name = f.name().unwrap().into_owned();
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut f, &mut buf).unwrap();
        out.start_file(name.as_str(), opts).unwrap();
        out.write_all(if name == replace { data } else { &buf }).unwrap();
    }
    if let Some((name, content)) = extra {
        out.start_file(name, opts).unwrap();
        out.write_all(content).unwrap();
    }
    out.finish().unwrap().into_inner()
}

#[test]
fn tampering_is_detected() {
    let bytes = hello_bundle();
    let tampered = rewrite(&bytes, "assets/readme.txt", b"evil", None);
    assert!(matches!(Bundle::open(&tampered, &OpenOptions::default()), Err(BundleError::Integrity(_))));
    let extra = rewrite(&bytes, "", b"", Some(("assets/unlisted.txt", b"x")));
    assert!(matches!(Bundle::open(&extra, &OpenOptions::default()), Err(BundleError::Integrity(_))));
}

#[test]
fn signatures() {
    let bytes = hello_bundle();
    let keys = generate_keypair().unwrap();
    let other = generate_keypair().unwrap();
    let signed = sign_bundle(&bytes, &keys.secret_key).unwrap();

    let opened = Bundle::open(&signed, &OpenOptions::default().trust_key(&keys.public_key)).unwrap();
    assert_eq!(opened.signer(), Some(keys.public_key.as_str()));
    // Untrusted signer, unsigned bundle, required signature.
    assert!(matches!(
        Bundle::open(&signed, &OpenOptions::default().trust_key(&other.public_key)),
        Err(BundleError::Signature(_))
    ));
    assert!(matches!(
        Bundle::open(&bytes, &OpenOptions::default().trust_key(&keys.public_key)),
        Err(BundleError::Signature(_))
    ));
    let required = OpenOptions { require_signature: true, ..OpenOptions::default() };
    assert!(Bundle::open(&bytes, &required).is_err());
    assert!(Bundle::open(&signed, &required).is_ok());

    // A modified manifest invalidates the signature.
    let manifest = opened.manifest_bytes().to_vec();
    let forged = String::from_utf8(manifest).unwrap().replace("\"hello\"", "\"hallo\"");
    let tampered = rewrite(&signed, "bundle.json", forged.as_bytes(), None);
    assert!(matches!(Bundle::open(&tampered, &OpenOptions::default()), Err(BundleError::Signature(_))));
}

fn raw_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, data) in entries {
        out.start_file(*name, opts).unwrap();
        out.write_all(data).unwrap();
    }
    out.finish().unwrap().into_inner()
}

#[test]
fn unsafe_archives_are_rejected() {
    for name in ["../evil.txt", "/etc/passwd", "a/../../b", "a\\b"] {
        let zip = raw_zip(&[("bundle.json", b"{}"), (name, b"x")]);
        let err = Bundle::open(&zip, &OpenOptions::default()).unwrap_err();
        assert!(matches!(err, BundleError::Archive(_)), "{name}: {err}");
    }
    // Zip bomb: highly compressible 8 MiB entry.
    let bomb = vec![0u8; 8 * 1024 * 1024];
    let zip = raw_zip(&[("bundle.json", b"{}"), ("bomb.bin", &bomb)]);
    assert!(matches!(Bundle::open(&zip, &OpenOptions::default()), Err(BundleError::Archive(_))));
    // Too many entries.
    let names: Vec<String> = (0..20).map(|i| format!("f{i}")).collect();
    let entries: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b"x"[..])).collect();
    let opts = OpenOptions { max_entries: 10, ..OpenOptions::default() };
    assert!(matches!(Bundle::open(&raw_zip(&entries), &opts), Err(BundleError::Archive(_))));
    // Missing manifest.
    assert!(matches!(Bundle::open(&raw_zip(&[("x", b"y")]), &OpenOptions::default()), Err(BundleError::Manifest(_))));
}

#[test]
fn compatibility_report_lists_missing_kinds() {
    let workflow = r#"
graph:
  nodes:
    - { id: start, type: start }
    - { id: x, type: acme.magic, model: { provider: openai, name: gpt } }
  edges: [{ source: start, target: x }]
"#;
    let bytes = BundleBuilder::new("x", "1").workflow("w.yaml", workflow).unwrap().build().unwrap();
    let bundle = Bundle::open(&bytes, &OpenOptions::default()).unwrap();
    let report = bundle.check(&Registry::with_builtins());
    assert_eq!(report.missing_node_kinds, vec!["acme.magic".to_owned()]);
    assert_eq!(report.providers, vec!["openai".to_owned()]);
    assert!(!report.is_compatible());
}

#[test]
fn inline_secrets_are_rejected() {
    let builder = || BundleBuilder::new("x", "1").workflow("w.yaml", read("examples/hello.yaml")).unwrap();
    let bad = "providers:\n  - { name: openai, kind: openai, api_key: sk-live-123 }\n";
    let err = builder().providers(bad).unwrap_err();
    assert!(matches!(err, BundleError::Secret(_)));
    assert!(!err.to_string().contains("sk-live-123"));

    let good = "providers:\n  - { name: openai, kind: openai, api_key_env: OPENAI_API_KEY }\n";
    let bytes = builder().providers(good).unwrap().build().unwrap();
    let bundle = Bundle::open(&bytes, &OpenOptions::default()).unwrap();
    assert!(bundle.providers_config().is_some());

    let kb_bad = "embedders: { e: { type: openai, model: m, api_key: k } }\n";
    assert!(matches!(builder().knowledge(kb_bad, Path::new(".")), Err(BundleError::Secret(_))));
}

#[test]
fn knowledge_store_ships_and_is_relocated() {
    let dir = std::env::temp_dir().join(format!("graphrt-bundle-kb-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("store.json"), b"{\"chunks\":[]}").unwrap();
    let kb = "stores:\n  local: { type: memory, path: store.json }\ndatasets:\n  - { id: d, store: local }\n";
    let bytes = BundleBuilder::new("kb", "1")
        .workflow("w.yaml", read("examples/hello.yaml"))
        .unwrap()
        .knowledge(kb, &dir)
        .unwrap()
        .build()
        .unwrap();
    let bundle = Bundle::open(&bytes, &OpenOptions::default()).unwrap();
    assert!(bundle.file("knowledge/local-store.json").is_some());

    let out = dir.join("extracted");
    bundle.extract_to(&out).unwrap();
    let config = bundle.knowledge_config_at(&out).unwrap().unwrap();
    let path = config["stores"]["local"]["path"].as_str().unwrap();
    assert!(Path::new(path).is_file(), "{path}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sqlite_store_with_pending_write_ahead_log_is_rejected() {
    let dir = std::env::temp_dir().join(format!("graphrt-bundle-kb-wal-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("kb.db"), b"main file").unwrap();
    std::fs::write(dir.join("kb.db-wal"), b"pending commits").unwrap();
    let kb = "stores:\n  local: { type: sqlite, path: kb.db }\ndatasets:\n  - { id: d, store: local }\n";
    let build =
        || BundleBuilder::new("kb", "1").workflow("w.yaml", read("examples/hello.yaml")).unwrap().knowledge(kb, &dir);

    let Err(BundleError::Build(message)) = build() else { panic!("expected a build error") };
    assert!(message.contains("write-ahead log"), "{message}");

    // An empty log (left after a clean checkpoint) is fine.
    std::fs::write(dir.join("kb.db-wal"), b"").unwrap();
    let bytes = build().unwrap().build().unwrap();
    let bundle = Bundle::open(&bytes, &OpenOptions::default()).unwrap();
    assert!(bundle.file("knowledge/local-kb.db").is_some());
    let _ = std::fs::remove_dir_all(&dir);
}
