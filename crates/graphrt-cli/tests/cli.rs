//! End-to-end tests of the `graphrt` binary on the offline examples.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;
use std::process::{Command, Output};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_graphrt"))
}

fn example(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples").join(name)
}

fn run(args: &[&str]) -> Output {
    bin().args(args).output().expect("spawn graphrt")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn temp_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graphrt-cli-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join(name)
}

#[test]
fn version_lists_features() {
    let out = run(&["version"]);
    assert!(out.status.success());
    assert!(stdout(&out).starts_with("graphrt "));
}

#[test]
fn validate_all_examples() {
    let mut names = vec!["hello.yaml", "branching.yaml", "approval.yaml"];
    if cfg!(feature = "code") {
        names.push("iteration.yaml");
    }
    if cfg!(feature = "http") {
        names.push("http.yaml");
    }
    if cfg!(feature = "llm") {
        names.extend(["chatbot.yaml", "research-agent.yaml"]);
    }
    for name in names {
        let path = example(name);
        let out = run(&["validate", path.to_str().expect("utf8 path")]);
        assert!(out.status.success(), "{name}: {}", stderr(&out));
    }
}

#[test]
fn validate_rejects_invalid_workflow() {
    let path = temp_path("bad.yaml");
    std::fs::write(
        &path,
        "graph:\n  nodes:\n    - {id: a, type: start}\n  edges:\n    - {source: a, target: missing}\n",
    )
    .expect("write");
    let out = run(&["validate", path.to_str().expect("utf8")]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("missing"), "{}", stderr(&out));
}

#[test]
fn inspect_shows_inputs() {
    let path = example("hello.yaml");
    let out = run(&["inspect", path.to_str().expect("utf8")]);
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("name") && text.contains("required"), "{text}");
    assert!(text.contains("status: valid"), "{text}");
}

#[test]
fn run_hello_json() {
    let path = example("hello.yaml");
    let out =
        run(&["run", path.to_str().expect("utf8"), "--input", "name=Ada", "--input", "excited:=true", "-o", "json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json output");
    assert_eq!(json["status"], "succeeded");
    assert_eq!(json["outputs"]["greeting"], "Hello, Ada!!!");
}

#[test]
fn run_missing_required_input_fails() {
    let path = example("hello.yaml");
    let out = run(&["run", path.to_str().expect("utf8")]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("name"), "{}", stderr(&out));
}

#[test]
fn run_branching_text_streams_answer() {
    let path = example("branching.yaml");
    let out = run(&["run", path.to_str().expect("utf8"), "--input", "priority:=9", "--input", "subject=DB down"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out).trim(), "URGENT: paging on-call for 'DB down' (priority 9)");

    let out = run(&["run", path.to_str().expect("utf8"), "--input", "priority=5", "--input", "subject=Typo"]);
    assert_eq!(stdout(&out).trim(), "Queued 'Typo' for the support team (priority 5)");
}

#[test]
fn run_events_is_ndjson() {
    let path = example("hello.yaml");
    let out = run(&["run", path.to_str().expect("utf8"), "--input", "name=Ada", "-o", "events"]);
    assert!(out.status.success());
    let types: Vec<String> = stdout(&out)
        .lines()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).expect("ndjson line")["type"].as_str().unwrap_or("").to_owned()
        })
        .collect();
    assert_eq!(types.first().map(String::as_str), Some("run_started"));
    assert_eq!(types.last().map(String::as_str), Some("run_succeeded"));
    assert!(types.iter().any(|t| t == "node_succeeded"));
}

#[test]
#[cfg(feature = "code")]
fn run_iteration_with_code() {
    let path = example("iteration.yaml");
    let out = run(&[
        "run",
        path.to_str().expect("utf8"),
        "--input",
        r#"orders=[{"sku":"A","qty":2,"price":3.5},{"sku":"B","qty":1,"price":10}]"#,
        "-o",
        "json",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(json["outputs"]["lines"][1]["total"], 10);
    assert!(json["outputs"]["summary"].as_str().unwrap_or_default().contains("Grand total: 17"));
}

#[test]
fn run_approval_pauses_and_writes_snapshot() {
    let path = example("approval.yaml");
    let snap = temp_path("approval-pause.snapshot.json");
    let out = run(&[
        "run",
        path.to_str().expect("utf8"),
        "--input",
        "amount:=1200",
        "--input",
        "reason=Laptop",
        "--snapshot-out",
        snap.to_str().expect("utf8"),
    ]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(stderr(&out).contains("human_input"));
    let snapshot: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&snap).expect("snapshot")).expect("json");
    assert_eq!(snapshot["root"]["nodes"]["review"]["status"], "paused");
}

#[test]
fn run_approval_resume_completes() {
    let path = example("approval.yaml");
    let snap = temp_path("approval-resume.snapshot.json");
    let wf = path.to_str().expect("utf8");
    let snap_s = snap.to_str().expect("utf8");
    let out = run(&["run", wf, "--input", "amount:=50", "--input", "reason=Lunch", "--snapshot-out", snap_s]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));

    let out = run(&[
        "run",
        wf,
        "--resume",
        snap_s,
        "--resume-input",
        r#"review={"action":"approve","values":{"note":"fine"}}"#,
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out).trim(), "Approved 50 (fine)");
}

#[test]
#[cfg(not(feature = "llm"))]
fn inspect_reports_missing_node_kinds() {
    let path = example("chatbot.yaml");
    let out = run(&["inspect", path.to_str().expect("utf8")]);
    assert!(out.status.success());
    assert!(stdout(&out).contains("not available in this build"), "{}", stdout(&out));
}

#[test]
fn resume_input_requires_snapshot() {
    let path = example("approval.yaml");
    let out = run(&["run", path.to_str().expect("utf8"), "--resume-input", "review={}"]);
    assert_eq!(out.status.code(), Some(2));
}
