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

/// A 1x1 PNG with a text chunk appended by hand would need a CRC, so the
/// metadata checks live in graphrt-image. Here: the tools through the binary.
#[cfg(feature = "image")]
#[test]
fn image_tools_inspect_and_thumbnail_a_workspace_image() {
    const PNG_1X1: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    let dir = temp_dir("images");
    std::fs::write(dir.join("dot.png"), graphrt_core::file::base64_decode(PNG_1X1).unwrap()).unwrap();
    let workflow = example("images.yaml");
    let run = |extra: &[&str]| {
        let mut args = vec!["run", workflow.to_str().unwrap(), "--input", "path=dot.png", "-o", "json"];
        args.extend(["--workspace", dir.to_str().unwrap()]);
        args.extend(extra);
        Command::new(env!("CARGO_BIN_EXE_graphrt")).args(args).output().unwrap()
    };

    let out = run(&["--image-tools"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["outputs"]["info"]["mime_type"], "image/png");
    assert_eq!(json["outputs"]["info"]["width"], 1);
    assert_eq!(json["outputs"]["files"][0]["reference"], "workspace:dot-thumb.png");
    assert_eq!(json["outputs"]["files"][0]["kind"], "image");
    let thumbnail = std::fs::read(dir.join("dot-thumb.png")).unwrap();
    assert!(thumbnail.starts_with(b"\x89PNG\r\n\x1a\n"));

    // Without `--image-tools` a workspace has file tools only.
    let out = run(&[]);
    assert!(!out.status.success());
    let report = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(report.contains("unknown tool provider `image`"), "{report}");

    // A file that is not an image is refused whatever it is called.
    std::fs::write(dir.join("dot.png"), b"<svg xmlns='http://www.w3.org/2000/svg'/>").unwrap();
    let out = run(&["--image-tools"]);
    assert!(!out.status.success());
    let report = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(report.contains("unsupported image"), "{report}");

    // `--image-model` needs a key, and both flags need a workspace.
    let out = Command::new(env!("CARGO_BIN_EXE_graphrt"))
        .args(["run", workflow.to_str().unwrap(), "--input", "path=dot.png"])
        .args(["--workspace", dir.to_str().unwrap(), "--image-tools", "--image-model", "gpt-image-2"])
        .env_remove("OPENAI_API_KEY")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("OPENAI_API_KEY"));
    let out = Command::new(env!("CARGO_BIN_EXE_graphrt"))
        .args(["run", workflow.to_str().unwrap(), "--image-tools"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let _ = std::fs::remove_dir_all(&dir);
}

/// `--workspace s3://...` is a build option, and a bad location is a usage error
/// before any request is made.
#[test]
fn an_object_storage_workspace_needs_its_feature_and_a_valid_location() {
    let workflow = example("files.yaml");
    let run = |location: &str, env: &[(&str, &str)]| {
        let args =
            ["run", workflow.to_str().unwrap(), "--workspace", location, "--input", "title=a", "--input", "body=b"];
        let mut command = Command::new(env!("CARGO_BIN_EXE_graphrt"));
        command.args(args).env("AWS_ACCESS_KEY_ID", "test").env("AWS_SECRET_ACCESS_KEY", "test");
        command.env_remove("GRAPHRT_S3_ALLOW_HTTP").env_remove("GRAPHRT_S3_ENDPOINT").envs(env.iter().copied());
        let out = command.output().unwrap();
        (out.status.code(), String::from_utf8_lossy(&out.stderr).into_owned())
    };
    #[cfg(not(feature = "files-s3"))]
    {
        let (code, report) = run("s3://my-bucket/agents", &[]);
        assert_eq!(code, Some(2), "{report}");
        assert!(report.contains("requires a build with the `files-s3` feature"), "{report}");
    }
    #[cfg(feature = "files-s3")]
    {
        let (code, report) = run("s3://Not_A_Bucket/agents", &[]);
        assert_eq!(code, Some(2), "{report}");
        assert!(report.contains("not a valid bucket name"), "{report}");
        let (code, report) = run("s3://my-bucket/a/../b", &[]);
        assert_eq!(code, Some(2), "{report}");
        assert!(report.contains("prefix"), "{report}");
        // An unencrypted endpoint is refused unless it is allowed by name.
        let (code, report) = run("s3://my-bucket/agents", &[("GRAPHRT_S3_ENDPOINT", "http://127.0.0.1:9")]);
        assert_eq!(code, Some(2), "{report}");
        assert!(report.contains("unencrypted"), "{report}");
        let (code, report) = run("s3://my-bucket", &[("GRAPHRT_S3_PATH_STYLE", "maybe")]);
        assert_eq!(code, Some(2), "{report}");
        assert!(report.contains("GRAPHRT_S3_PATH_STYLE"), "{report}");
    }
/// `--document-tools`: a workflow writes a PDF, a Word document and a workbook.
#[cfg(feature = "documents")]
#[test]
fn document_tools_write_pdf_word_and_excel_files() {
    let dir = temp_dir("documents");
    let workflow = example("documents.yaml");
    let run = |extra: &[&str]| {
        let mut args = vec!["run", workflow.to_str().unwrap(), "--input", "title=Sales report", "-o", "json"];
        args.extend(["--workspace", dir.to_str().unwrap()]);
        args.extend(extra);
        Command::new(env!("CARGO_BIN_EXE_graphrt")).args(args).output().unwrap()
    };
    let out = run(&["--document-tools"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        json["outputs"]["pdf"].as_str().unwrap().starts_with("Created reports/report.pdf (application/pdf, 1 page")
    );
    assert_eq!(json["outputs"]["files"][0]["reference"], "workspace:reports/report.pdf");
    assert_eq!(json["outputs"]["files"][0]["mime_type"], "application/pdf");
    assert!(std::fs::read(dir.join("reports/report.pdf")).unwrap().starts_with(b"%PDF-1.4"));
    assert!(std::fs::read(dir.join("reports/report.docx")).unwrap().starts_with(b"PK"));
    assert!(std::fs::read(dir.join("reports/revenue.xlsx")).unwrap().starts_with(b"PK"));

    // Without the flag a workspace has file tools only.
    let out = run(&[]);
    assert!(!out.status.success());
    let report = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(report.contains("unknown tool provider `documents`"), "{report}");
    let _ = std::fs::remove_dir_all(&dir);
}
