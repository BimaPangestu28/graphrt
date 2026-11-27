//! The phone capabilities of the embedding API: skills, learning, the file
//! workspace and UI block validation, each behind its cargo feature.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use graphrt_ffi::{GraphrtError, RunEvent, RunListener, Runtime};

struct Terminal(Mutex<mpsc::Sender<RunEvent>>);

impl RunListener for Terminal {
    fn on_event(&self, event: RunEvent) {
        if matches!(event, RunEvent::Paused { .. } | RunEvent::Finished { .. }) {
            self.0.lock().unwrap().send(event).unwrap();
        }
    }
}

/// Run `workflow` with `inputs` and return its answer.
#[allow(dead_code)] // unused in builds without `scripted-llm`
fn answer(runtime: &Arc<Runtime>, workflow: &str, inputs: &str) -> String {
    let workflow = runtime.clone().load_workflow(workflow.into()).unwrap();
    let (tx, rx) = mpsc::channel();
    workflow.start(inputs.into(), "{}".into(), Arc::new(Terminal(Mutex::new(tx)))).unwrap();
    match rx.recv_timeout(Duration::from_secs(20)).unwrap() {
        RunEvent::Finished { result } => {
            assert_eq!(result.status, graphrt_ffi::RunStatus::Succeeded, "{result:?}");
            result.answer
        }
        other => panic!("the run paused: {other:?}"),
    }
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("graphrt-ffi-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(all(feature = "skills", feature = "scripted-llm"))]
#[test]
fn an_installed_skill_is_loaded_by_an_agent() {
    use graphrt_ffi::SkillInstall;

    let dir = temp_dir("skills");
    let config = serde_json::json!({
        "skills_dir": dir.join("skills"),
        "scripted_models": {"demo": [
            {"tool_calls": [{"name": "load_skill", "arguments": {"name": "trip-planner"}}]},
            {"text": "{tool_result}"},
        ]},
    });
    let runtime = Runtime::new(config.to_string()).unwrap();
    let skill_md =
        "---\nname: trip-planner\ndescription: Use when someone plans a trip.\n---\nAlways ask for the dates first.\n";
    let installed = runtime
        .install_skill(SkillInstall {
            skill_md: skill_md.into(),
            grants: vec![],
            replace: false,
            accept_findings: false,
        })
        .unwrap();
    assert_eq!((installed.name.as_str(), installed.enabled), ("trip-planner", true));
    assert_eq!(runtime.list_skills().unwrap().len(), 1);

    let workflow = r#"
version: "1"
graph:
  nodes:
    - { id: start, type: start }
    - id: assistant
      type: agent
      model: { provider: demo, name: scripted }
      query: Plan my trip
      skills: [trip-planner]
    - { id: reply, type: answer, answer: "{{#assistant.text#}}" }
  edges:
    - { source: start, target: assistant }
    - { source: assistant, target: reply }
"#;
    let text = answer(&runtime, workflow, "{}");
    assert!(text.contains("Always ask for the dates first."), "{text}");

    let duplicate = runtime.install_skill(SkillInstall {
        skill_md: skill_md.into(),
        grants: vec![],
        replace: false,
        accept_findings: false,
    });
    assert!(duplicate.is_err(), "installing twice needs `replace`");
    assert!(!runtime.set_skill_enabled("trip-planner".into(), false).unwrap().enabled);
    runtime.remove_skill("trip-planner".into()).unwrap();
    assert!(runtime.list_skills().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(all(feature = "learning", feature = "scripted-llm"))]
#[test]
fn a_lesson_is_used_only_once_approved() {
    use graphrt_ffi::{LessonDecision, LessonKind, LessonProposal, LessonStatus};

    let dir = temp_dir("learning");
    let config = serde_json::json!({
        "learning_dir": dir.join("learning"),
        "scripted_models": {"echo": [{"text": "ok"}]},
    });
    let runtime = Runtime::new(config.to_string()).unwrap();
    let proposal = LessonProposal {
        agent: "assistant".into(),
        user: Some("ada".into()),
        kind: LessonKind::Preference,
        text: "Answer in Indonesian.".into(),
        run_id: Some("run-1".into()),
        author: Some("ada".into()),
        author_authenticated: true,
        evidence: None,
    };
    let lesson = runtime.propose_lesson(proposal).unwrap();
    assert_eq!(lesson.status, LessonStatus::Proposed);
    assert_eq!(runtime.list_lessons("assistant".into(), Some(LessonStatus::Approved)).unwrap().len(), 0);
    let approved = runtime.decide_lesson("assistant".into(), lesson.id.clone(), LessonDecision::Approve).unwrap();
    assert_eq!(approved.status, LessonStatus::Approved);
    let owner =
        runtime.add_lesson("assistant".into(), None, LessonKind::Fact, "Invoices go to finance.".into()).unwrap();
    assert_eq!(owner.status, LessonStatus::Approved, "the owner's own lesson needs no approval");
    assert_eq!(runtime.list_lessons("assistant".into(), None).unwrap().len(), 2);

    // A node with `learning` compiles and runs against the store.
    let workflow = r#"
version: "1"
graph:
  nodes:
    - { id: start, type: start }
    - id: assistant
      type: agent
      model: { provider: echo, name: scripted }
      query: hi
      learning: { agent: assistant }
    - { id: reply, type: answer, answer: "{{#assistant.text#}}" }
  edges:
    - { source: start, target: assistant }
    - { source: assistant, target: reply }
"#;
    assert_eq!(answer(&runtime, workflow, "{}"), "ok");

    runtime.delete_lesson("assistant".into(), owner.id).unwrap();
    assert_eq!(runtime.list_lessons("assistant".into(), None).unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(all(feature = "files", feature = "scripted-llm"))]
#[test]
fn agents_write_and_read_the_workspace_only() {
    let dir = temp_dir("workspace");
    let config = serde_json::json!({
        "workspace": {"dir": dir, "max_files": 10},
        "scripted_models": {"demo": [
            {"tool_calls": [{"name": "write", "arguments": {"path": "notes/today.md", "content": "Call Ada"}}]},
            {"tool_calls": [{"name": "read", "arguments": {"path": "notes/today.md"}}]},
            {"text": "{tool_result}"},
        ]},
    });
    let runtime = Runtime::new(config.to_string()).unwrap();
    assert!(runtime.tool_providers().contains(&"files".to_owned()));
    let workflow = r#"
version: "1"
graph:
  nodes:
    - { id: start, type: start }
    - id: assistant
      type: agent
      model: { provider: demo, name: scripted }
      query: Note it down
      tools: [files/*]
    - { id: reply, type: answer, answer: "{{#assistant.text#}}" }
  edges:
    - { source: start, target: assistant }
    - { source: assistant, target: reply }
"#;
    let text = answer(&runtime, workflow, "{}");
    assert!(text.contains("Call Ada"), "{text}");
    assert_eq!(std::fs::read_to_string(dir.join("notes/today.md")).unwrap(), "Call Ada");
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(not(feature = "skills"))]
#[test]
fn skills_need_their_feature() {
    let runtime = Runtime::new("{}".into()).unwrap();
    assert!(matches!(runtime.list_skills(), Err(GraphrtError::Unsupported { .. })));
    assert!(Runtime::new(r#"{"skills_dir": "/tmp/x"}"#.into()).is_err());
}

#[cfg(feature = "skills")]
#[test]
fn skills_need_a_directory() {
    let runtime = Runtime::new("{}".into()).unwrap();
    let error = runtime.list_skills().unwrap_err();
    assert!(matches!(&error, GraphrtError::Unsupported { message } if message.contains("skills_dir")), "{error}");
}

#[cfg(feature = "learning")]
#[test]
fn learning_needs_a_directory() {
    let runtime = Runtime::new("{}".into()).unwrap();
    assert!(matches!(runtime.list_lessons("a".into(), None), Err(GraphrtError::Unsupported { .. })));
}

#[cfg(not(feature = "ui"))]
#[test]
fn ui_validation_needs_its_feature() {
    let result = graphrt_ffi::validate_ui_block(r#"{"$ui": "stats", "version": 1}"#.into());
    assert!(matches!(result, Err(GraphrtError::Unsupported { .. })));
}

#[test]
fn unknown_config_keys_are_refused() {
    assert!(matches!(Runtime::new(r#"{"skill_dir": "x"}"#.into()), Err(GraphrtError::InvalidConfig { .. })));
    let _ = temp_dir;
}
