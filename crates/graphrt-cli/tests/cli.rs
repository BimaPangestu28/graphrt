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
        names.extend(["chatbot.yaml", "research-agent.yaml", "supabase-chatbot/agent.yaml"]);
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
    assert!(stdout(&out).contains("not available: missing plugin or build feature"), "{}", stdout(&out));
}

#[test]
fn resume_input_requires_snapshot() {
    let path = example("approval.yaml");
    let out = run(&["run", path.to_str().expect("utf8"), "--resume-input", "review={}"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn nodes_lists_the_catalog() {
    let out = run(&["nodes"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("if-else"));
    let out = run(&["nodes", "--json"]);
    let json: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    assert!(json.as_array().expect("array").iter().any(|d| d["kind"] == "start"));
    let out = run(&["nodes", "--kind", "loop", "--json"]);
    let json: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    assert_eq!(json["container"], true);
    let out = run(&["nodes", "--kind", "nope"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
#[cfg(feature = "python")]
fn python_code_requires_explicit_opt_in() {
    let wf = temp_path("python.yaml");
    std::fs::write(
        &wf,
        "graph:\n  nodes:\n    - {id: start, type: start, variables: [{variable: n, type: number, required: true}]}\n    \
         - id: py\n      type: code\n      code_language: python3\n      code: \"def main(n):\\n    return {'sq': n * n}\"\n      \
         variables: [{variable: n, value_selector: [start, n]}]\n    \
         - {id: end, type: end, outputs: [{variable: sq, value_selector: [py, sq]}]}\n  \
         edges: [{source: start, target: py}, {source: py, target: end}]\n",
    )
    .expect("write workflow");
    let wf = wf.to_str().expect("utf8");
    let out = run(&["run", wf, "--input", "n:=7", "--output", "json"]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(stdout(&out).contains("sandbox"), "{}", stdout(&out));

    let out = run(&["run", wf, "--input", "n:=7", "--output", "json", "--allow-python-subprocess"]);
    assert!(out.status.success(), "{} {}", stdout(&out), stderr(&out));
    let json: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("json");
    assert_eq!(json["outputs"]["sq"], 49);
}

#[test]
#[cfg(feature = "llm")]
fn providers_file_errors_are_reported() {
    let providers = temp_path("providers.yaml");
    std::fs::write(
        &providers,
        "providers:\n  - name: x\n    kind: openai\n    api_key_env: GRAPHRT_TEST_MISSING_KEY\n",
    )
    .expect("write providers");
    let out = run(&[
        "run",
        example("hello.yaml").to_str().expect("utf8"),
        "--input",
        "name=A",
        "--providers",
        providers.to_str().expect("utf8"),
    ]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("GRAPHRT_TEST_MISSING_KEY"), "{}", stderr(&out));
}

#[test]
#[cfg(feature = "llm")]
fn model_override_is_validated() {
    let out = run(&["run", example("chatbot.yaml").to_str().expect("utf8"), "--model", "no-slash"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("provider/name"), "{}", stderr(&out));
}

/// The chatbot example's start inputs: a bounded message and the JWT as a secret
/// (required by `--input-header` / `--verify-jwt`).
#[cfg(feature = "llm")]
#[test]
fn supabase_chatbot_declares_its_contract() {
    let path = example("supabase-chatbot/agent.yaml");
    let out = run(&["inspect", path.to_str().expect("utf8")]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    for expected in ["message", "paragraph", "session_id", "user_jwt", "secret", "status: valid"] {
        assert!(text.contains(expected), "missing `{expected}` in:\n{text}");
    }
}

/// User tokens only open run starts and feedback, so a `--verify-jwt` server must
/// have an API key closing everything else, on loopback too: a same-host reverse
/// proxy would make a keyless loopback server public.
#[cfg(feature = "jwt")]
#[test]
fn verify_jwt_needs_an_api_key_on_every_address() {
    let path = example("supabase-chatbot/agent.yaml");
    for addr in ["0.0.0.0:0", "127.0.0.1:0"] {
        let out = bin()
            .args(["serve", path.to_str().expect("utf8"), "--addr", addr, "--no-default-plugins"])
            .args(["--input-header", "user_jwt=X-User-Authorization", "--verify-jwt", "user_jwt"])
            .args(["--jwt-secret-env", "TEST_JWT_SECRET"])
            .env("TEST_JWT_SECRET", "a-secret-for-the-startup-check-only")
            .env_remove("GRAPHRT_API_KEY")
            .output()
            .expect("spawn graphrt");
        assert_eq!(out.status.code(), Some(2), "{addr}: {}", stderr(&out));
        assert!(stderr(&out).contains("needs $GRAPHRT_API_KEY"), "{addr}: {}", stderr(&out));
    }
}

#[test]
fn serve_refuses_an_invalid_deployment_name() {
    let path = example("hello.yaml");
    let out = bin()
        .args(["serve", path.to_str().expect("utf8"), "--addr", "127.0.0.1:0", "--no-default-plugins"])
        .args(["--deployment", "shop/eu"])
        .output()
        .expect("spawn graphrt");
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("invalid deployment name `shop/eu`"), "{}", stderr(&out));
}

#[cfg(feature = "plugins")]
#[test]
fn serve_refuses_a_plugin_idle_timeout_without_a_pool_and_an_oversized_pool() {
    let path = example("hello.yaml");
    let serve = |extra: &[&str]| {
        bin()
            .args(["serve", path.to_str().expect("utf8"), "--addr", "127.0.0.1:0", "--no-default-plugins"])
            .args(extra)
            .output()
            .expect("spawn graphrt")
    };
    let out = serve(&["--plugin-idle", "30s"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("`--plugin-idle` needs `--plugin-pool 1` or more"), "{}", stderr(&out));
    let out = serve(&["--plugin-pool", "33"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("--plugin-pool"), "{}", stderr(&out));
}
