//! `graphrt eval` end to end: the offline examples under `examples/evals`,
//! JSON / JUnit output, gate exit codes and replay cassette misses.
#![cfg(feature = "eval")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graphrt-eval-cli-{name}-{}", std::process::id()));
    let _fresh = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Run `graphrt eval <args>` from the repository root, without ambient config.
fn graphrt_eval(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_graphrt"))
        .arg("eval")
        .args(args)
        .arg("--no-default-plugins")
        .current_dir(repo())
        .env_remove("GRAPHRT_PROVIDERS")
        .env_remove("GRAPHRT_REPLAY_MODE")
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .env_remove("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
        .env("GRAPHRT_TELEMETRY", "0")
        .output()
        .expect("spawn graphrt")
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn has(program: &str) -> bool {
    Command::new(program).arg("--version").output().is_ok_and(|o| o.status.success())
}

#[test]
fn offline_examples_pass_and_write_reports() {
    let dir = scratch("examples");
    for example in ["hello", "branching", "retrieval/retrieval", "judged/judged"] {
        let file = format!("examples/evals/{example}.eval.yaml");
        let stem = example.replace('/', "-");
        let json = dir.join(format!("{stem}.json"));
        let junit = dir.join(format!("{stem}.xml"));
        let out = graphrt_eval(&[&file, "-o", json.to_str().unwrap(), "--junit", junit.to_str().unwrap()]);
        assert_eq!(out.status.code(), Some(0), "{file}: {}", stderr(&out));
        assert!(stderr(&out).contains("gate: PASSED"), "{}", stderr(&out));

        let report: serde_json::Value = serde_json::from_slice(&std::fs::read(&json).unwrap()).unwrap();
        assert_eq!(report["schema_version"], 2);
        assert_eq!(report["summary"]["gate"]["passed"], true);
        let total = report["summary"]["total"].as_u64().unwrap();
        assert!(total >= 3, "{example}: {total}");
        assert_eq!(report["tests"].as_array().unwrap().len() as u64, total);
        let xml = std::fs::read_to_string(&junit).unwrap();
        assert_eq!(xml.matches("<testcase ").count() as u64, total, "{xml}");
        assert!(xml.contains("failures=\"0\" errors=\"0\""), "{xml}");
    }
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("retrieval-retrieval.json")).unwrap()).unwrap();
    assert!(!report["tests"][0]["run"]["retrieval"].as_array().unwrap().is_empty());
    assert_eq!(report["summary"]["metrics"]["recall@1"]["mean_score"], 1.0);
    let _cleanup = std::fs::remove_dir_all(&dir);
}

