//! `--workspace`: the `files` tools through the real binary.

#![cfg(feature = "files")]
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples").join(name)
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graphrt-workspace-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run_files(workspace: &Path, title: &str, extra: &[&str]) -> Output {
    let workflow = example("files.yaml");
    let title = format!("title={title}");
    let mut args = vec!["run", workflow.to_str().unwrap(), "--input", &title, "--input", "body=pear,7", "-o", "json"];
    let workspace = workspace.to_str().unwrap();
    args.extend(["--workspace", workspace]);
    args.extend(extra);
    Command::new(env!("CARGO_BIN_EXE_graphrt")).args(args).output().unwrap()
}

#[test]
fn a_workflow_writes_searches_and_reads_in_its_workspace() {
    let dir = temp_dir("roundtrip");
    let workspace = dir.join("ws");
    let out = run_files(&workspace, "Stock", &[]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["outputs"]["written"], "Wrote notes/Stock.md (16 B, text/markdown).");
    assert_eq!(json["outputs"]["files"][0]["reference"], "workspace:notes/Stock.md");
    assert_eq!(json["outputs"]["files"][0]["mime_type"], "text/markdown");
    assert!(json["outputs"]["found"].as_str().unwrap().ends_with("notes/Stock.md:1: # Stock"));
    assert!(json["outputs"]["content"].as_str().unwrap().contains("\n# Stock\n\npear,7\n"));
    assert_eq!(std::fs::read_to_string(workspace.join("notes/Stock.md")).unwrap(), "# Stock\n\npear,7\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_input_cannot_steer_a_write_out_of_the_workspace() {
    let dir = temp_dir("escape");
    let workspace = dir.join("ws");
    // The workflow builds the path from this input: `notes/../../escaped.md`.
    let out = run_files(&workspace, "../../escaped", &[]);
    assert!(!out.status.success());
    let report = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(report.contains("invalid path"), "{report}");
    assert!(!dir.join("escaped.md").exists());
    assert!(!dir.parent().unwrap().join("escaped.md").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_read_only_workspace_refuses_the_write() {
    let dir = temp_dir("readonly");
    let out = run_files(&dir, "Stock", &["--workspace-read-only"]);
    assert!(!out.status.success());
    assert!(!dir.join("notes").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn without_a_workspace_there_are_no_file_tools() {
    let workflow = example("files.yaml");
    let args = ["run", workflow.to_str().unwrap(), "--input", "title=Stock", "--input", "body=x", "-o", "json"];
    let out = Command::new(env!("CARGO_BIN_EXE_graphrt")).args(args).output().unwrap();
    assert!(!out.status.success());
    let report = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(report.contains("unknown tool provider `files`"), "{report}");
}

#[test]
fn per_user_needs_a_scope_and_modifiers_need_a_workspace() {
    let dir = temp_dir("peruser");
    // The example sends no `tool_context`, so a per-user workspace refuses it.
    let out = run_files(&dir, "Stock", &["--workspace-per-user"]);
    assert!(!out.status.success());
    let report = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(report.contains("this workspace is per user"), "{report}");
    assert!(!dir.join("notes").exists());

    let workflow = example("files.yaml");
    let out = Command::new(env!("CARGO_BIN_EXE_graphrt"))
        .args(["run", workflow.to_str().unwrap(), "--workspace-read-only"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--workspace"));
    let _ = std::fs::remove_dir_all(&dir);
}
