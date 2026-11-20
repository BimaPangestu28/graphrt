//! `judge` assertions backed by a judge workflow whose `llm` node is a
//! scripted [`MockModel`]: input mapping, verdicts, `unknown` handling,
//! compile-once caching and the fingerprint in the results.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use graphrt_core::node::TokioClock;
use graphrt_core::{Engine, Graph, Registry, Services, Workflow};
use graphrt_eval::judge::{self, JudgeRecord};
use graphrt_eval::runner::Target;
use graphrt_eval::{AssertionRegistry, EngineTarget, EvalError, EvalSuite, Runner, RunnerOptions, TestStatus, Verdict};
use graphrt_llm::ModelRegistry;
use graphrt_llm::mock::MockModel;

/// The system under test: echoes a canned answer.
const ANSWERER: &str = r#"
version: "1"
name: answerer
graph:
  nodes:
    - { id: start, type: start, variables: [{ variable: answer, type: paragraph, required: true }] }
    - { id: end, type: end, outputs: [{ variable: answer, value_selector: [start, answer] }] }
  edges: [{ source: start, target: end }]
"#;

/// A binary judge: one `llm` node with structured output.
const JUDGE: &str = r#"
version: "1"
name: polite-judge
graph:
  nodes:
    - id: start
      type: start
      variables:
        - { variable: output, type: paragraph, required: true }
        - { variable: question, type: paragraph }
        - { variable: vars, type: json }
    - id: judge
      type: llm
      model: { provider: mock, name: judge-model }
      prompt_template:
        - { role: system, text: "Is the answer polite? Reply pass, fail or unknown." }
        - { role: user, text: "Question: {{#start.question#}}\nAnswer: {{#start.output#}}" }
      structured_output:
        schema:
          type: object
          required: [verdict, reasoning]
          properties:
            verdict: { type: string, enum: [pass, fail, unknown] }
            reasoning: { type: string }
    - { id: end, type: end, outputs: [{ variable: result, value_selector: [judge, structured_output] }] }
  edges:
    - { source: start, target: judge }
    - { source: judge, target: end }
"#;

const SUITE: &str = r#"
workflow: answerer.yaml
tests:
  - id: polite
    vars: { answer: "Thank you, happy to help.", question: "Can you help?" }
    assert: &polite
      - { type: judge, workflow: judges/polite.yaml, metric: politeness, inputs: { question: "{{question}}" } }
  - id: rude
    vars: { answer: "Go away.", question: "Can you help?" }
    assert: *polite
  - id: unsure-excluded
    vars: { answer: "Hmm.", question: "?" }
    assert:
      - { type: judge, workflow: judges/polite.yaml, unknown: exclude, metric: politeness }
  - id: unsure-fails
    vars: { answer: "Hmm.", question: "?" }
    assert: *polite
"#;

fn registry() -> Registry {
    let mut registry = Registry::with_builtins();
    graphrt_llm::register(&mut registry);
    registry
}

fn target(yaml: &str, models: ModelRegistry) -> EngineTarget {
    let graph = Graph::build(Workflow::parse(yaml).unwrap(), &registry()).unwrap();
    let engine =
        Engine::new(Arc::new(graph)).with_services(Services::new().with(models)).with_clock(Arc::new(TokioClock));
    EngineTarget::new(engine)
}

fn verdict(verdict: &str, reasoning: &str) -> Vec<graphrt_llm::ChatDelta> {
    MockModel::text(&serde_json::json!({"verdict": verdict, "reasoning": reasoning}).to_string())
}