#[test]
fn agent_trajectories_replay_from_cassettes() {
    if !has("python3") {
        eprintln!("skipping: python3 is required for the example tool plugin");
        return;
    }
    let dir = scratch("agent");
    let json = dir.join("agent.json");
    let out = graphrt_eval(&["examples/evals/agent/agent.eval.yaml", "-o", json.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let report: serde_json::Value = serde_json::from_slice(&std::fs::read(&json).unwrap()).unwrap();
    let refund = report["tests"].as_array().unwrap().iter().find(|t| t["id"] == "refund-on-request").unwrap();
    let calls: Vec<&str> =
        refund["run"]["tool_calls"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(calls, ["search_orders", "refund_order"]);
    assert_eq!(refund["run"]["tool_calls"][0]["provider"], "orders");
    assert!(refund["run"]["usage"]["total_tokens"].as_u64().unwrap() > 0, "usage comes from the cassettes");

    // The same cassettes serve `graphrt run` through the providers file.
    let run = Command::new(env!("CARGO_BIN_EXE_graphrt"))
        .args(["run", "examples/evals/agent/support-agent.yaml", "-o", "json", "--no-default-plugins"])
        .args(["--providers", "examples/evals/agent/providers.replay.yaml"])
        .args(["--plugins", "examples/evals/agent/plugins", "--input", "question=Where is order 123?"])
        .current_dir(repo())
        .env_remove("GRAPHRT_REPLAY_MODE")
        .env("GRAPHRT_TELEMETRY", "0")
        .output()
        .unwrap();
    assert!(run.status.success(), "{}", stderr(&run));
    assert!(String::from_utf8_lossy(&run.stdout).contains("shipped"));

    // A question nobody recorded is a clear replay miss, reported as a run error.
    let eval_file = dir.join("miss.eval.yaml");
    let agent_dir = repo().join("examples/evals/agent");
    std::fs::write(
        &eval_file,
        format!(
            "workflow: {0}/support-agent.yaml\nproviders: {0}/providers.replay.yaml\nplugins: [{0}/plugins]\n\
             tests:\n  - id: unrecorded\n    vars: {{ question: \"Cancel order 999\" }}\n    assert: [{{ type: contains, value: x }}]\n",
            agent_dir.display()
        ),
    )
    .unwrap();
    let out = graphrt_eval(&[eval_file.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(5), "{}", stderr(&out));
    let text = stderr(&out);
    assert!(text.contains("ERROR") && text.contains("no cassette for this request"), "{text}");
    let _cleanup = std::fs::remove_dir_all(&dir);
}

fn write_failing_suite(dir: &Path) -> PathBuf {
    let file = dir.join("failing.eval.yaml");
    std::fs::write(
        &file,
        format!(
            "workflow: {}\ntests:\n  - id: ok\n    vars: {{ name: Ada }}\n    assert: [{{ type: equals, value: \"Hello, Ada.\" }}]\n  \
             - id: wrong\n    tags: [broken]\n    vars: {{ name: Bob }}\n    assert: [{{ type: contains, value: Alice }}]\n",
            repo().join("examples/hello.yaml").display()
        ),
    )
    .unwrap();
    file
}

#[test]
fn gate_failures_exit_with_a_configurable_code() {
    let dir = scratch("gate");
    let file = write_failing_suite(&dir);
    let file = file.to_str().unwrap();
    let junit = dir.join("junit.xml");

    let out = graphrt_eval(&[file, "--junit", junit.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(5), "{}", stderr(&out));
    assert!(stderr(&out).contains("FAIL  wrong") && stderr(&out).contains("gate: FAILED"), "{}", stderr(&out));
    let xml = std::fs::read_to_string(&junit).unwrap();
    assert!(xml.contains("<failure message=\"contains: &quot;Alice&quot; not found"), "{xml}");

    assert_eq!(graphrt_eval(&[file, "--fail-exit-code", "7"]).status.code(), Some(7));
    assert_eq!(graphrt_eval(&[file, "--pass-rate", "0.5"]).status.code(), Some(0));
    let filtered = graphrt_eval(&[file, "--filter", "ok", "--repeat", "2", "-j", "1"]);
    assert_eq!(filtered.status.code(), Some(0), "{}", stderr(&filtered));
    assert!(stderr(&filtered).contains("ok#1") && !stderr(&filtered).contains("wrong"), "{}", stderr(&filtered));
    let _cleanup = std::fs::remove_dir_all(&dir);
}

#[test]
fn invalid_eval_files_exit_with_usage_errors() {
    let dir = scratch("invalid");
    let file = dir.join("bad.eval.yaml");
    std::fs::write(&file, "workflow: nope.yaml\ntests:\n  - assert: [{ type: no-such-check }]\n").unwrap();
    let out = graphrt_eval(&[file.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));

    let hello = repo().join("examples/hello.yaml");
    std::fs::write(&file, format!("workflow: {}\ntests:\n  - assert: [{{ type: no-such-check }}]\n", hello.display()))
        .unwrap();
    let out = graphrt_eval(&[file.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("unknown assertion type `no-such-check`"), "{}", stderr(&out));
    assert_eq!(graphrt_eval(&["examples/evals/missing.eval.yaml"]).status.code(), Some(2));
    let _cleanup = std::fs::remove_dir_all(&dir);
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[test]
fn judges_and_trials_replay_offline() {
    let dir = scratch("judged");
    let json = dir.join("judged.json");
    let out = graphrt_eval(&["examples/evals/judged/judged.eval.yaml", "--trials", "3", "-o", json.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stderr(&out);
    assert!(text.contains("x 3 trials") && text.contains("pass^3 1.000"), "{text}");
    let report = read_json(&json);
    assert_eq!(report["tests"].as_array().unwrap().len(), 12);
    assert_eq!(report["tests"][2]["trial"], 2);
    assert_eq!(report["cases"].as_array().unwrap().len(), 4);
    assert_eq!(report["cases"][0]["pass_hat_k"], 1.0);
    assert_eq!(report["summary"]["trials"]["k"], 3);
    assert!(report["summary"]["judge_usage"]["total_tokens"].as_u64().unwrap() > 0);
    let faithfulness = &report["summary"]["metrics"]["faithfulness"];
    assert_eq!((faithfulness["count"].as_u64(), faithfulness["excluded"].as_u64()), (Some(9), Some(3)));
    let judge = &report["tests"][0]["assertions"][2]["judge"];
    assert_eq!(judge["workflow"], "examples/evals/judges/faithfulness.yaml");
    assert_eq!(judge["verdict"], "pass");
    assert_eq!(judge["fingerprint"].as_str().unwrap().len(), 16);

    // The results are the next run's baseline: nothing regressed.
    let again = graphrt_eval(&["examples/evals/judged/judged.eval.yaml", "--baseline", json.to_str().unwrap()]);
    assert_eq!(again.status.code(), Some(0), "{}", stderr(&again));
    assert!(stderr(&again).contains("0 regression(s), 0 improvement(s)"), "{}", stderr(&again));

    // `--judge-model` points the judges (only) at a model nobody recorded.
    let other = graphrt_eval(&["examples/evals/judged/judged.eval.yaml", "--judge-model", "openai/gpt-unrecorded"]);
    assert_eq!(other.status.code(), Some(5), "{}", stderr(&other));
    let text = stderr(&other);
    assert!(text.contains("judge run did not succeed") && text.contains("no cassette"), "{text}");
    assert!(!text.contains("ERROR"), "the workflow under test still replays: {text}");
    let _cleanup = std::fs::remove_dir_all(&dir);
}

#[test]
fn calibration_reports_agreement_with_labels() {
    let dir = scratch("calibration");
    let json = dir.join("calibration.json");
    let out = graphrt_eval(&["examples/evals/calibration/faithfulness.eval.yaml", "-o", json.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stderr(&out);
    assert!(text.contains("agreement 0.778  TPR 0.800  TNR 0.750  kappa 0.550"), "{text}");
    let c = &read_json(&json)["calibration"];
    assert_eq!((c["labelled"].as_u64(), c["judged"].as_u64(), c["unknown"].as_u64()), (Some(10), Some(9), Some(1)));
    assert_eq!(c["confusion"], serde_json::json!({"tp": 4, "fp": 1, "tn": 3, "fn": 1}));

    // A stricter kappa threshold fails the gate.
    let calibration = repo().join("examples/evals/calibration");
    let original = std::fs::read_to_string(calibration.join("faithfulness.eval.yaml")).unwrap();
    let rewritten = original
        .replace("min_kappa: 0.5", "min_kappa: 0.9")
        .replace("workflow: ../", &format!("workflow: {}/../", calibration.display()))
        .replace("providers: ", &format!("providers: {}/", calibration.display()))
        .replace("dataset: ", &format!("dataset: {}/", calibration.display()));
    let strict = dir.join("strict.eval.yaml");
    std::fs::write(&strict, rewritten).unwrap();
    let out = graphrt_eval(&[strict.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(5), "{}", stderr(&out));
    assert!(stderr(&out).contains("gate: FAILED (kappa 0.550 < 0.900)"), "{}", stderr(&out));
    let _cleanup = std::fs::remove_dir_all(&dir);
}

#[test]
fn baseline_regressions_fail_the_gate() {
    let dir = scratch("baseline");
    let file = write_failing_suite(&dir);
    let file = file.to_str().unwrap();
    let json = dir.join("base.json");
    let out = graphrt_eval(&[file, "--pass-rate", "0", "-o", json.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    // Pretend `wrong` passed last time.
    let mut baseline = read_json(&json);
    let wrong = baseline["tests"].as_array_mut().unwrap().iter_mut().find(|t| t["id"] == "wrong").unwrap();
    wrong["status"] = "passed".into();
    std::fs::write(&json, serde_json::to_string(&baseline).unwrap()).unwrap();

    let out = graphrt_eval(&[file, "--pass-rate", "0", "--baseline", json.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(5), "{}", stderr(&out));
    let text = stderr(&out);
    assert!(text.contains("REGRESSED test wrong") && text.contains("1 regression(s) vs the baseline"), "{text}");

    let missing = graphrt_eval(&[file, "--baseline", dir.join("nope.json").to_str().unwrap()]);
    assert_eq!(missing.status.code(), Some(2), "{}", stderr(&missing));
    let _cleanup = std::fs::remove_dir_all(&dir);
}
