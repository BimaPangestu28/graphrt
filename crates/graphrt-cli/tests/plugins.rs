//! End-to-end plugin tests through the `graphrt` binary: process plugins (Python,
//! JavaScript), a WebAssembly plugin, discovery, install/remove and scaffolding.
#![cfg(all(feature = "plugins", feature = "wasm"))]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn plugin_examples() -> PathBuf {
    root().join("plugins/examples")
}

/// A scratch directory unique to one test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graphrt-plugin-cli-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Run `graphrt` with an isolated HOME and plugin path, so the developer's own
/// `~/.graphrt/plugins` never leaks into tests.
fn graphrt(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_graphrt"))
        .args(args)
        .env("HOME", home)
        .env_remove("GRAPHRT_PLUGIN_PATH")
        .output()
        .expect("spawn graphrt")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn has(program: &str) -> bool {
    Command::new(program).arg("--version").output().is_ok_and(|o| o.status.success())
}

#[test]
fn workflow_runs_python_javascript_and_wasm_plugins() {
    if !has("python3") || !has("node") {
        eprintln!("skipping: python3 and node are required");
        return;
    }
    let home = scratch("run");
    let workflow = plugin_examples().join("demo.yaml");
    let plugins = plugin_examples();
    let out = graphrt(
        &home,
        &[
            "run",
            workflow.to_str().unwrap(),
            "--plugins",
            plugins.to_str().unwrap(),
            "--input",
            "review=I love this, the support was great",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("positive (1.0)"), "{text}");
    assert!(text.contains("I LOVE THIS, THE SUPPORT WAS GREAT"), "{text}");
    assert!(text.contains("i-love-this-the-support-was-great"), "{text}");

    // Same workflow, negative branch, JSON output.
    let out = graphrt(
        &home,
        &[
            "run",
            workflow.to_str().unwrap(),
            "--plugins",
            plugins.to_str().unwrap(),
            "--input",
            "review=terrible and slow",
            "--output",
            "json",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let result: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    assert_eq!(result["status"], "succeeded");
    assert!(result["outputs"]["answer"].as_str().unwrap().starts_with("negative"), "{result}");
}

#[test]
fn catalog_includes_plugin_kinds() {
    let home = scratch("nodes");
    let plugins = plugin_examples();
    let out = graphrt(&home, &["nodes", "--json", "--plugins", plugins.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
    let catalog: Vec<serde_json::Value> = serde_json::from_str(&stdout(&out)).expect("json");
    let find = |kind: &str| catalog.iter().find(|d| d["kind"] == kind).cloned();
    assert_eq!(find("sentiment.analyze").expect("python kind")["source"], "plugin:sentiment");
    assert_eq!(find("upper.transform").expect("js kind")["source"], "plugin:upper");
    assert_eq!(find("slugify").expect("wasm kind")["source"], "plugin:text-tools");
    assert!(find("llm").is_some() || find("start").is_some(), "built-ins still listed");

    // Without the plugin path the kinds disappear.
    let out = graphrt(&home, &["nodes", "--json"]);
    let catalog: Vec<serde_json::Value> = serde_json::from_str(&stdout(&out)).expect("json");
    assert!(catalog.iter().all(|d| d["kind"] != "sentiment.analyze"));
}

#[test]
fn validate_explains_missing_plugin_kinds() {
    let home = scratch("validate");
    let workflow = plugin_examples().join("demo.yaml");
    let out = graphrt(&home, &["validate", workflow.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    let err = stderr(&out);
    assert!(err.contains("unknown node type"), "{err}");
    assert!(err.contains("graphrt plugin install"), "{err}");

    let plugins = plugin_examples();
    let out = graphrt(&home, &["validate", workflow.to_str().unwrap(), "--plugins", plugins.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn bare_wasm_module_is_a_plugin() {
    let home = scratch("bare-wasm");
    let dir = home.join("mods");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(plugin_examples().join("wasm-text-tools/text_tools.wasm"), dir.join("text_tools.wasm")).unwrap();
    let workflow = home.join("wf.yaml");
    std::fs::write(
        &workflow,
        "graph:\n  nodes:\n    - { id: start, type: start, variables: [{ variable: t, type: text-input }] }\n    \
         - { id: s, type: slugify, text: \"{{#start.t#}}\" }\n    - { id: end, type: end, outputs: [{ variable: slug, value_selector: [s, slug] }] }\n  \
         edges:\n    - { source: start, target: s }\n    - { source: s, target: end }\n",
    )
    .unwrap();
    let out = graphrt(
        &home,
        &[
            "run",
            workflow.to_str().unwrap(),
            "--plugins",
            dir.to_str().unwrap(),
            "--input",
            "t=Hello Wasm World",
            "-o",
            "json",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let result: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    assert_eq!(result["outputs"]["slug"], "hello-wasm-world");
}

#[test]
fn install_list_info_remove() {
    let home = scratch("install");
    let source = plugin_examples().join("python-sentiment");
    let out = graphrt(&home, &["plugin", "install", source.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("installed sentiment"), "{}", stdout(&out));
    assert!(home.join(".graphrt/plugins/sentiment/graphrt-plugin.json").is_file());

    let out = graphrt(&home, &["plugin", "list", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let list: Vec<serde_json::Value> = serde_json::from_str(&stdout(&out)).expect("json");
    let sentiment = list.iter().find(|p| p["name"] == "sentiment").expect("listed");
    assert_eq!(sentiment["runtime"], "process");
    assert_eq!(sentiment["nodes"], serde_json::json!(["sentiment.analyze"]));
    assert_eq!(sentiment["tools"], serde_json::json!(["score"]));

    let out = graphrt(&home, &["plugin", "info", "sentiment"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("node sentiment.analyze"), "{}", stdout(&out));

    // An installed plugin is discovered without `--plugins`.
    let out = graphrt(&home, &["nodes", "--kind", "sentiment.analyze"]);
    assert!(out.status.success(), "{}", stderr(&out));

    let out = graphrt(&home, &["plugin", "remove", "sentiment"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!home.join(".graphrt/plugins/sentiment").exists());
    let out = graphrt(&home, &["plugin", "remove", "../etc"]);
    assert_eq!(out.status.code(), Some(2));
}

/// Copy a plugin example into `dst` (files only; the examples are flat).
fn copy_plugin(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("plugin dir");
    for entry in std::fs::read_dir(src).expect("read example") {
        let entry = entry.expect("entry");
        if entry.file_type().expect("file type").is_file() {
            std::fs::copy(entry.path(), dst.join(entry.file_name())).expect("copy");
        }
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn git");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn install_from_git_monorepo_subdirectory() {
    if !has("git") {
        eprintln!("skipping: git is required");
        return;
    }
    let home = scratch("install-subdir");
    let repo = home.join("collection");
    copy_plugin(&plugin_examples().join("python-sentiment"), &repo.join("sentiment"));
    std::fs::write(repo.join("README.md"), "plugins").expect("readme");
    git(&repo, &["init", "--quiet"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "--quiet", "-m", "fixture"]);
    git(&repo, &["tag", "v1"]);
    let url = format!("file://{}", repo.display());
    let dest = home.join("installed");
    let dest_arg = dest.to_str().unwrap();

    // The repository root has no manifest, so a plain install fails...
    let out = graphrt(&home, &["plugin", "install", &url, "--rev", "v1", "--dir", dest_arg]);
    assert!(!out.status.success());
    // ...and `--subdir` picks the plugin out of the collection.
    let out = graphrt(&home, &["plugin", "install", &url, "--rev", "v1", "--subdir", "sentiment", "--dir", dest_arg]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("installed sentiment"), "{}", stdout(&out));
    assert!(dest.join("sentiment/graphrt-plugin.json").is_file());
    assert!(!dest.join("sentiment/README.md").exists());

    let out = graphrt(&home, &["plugin", "install", &url, "--subdir", "../sentiment", "--dir", dest_arg]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("`..` is not allowed"), "{}", stderr(&out));

    // `--subdir` with a local directory is a usage error.
    let local = repo.join("sentiment");
    let out = graphrt(&home, &["plugin", "install", local.to_str().unwrap(), "--subdir", "x", "--dir", dest_arg]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

#[test]
fn scaffolded_python_plugin_runs() {
    if !has("python3") {
        eprintln!("skipping: python3 is required");
        return;
    }
    let home = scratch("new");
    let out = graphrt(&home, &["plugin", "new", "greeter", "--lang", "python", "--dir", home.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
    let plugin = home.join("greeter");
    assert!(plugin.join("graphrt-plugin.json").is_file());
    let out = graphrt(
        &home,
        &[
            "run",
            plugin.join("example.yaml").to_str().unwrap(),
            "--plugins",
            plugin.to_str().unwrap(),
            "--input",
            "who=World",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("Hello, World!"), "{}", stdout(&out));

    // Refuses to overwrite and rejects bad names.
    let out = graphrt(&home, &["plugin", "new", "greeter", "--dir", home.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    let out = graphrt(&home, &["plugin", "new", "Bad Name", "--dir", home.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn broken_plugin_is_skipped_with_a_warning() {
    let home = scratch("broken");
    let dir = home.join("plugins/broken");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("graphrt-plugin.json"), r#"{"name": "broken", "runtime": "process"}"#).unwrap();
    let plugins = home.join("plugins");
    let out = graphrt(&home, &["nodes", "--plugins", plugins.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("plugin `broken` skipped"), "{}", stderr(&out));
}

#[test]
fn scaffold_templates_match_the_sdks() {
    let pairs = [
        ("crates/graphrt-cli/templates/python/graphrt_plugin.py", "sdk/python/graphrt_plugin.py"),
        ("crates/graphrt-cli/templates/typescript/graphrt-plugin.mjs", "sdk/typescript/index.mjs"),
    ];
    for (template, sdk) in pairs {
        let a = std::fs::read_to_string(root().join(template)).unwrap();
        let b = std::fs::read_to_string(root().join(sdk)).unwrap();
        assert!(a == b, "{template} is out of date: copy {sdk} over it");
    }
}
