//! Using graphrt-eval from `cargo test`: a workflow with an `llm` node backed
//! by a scripted model, evaluated in-process through [`EngineTarget`].
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::Path;
use std::sync::Arc;

use graphrt_core::node::TokioClock;
use graphrt_core::{Engine, Graph, Registry, Services, Workflow};
use graphrt_eval::{EngineTarget, EvalSuite, Runner, RunnerOptions, TestStatus};
use graphrt_llm::ModelRegistry;
use graphrt_llm::mock::MockModel;

const WORKFLOW: &str = r#"
version: "1"
name: summarize
graph:
  nodes:
    - { id: start, type: start, variables: [{ variable: text, type: paragraph, required: true }] }
    - id: llm
      type: llm
      model: { provider: mock, name: m }
      prompt_template: [{ role: user, text: "Summarize: {{#start.text#}}" }]
    - { id: answer, type: answer, answer: "{{#llm.text#}}" }
  edges:
    - { source: start, target: llm }
    - { source: llm, target: answer }
"#;

const SUITE: &str = r#"
workflow: summarize.yaml
defaultTest:
  assert: [{ type: tokens, threshold: 10 }]
tests:
  - id: short
    vars: { text: "a long story" }
    assert:
      - { type: equals, value: "A story." }
  - id: wrong
    vars: { text: "another story" }
    assert:
      - { type: contains, value: "dragon" }
"#;

fn engine(model: Arc<MockModel>) -> Engine {
    let mut registry = Registry::with_builtins();
    graphrt_llm::register(&mut registry);
    let graph = Graph::build(Workflow::parse(WORKFLOW).unwrap(), &registry).unwrap();
    let models = ModelRegistry::new().with("mock", model);
    Engine::new(Arc::new(graph)).with_services(Services::new().with(models)).with_clock(Arc::new(TokioClock))
}

#[tokio::test]
async fn evaluates_a_workflow_in_process() {
    let model = Arc::new(MockModel::new(vec![MockModel::text("A story."), MockModel::text("Another story.")]));
    let suite = EvalSuite::parse(SUITE, Path::new("")).unwrap();
    let mut options = RunnerOptions::default();
    options.concurrency = 1;
    let target = EngineTarget::new(engine(Arc::clone(&model)));
    let mut streamed = 0;
    let report = Runner::new(options).run(&suite, &target, |_| streamed += 1).await.unwrap();

    assert_eq!(streamed, 2);
    assert_eq!(model.requests().len(), 2, "one model call per test");
    let short = &report.tests[0];
    assert_eq!(short.status, TestStatus::Passed, "{short:?}");
    assert_eq!(short.run.answer, "A story.");
    assert_eq!(short.run.usage.total_tokens, 2);
    let wrong = &report.tests[1];
    assert_eq!(wrong.status, TestStatus::Failed);
    assert!(wrong.reason.as_deref().unwrap().contains("dragon"));
    assert_eq!(report.workflow.name, "summarize");
    assert!(!report.workflow.fingerprint.is_empty());
    assert!(!report.gate_passed());
}

#[tokio::test]
async fn failed_runs_are_errors() {
    // An exhausted script makes the llm node fail.
    let model = Arc::new(MockModel::new(vec![]));
    let suite = EvalSuite::parse(SUITE, Path::new("")).unwrap();
    let report =
        Runner::new(RunnerOptions::default()).run(&suite, &EngineTarget::new(engine(model)), |_| {}).await.unwrap();
    assert!(report.tests.iter().all(|t| t.status == TestStatus::Error), "{:?}", report.tests);
    assert_eq!(report.summary.errors, 2);
    assert!(report.to_junit().contains("<error "));
}
