//! End-to-end bundle tests through the `graphrt` binary: build, inspect, verify,
//! sign, run (incl. a bundled WebAssembly plugin) and serving bundles.
#![cfg(feature = "bundle")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graphrt-bundle-cli-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Run `graphrt` with an isolated HOME so installed plugins never leak in.
fn graphrt(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_graphrt"))
        .args(args)
        .env("HOME", home)
        .env_remove("GRAPHRT_PLUGIN_PATH")
        .env_remove("GRAPHRT_TRUSTED_KEYS")
        .output()
        .expect("spawn graphrt")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

const SLUG: &str = r#"version: "1"
name: slugger
graph:
  nodes:
    - { id: start, type: start, variables: [{ variable: text, type: text-input, required: true }] }
    - { id: slug, type: slugify, text: "{{#start.text#}}" }
    - { id: end, type: end, outputs: [{ variable: slug, value_selector: [slug, slug] }] }
  edges:
    - { source: start, target: slug }
    - { source: slug, target: end }
"#;

#[test]
fn hello_bundle_build_inspect_run() {
    let dir = scratch("hello");
    let out = dir.join("hello.grt");
    let wf = root().join("examples/hello.yaml");
    let o = graphrt(&dir, &["bundle", "build", wf.to_str().unwrap(), "-o", out.to_str().unwrap()]);
    assert!(o.status.success(), "{}", stderr(&o));

    let o = graphrt(&dir, &["bundle", "inspect", out.to_str().unwrap(), "--json"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let report: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(report["manifest"]["name"], "hello");
    assert_eq!(report["compatibility"]["missing_node_kinds"], serde_json::json!([]));

    let o = graphrt(&dir, &["run", out.to_str().unwrap(), "--input", "name=Ada", "-o", "json"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let result: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(result["outputs"]["greeting"], "Hello, Ada.");
}

#[test]
fn wasm_plugin_is_bundled_and_runs_without_the_plugin_installed() {
    let dir = scratch("wasm");
    let wf = dir.join("slug.yaml");
    std::fs::write(&wf, SLUG).unwrap();
    let out = dir.join("slug.grt");
    let plugins = root().join("plugins/examples/wasm-text-tools");
    let o = graphrt(
        &dir,
        &["bundle", "build", wf.to_str().unwrap(), "--plugins", plugins.to_str().unwrap(), "-o", out.to_str().unwrap()],
    );
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stdout(&o).contains("text-tools (auto)"), "{}", stdout(&o));

    // No plugin search paths: the module comes from the bundle.
    let o = graphrt(&dir, &["run", out.to_str().unwrap(), "--input", "text=Hello Bundle", "-o", "json"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let result: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(result["outputs"]["slug"], "hello-bundle");
}

#[test]
fn signing_and_trust() {
    let dir = scratch("sign");
    let out = dir.join("hello.grt");
    let key = dir.join("signing.key");
    let wf = root().join("examples/hello.yaml");
    assert!(graphrt(&dir, &["bundle", "build", wf.to_str().unwrap(), "-o", out.to_str().unwrap()]).status.success());

    let o = graphrt(&dir, &["bundle", "keygen", "--out", key.to_str().unwrap()]);
    assert!(o.status.success(), "{}", stderr(&o));
    let public = stdout(&o).lines().find_map(|l| l.strip_prefix("public key: ")).unwrap().trim().to_owned();
    // keygen never overwrites.
    assert!(!graphrt(&dir, &["bundle", "keygen", "--out", key.to_str().unwrap()]).status.success());

    // Unsigned bundles fail when a key is trusted.
    let o = graphrt(&dir, &["run", out.to_str().unwrap(), "--input", "name=A", "--trust-key", &public]);
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(stderr(&o).contains("unsigned"), "{}", stderr(&o));

    let o = graphrt(&dir, &["bundle", "sign", out.to_str().unwrap(), "--key", key.to_str().unwrap()]);
    assert!(o.status.success(), "{}", stderr(&o));
    let o = graphrt(&dir, &["bundle", "verify", out.to_str().unwrap(), "--trust-key", &public]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stdout(&o).contains("signed by"), "{}", stdout(&o));
    let o = graphrt(&dir, &["run", out.to_str().unwrap(), "--input", "name=A", "--trust-key", &public]);
    assert!(o.status.success(), "{}", stderr(&o));

    // Another key is not trusted.
    let other = "00".repeat(32);
    let o = graphrt(&dir, &["bundle", "verify", out.to_str().unwrap(), "--trust-key", &other]);
    assert!(!o.status.success());
}

#[test]
fn inline_provider_keys_are_rejected() {
    let dir = scratch("secrets");
    let providers = dir.join("providers.yaml");
    std::fs::write(&providers, "providers:\n  - { name: openai, kind: openai, api_key: sk-live-abc }\n").unwrap();
    let wf = root().join("examples/hello.yaml");
    let out = dir.join("x.grt");
    let o = graphrt(
        &dir,
        &[
            "bundle",
            "build",
            wf.to_str().unwrap(),
            "--providers",
            providers.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
        ],
    );
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("inline secret"), "{}", stderr(&o));
    assert!(!stderr(&o).contains("sk-live-abc"));
    assert!(!out.exists());
}

#[cfg(feature = "serve")]
#[tokio::test]
async fn serve_loads_bundles_from_a_directory() {
    use graphrt_cli::serve::{build_state, discover};
    use graphrt_cli::setup::ServiceOptions;

    let dir = scratch("serve");
    let wf = root().join("examples/hello.yaml");
    let out = dir.join("greeter.grt");
    assert!(graphrt(&dir, &["bundle", "build", wf.to_str().unwrap(), "-o", out.to_str().unwrap()]).status.success());
    let files = discover(&dir).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].0, "greeter");
    let options = ServiceOptions {
        plugins: graphrt_cli::extensions::PluginOptions {
            dirs: Vec::new(),
            no_default: true,
            trusted: Vec::new(),
            pool: Default::default(),
        },
        ..ServiceOptions::default()
    };
    let state = build_state(&files, &options, graphrt_core::engine::Limits::default(), None).await;
    assert!(state.is_ok(), "{:?}", state.err().map(|e| e.message));
}