#[tokio::test]
async fn judges_run_as_workflows_with_mapped_inputs() {
    let judge_model = Arc::new(MockModel::new(vec![
        verdict("pass", "courteous"),
        verdict("fail", "dismissive"),
        verdict("unknown", "too short to tell"),
        verdict("unknown", "too short to tell"),
    ]));
    let loads = Arc::new(AtomicUsize::new(0));
    let mut assertions = AssertionRegistry::with_builtins();
    let (model, counter) = (Arc::clone(&judge_model), Arc::clone(&loads));
    judge::register(&mut assertions, "/evals", move |path: &Path| -> Result<Arc<dyn Target>, EvalError> {
        assert_eq!(path, Path::new("/evals/judges/polite.yaml"));
        counter.fetch_add(1, Ordering::SeqCst);
        let models = ModelRegistry::new().with("mock", Arc::clone(&model) as Arc<dyn graphrt_llm::ChatModel>);
        Ok(Arc::new(target(JUDGE, models).with_path("judges/polite.yaml")))
    });

    let suite = EvalSuite::parse(SUITE, Path::new("/evals")).unwrap();
    let mut options = RunnerOptions::default();
    options.concurrency = 1;
    let report = Runner::new(options)
        .with_registry(assertions)
        .run(&suite, &target(ANSWERER, ModelRegistry::new()), |_| {})
        .await
        .unwrap();

    assert_eq!(loads.load(Ordering::SeqCst), 1, "the judge is compiled once for every test");
    let requests = judge_model.requests();
    assert_eq!(requests.len(), 4, "one judge run per test");
    let prompt = format!("{:?}", requests[0].messages);
    assert!(prompt.contains("Question: Can you help?") && prompt.contains("Answer: Thank you"), "{prompt}");

    let statuses: Vec<TestStatus> = report.tests.iter().map(|t| t.status).collect();
    assert_eq!(statuses, [TestStatus::Passed, TestStatus::Failed, TestStatus::Passed, TestStatus::Failed]);
    let polite = &report.tests[0].assertions[0];
    assert_eq!(polite.reason, "judge: pass: courteous");
    let record: &JudgeRecord = polite.judge.as_deref().unwrap();
    assert_eq!(record.workflow, "judges/polite.yaml");
    assert!(!record.fingerprint.is_empty(), "the judge fingerprint is recorded");
    assert_eq!(record.verdict, Some(Verdict::Pass));
    assert_eq!(record.reasoning.as_deref(), Some("courteous"));
    assert!(!record.run_id.is_empty());

    // `unknown: exclude` reports the verdict without scoring or gating it.
    let excluded = &report.tests[2].assertions[0];
    assert!(excluded.excluded && excluded.reason.ends_with("(excluded)"), "{excluded:?}");
    assert_eq!(report.tests[2].score, 1.0);
    // The default, `unknown: fail`, fails the test.
    assert!(report.tests[3].reason.as_deref().unwrap().contains("judge: unknown: too short to tell"));
    let politeness = &report.summary.metrics["politeness"];
    assert_eq!((politeness.count, politeness.passed, politeness.excluded), (3, 1, 1));

    let json = report.to_json().unwrap();
    assert!(json.contains("\"fingerprint\"") && json.contains("\"excluded\": true"), "{json}");
}

#[test]
fn judge_failures_and_bad_specs_are_reported() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
    // An exhausted script fails the judge's llm node: the assertion fails, the test is not an error.
    let judge_model = Arc::new(MockModel::new(vec![MockModel::text("not json at all")]));
    let mut assertions = AssertionRegistry::with_builtins();
    let model = Arc::clone(&judge_model);
    judge::register(&mut assertions, "", move |_: &Path| -> Result<Arc<dyn Target>, EvalError> {
        let models = ModelRegistry::new().with("mock", Arc::clone(&model) as Arc<dyn graphrt_llm::ChatModel>);
        Ok(Arc::new(target(JUDGE, models)))
    });
    let suite = EvalSuite::parse(
        "workflow: a.yaml\ntests:\n  - vars: { answer: hi }\n    assert: [{ type: judge, workflow: j.yaml }]\n",
        Path::new(""),
    )
    .unwrap();
    let answerer = target(ANSWERER, ModelRegistry::new());
    let runner = Runner::new(RunnerOptions::default()).with_registry(assertions);
    let report = rt.block_on(runner.run(&suite, &answerer, |_| {})).unwrap();
    let test = &report.tests[0];
    assert_eq!(test.status, TestStatus::Failed);
    assert!(test.reason.as_deref().unwrap().contains("judge run did not succeed"), "{:?}", test.reason);
    assert!(test.assertions[0].judge.is_some());

    let bad = |assertion: &str| {
        let text = format!("workflow: a.yaml\ntests:\n  - vars: {{ answer: hi }}\n    assert: [{assertion}]\n");
        let suite = EvalSuite::parse(&text, Path::new("")).unwrap();
        rt.block_on(runner.run(&suite, &answerer, |_| {})).unwrap_err().to_string()
    };
    assert!(bad("{ type: judge }").contains("`workflow`"));
    assert!(bad("{ type: judge, workflow: j.yaml, unknown: skip }").contains("`unknown`"));
    assert!(bad("{ type: judge, workflow: j.yaml, inputs: 3 }").contains("`inputs`"));
    assert!(bad("{ type: judge, workflow: j.yaml, rubric: x }").contains("unknown key `rubric`"));
}
