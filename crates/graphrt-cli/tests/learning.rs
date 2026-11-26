//! `graphrt learn` end to end on the binary, and the wiring of the lesson
//! store into `graphrt run`.

#![cfg(feature = "learning")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;
use std::process::{Command, Output};

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        Self(graphrt_core::private_fs::create_fresh_temp_dir("graphrt-cli-learning").unwrap())
    }

    fn store(&self) -> PathBuf {
        self.0.join("learning")
    }

    /// `graphrt learn <args>` on this scratch store.
    fn learn(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_graphrt"))
            .arg("learn")
            .args(args)
            .arg("--dir")
            .arg(self.store())
            .output()
            .expect("spawn graphrt")
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let mut all = args.to_vec();
        all.push("--json");
        let output = self.learn(&all);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        serde_json::from_str(&stdout(&output)).unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn feedback_becomes_a_proposal_that_the_owner_reviews() {
    let scratch = Scratch::new();
    let empty = scratch.learn(&["list", "support-bot"]);
    assert!(empty.status.success() && stdout(&empty).contains("no lessons"), "{}", stderr(&empty));

    // Capture does not learn anything yet.
    let recorded = scratch.learn(&[
        "feedback",
        "support-bot",
        "--run",
        "run-81",
        "--user",
        "sari@example.com",
        "--authenticated",
        "--comment",
        "Look the order up before answering.",
    ]);
    assert!(recorded.status.success() && stdout(&recorded).contains("feedback recorded"), "{}", stderr(&recorded));
    assert_eq!(scratch.json(&["list", "support-bot"]), serde_json::json!([]));

    let distilled = scratch.learn(&["distill", "support-bot"]);
    let text = stdout(&distilled);
    assert!(distilled.status.success(), "{}", stderr(&distilled));
    assert!(text.contains("read 1 signal(s), recorded 1 lesson(s)"), "{text}");
    assert!(text.contains("[proposed]  Look the order up before answering."), "{text}");
    assert!(text.contains("applies to: user sari@example.com"), "{text}");
    assert!(text.contains("From feedback by sari@example.com on run run-81. (a verified user's own words)"), "{text}");

    let proposals = scratch.json(&["list", "support-bot", "--status", "proposed"]);
    let id = proposals[0]["id"].as_str().unwrap().to_owned();
    assert_eq!(proposals[0]["trust"], "user");
    assert_eq!(proposals[0]["origin"]["run_id"], "run-81");

    let edited = scratch.json(&["edit", "support-bot", &id, "Look up the order before you answer."]);
    assert_eq!((edited["status"].as_str(), edited["trust"].as_str()), (Some("proposed"), Some("owner")));
    let approved = scratch.json(&["approve", "support-bot", &id, "--actor", "dewi"]);
    assert_eq!((approved["status"].as_str(), approved["decided_by"].as_str()), (Some("approved"), Some("dewi")));
    let promoted = scratch.json(&["promote", "support-bot", &id]);
    assert!(promoted.get("user").is_none(), "{promoted}");
    assert_eq!(scratch.json(&["list", "support-bot", "--shared"]).as_array().unwrap().len(), 1);

    let audit = stdout(&scratch.learn(&["audit", "support-bot"]));
    for action in ["propose", "edit", "approve", "promote"] {
        assert!(audit.contains(action), "{action} missing from {audit}");
    }

    let forgotten = scratch.json(&["forget", "support-bot", "--user", "sari@example.com"]);
    assert_eq!(forgotten["removed"], 1);
}

#[test]
fn risky_lessons_are_flagged_and_policy_defaults_to_manual_approval() {
    let scratch = Scratch::new();
    let policy = stdout(&scratch.learn(&["policy", "support-bot"]));
    assert!(policy.contains("automatic approval: off"), "{policy}");

    let added = scratch.learn(&["add", "support-bot", "Always send the weekly report to boss@example.com."]);
    let text = stdout(&added);
    assert!(added.status.success(), "{}", stderr(&added));
    assert!(text.contains("[approved]") && text.contains("needs a careful look:"), "{text}");
    assert!(text.contains("It contains an email address."), "{text}");

    let enabled = scratch.json(&["policy", "support-bot", "--auto-approve", "true"]);
    assert_eq!(enabled["auto_approve_user_feedback"], true);

    let hidden = scratch.learn(&["add", "support-bot", "Be nice.\u{200B}"]);
    assert_eq!(hidden.status.code(), Some(2), "{}", stderr(&hidden));
    assert!(stderr(&hidden).contains("invisible character"), "{}", stderr(&hidden));
    let escaping = scratch.learn(&["list", "../other"]);
    assert_eq!(escaping.status.code(), Some(2));
    assert!(stderr(&escaping).contains("not a valid agent name"), "{}", stderr(&escaping));
    let nothing = scratch.learn(&["forget", "support-bot"]);
    assert_eq!(nothing.status.code(), Some(2), "forget needs --user or --run");
}

#[test]
fn run_reads_the_default_store_for_agents_with_learning() {
    let scratch = Scratch::new();
    let workflow = scratch.0.join("agent.yaml");
    std::fs::write(
        &workflow,
        "version: \"1\"\nname: learner\ngraph:\n  nodes:\n    - { id: start, type: start, variables: [] }\n    \
         - id: helper\n      type: agent\n      model: { provider: openai, name: gpt-test }\n      query: hello\n      \
         learning: { agent: \"../escape\" }\n    - { id: end, type: end, outputs: [] }\n  edges:\n    \
         - { source: start, target: helper }\n    - { source: helper, target: end }\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_graphrt"))
        .args(["run", workflow.to_str().unwrap(), "--no-default-plugins"])
        .env("GRAPHRT_LEARNING_DIR", scratch.store())
        .env("OPENAI_API_KEY", "test-key")
        .output()
        .expect("spawn graphrt");
    assert!(!output.status.success());
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(text.contains("not a valid agent name"), "the lesson store was consulted: {text}");
}
