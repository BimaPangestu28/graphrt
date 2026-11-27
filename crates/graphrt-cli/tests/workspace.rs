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

/// `--image-model gemini/NAME`: the Gemini generator behind `image/generate`,
/// against a local server that answers like the Gemini API.
#[cfg(feature = "image")]
#[test]
fn a_gemini_image_model_generates_into_the_workspace() {
    use std::io::{Read, Write};
    const PNG_1X1: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    let dir = temp_dir("gemini");
    let workflow = dir.join("draw.yaml");
    std::fs::write(
        &workflow,
        r#"version: "1"
name: draw
graph:
  nodes:
    - { id: start, type: start }
    - id: draw
      type: tool
      provider_id: image
      tool_name: generate
      tool_parameters:
        prompt: { type: constant, value: "a red bicycle" }
        size: { type: constant, value: "1024x1024" }
        output: { type: constant, value: "generated/bike.png" }
    - id: end
      type: end
      outputs:
        - { variable: text, value_selector: [draw, text] }
        - { variable: files, value_selector: [draw, files] }
  edges:
    - { source: start, target: draw }
    - { source: draw, target: end }
"#,
    )
    .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}/v1beta", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let read = socket.read(&mut buffer).unwrap_or(0);
            request.extend_from_slice(&buffer[..read]);
            let text = String::from_utf8_lossy(&request).into_owned();
            let complete = text.split_once("\r\n\r\n").is_some_and(|(head, body)| {
                let length = head.lines().find_map(|line| {
                    line.to_ascii_lowercase().strip_prefix("content-length:")?.trim().parse::<usize>().ok()
                });
                body.len() >= length.unwrap_or(0)
            });
            if read == 0 || complete {
                break;
            }
        }
        let body = format!(
            r#"{{"candidates":[{{"content":{{"parts":[{{"inlineData":{{"mimeType":"image/png","data":"{PNG_1X1}"}}}}]}}}}]}}"#
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).unwrap();
        String::from_utf8_lossy(&request).into_owned()
    });
    let run = |env: &[(&str, &str)]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_graphrt"));
        command.args([
            "run",
            workflow.to_str().unwrap(),
            "-o",
            "json",
            "--workspace",
            dir.join("ws").to_str().unwrap(),
        ]);
        command.args(["--image-tools", "--image-model", "gemini/gemini-3.1-flash-image"]);
        command.env_remove("GEMINI_API_KEY").env_remove("GOOGLE_API_KEY").envs(env.iter().copied());
        command.output().unwrap()
    };
    let out = run(&[("GEMINI_API_KEY", "test-key-for-the-fake"), ("GEMINI_BASE_URL", &base_url)]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(json["outputs"]["text"].as_str().unwrap().starts_with("Generated: generated/bike.png (image/png, 1x1"));
    assert_eq!(json["outputs"]["files"][0]["reference"], "workspace:generated/bike.png");
    assert!(std::fs::read(dir.join("ws/generated/bike.png")).unwrap().starts_with(b"\x89PNG"));
    let request = server.join().unwrap();
    assert!(request.starts_with("POST /v1beta/models/gemini-3.1-flash-image:generateContent HTTP/1.1"), "{request}");
    assert!(request.to_ascii_lowercase().contains("x-goog-api-key: test-key-for-the-fake"));

    // Without a key the server does not start a run at all.
    let out = run(&[]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("GEMINI_API_KEY"));
    let _ = std::fs::remove_dir_all(&dir);
}
