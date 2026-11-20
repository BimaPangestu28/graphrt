//! Running a suite: the [`Target`] under test, concurrency, trials, filters,
//! scoring, trial statistics, calibration and the gate.

use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use futures::future::BoxFuture;
use graphrt_core::response::ResponseAssembler;
use graphrt_core::{Engine, RunInput, Value};
use indexmap::IndexMap;
use serde_json::Value as Json;

use crate::assertion::{AssertionContext, AssertionRegistry, AssertionResult, PreparedAssertion};
use crate::capture::{RunCapture, RunStatus};
use crate::config::{AssertionSpec, EvalSuite, TestCase, inputs_of};
use crate::error::EvalError;
use crate::path::render;
use crate::report::{
    EvalReport, REPORT_SCHEMA_VERSION, RunSummary, Summary, TestResult, TestStatus, WorkflowInfo, case_summaries,
    trial_summary,
};

/// The system under test: runs one test case and reports what happened.
///
/// [`EngineTarget`] runs a compiled graphrt workflow in-process; other
/// implementations can call a server or a fake.
pub trait Target: Send + Sync {
    /// Run `case` once. Return `Err` only when the run could not be attempted;
    /// a failed run is a capture with [`RunStatus::Failed`].
    fn run<'a>(&'a self, case: &'a TestCase) -> BoxFuture<'a, Result<RunCapture, EvalError>>;

    /// Describes the workflow for the report.
    fn workflow(&self) -> WorkflowInfo {
        WorkflowInfo::default()
    }

    /// The start variables and their input types (`paragraph`, `json`...),
    /// used to shape judge inputs. Empty when unknown.
    fn start_variables(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// Runs test cases on an [`Engine`] (one compiled graph, shared by every test).
///
/// Set a [`graphrt_core::node::Clock`] with timers on the engine (e.g.
/// `TokioClock`) for `Limits::max_duration_ms` to act as a per-test timeout.
#[derive(Clone, Debug)]
pub struct EngineTarget {
    engine: Engine,
    env: IndexMap<String, Value>,
    path: String,
}

impl EngineTarget {
    pub fn new(engine: Engine) -> Self {
        Self { engine, env: IndexMap::new(), path: String::new() }
    }

    /// Values for the workflow's declared `env.*` variables.
    pub fn with_env(mut self, env: IndexMap<String, Value>) -> Self {
        self.env = env;
        self
    }

    /// The workflow file path shown in the report.
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }

    /// The engine test cases run on (for hosts that drive runs themselves, e.g. to trace them).
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// The values of the workflow's `env.*` variables.
    pub fn env(&self) -> &IndexMap<String, Value> {
        &self.env
    }

    async fn run_case(&self, case: &TestCase) -> RunCapture {
        let mut input = RunInput::new();
        input.inputs = inputs_of(&case.vars);
        input.env.clone_from(&self.env);
        let started = Instant::now();
        let mut assembler = ResponseAssembler::new(self.engine.graph());
        let mut events = Vec::new();
        let mut stream = self.engine.run(input);
        while let Some(event) = stream.next().await {
            let _chunks = assembler.push(&event);
            events.push(event);
        }
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        RunCapture::from_events(events, assembler.text_so_far().to_owned(), latency_ms)
    }
}

impl Target for EngineTarget {
    fn run<'a>(&'a self, case: &'a TestCase) -> BoxFuture<'a, Result<RunCapture, EvalError>> {
        Box::pin(async move { Ok(self.run_case(case).await) })
    }

    fn workflow(&self) -> WorkflowInfo {
        let graph = self.engine.graph();
        WorkflowInfo {
            path: self.path.clone(),
            name: graph.workflow().name.clone().unwrap_or_default(),
            fingerprint: graph.fingerprint().to_owned(),
        }
    }

    fn start_variables(&self) -> Vec<(String, String)> {
        let nodes = &self.engine.graph().workflow().graph.nodes;
        let Some(start) = nodes.iter().find(|n| n.kind == "start" && n.parent.is_none()) else { return Vec::new() };
        let declared = start.config.get("variables").and_then(Json::as_array).into_iter().flatten();
        declared
            .filter_map(|v| {
                let name = v.get("variable")?.as_str()?;
                let kind = v.get("type").and_then(Json::as_str).unwrap_or("text-input");
                Some((name.to_owned(), kind.to_owned()))
            })
            .collect()
    }
}

/// How to run a suite.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct RunnerOptions {
    /// Tests running at the same time (at least 1).
    pub concurrency: usize,
    /// Trials per test case (at least 1); each trial is a separate result, and
    /// with more than one the report adds pass@k and pass^k per case.
    pub trials: u32,
    /// Keep only cases whose id or a tag matches one of these (`*` wildcards).
    pub filters: Vec<String>,
    /// Override the suite's `gate.pass_rate`.
    pub pass_rate: Option<f64>,
    /// Include the run events in the results.
    pub keep_events: bool,
}

impl Default for RunnerOptions {
    fn default() -> Self {
        Self { concurrency: 4, trials: 1, filters: Vec::new(), pass_rate: None, keep_events: false }
    }
}

/// Runs suites against a [`Target`].
#[derive(Clone, Debug, Default)]
pub struct Runner {
    registry: AssertionRegistry,
    options: RunnerOptions,
}

/// A selected test case with its assertions built.
type PreparedCase = (Arc<TestCase>, Arc<Vec<PreparedAssertion>>);

struct Job {
    index: usize,
    trial: u32,
    case: Arc<TestCase>,
    assertions: Arc<Vec<PreparedAssertion>>,
}

impl Runner {
    /// A runner with the built-in assertion types.
    pub fn new(options: RunnerOptions) -> Self {
        Self { registry: AssertionRegistry::with_builtins(), options }
    }

    /// Use these assertion types (e.g. built-ins plus custom ones).
    pub fn with_registry(mut self, registry: AssertionRegistry) -> Self {
        self.registry = registry;
        self
    }

    /// The cases selected by the filters, with their assertions built.
    /// Fails before anything runs if an assertion is invalid.
    fn prepare(&self, suite: &EvalSuite) -> Result<Vec<PreparedCase>, EvalError> {
        let filters = &self.options.filters;
        let mut prepared = Vec::new();
        for case in suite.cases.iter().filter(|c| filters.is_empty() || filters.iter().any(|f| c.matches(f))) {
            let context = template_context(case);
            let mut assertions = Vec::with_capacity(case.assertions.len());
            for spec in &case.assertions {
                let rendered = render_spec(spec, &context)
                    .and_then(|spec| self.registry.prepare(&spec))
                    .map_err(|e| EvalError::invalid(format!("test `{}`: {e}", case.id)))?;
                assertions.push(rendered);
            }
            prepared.push((Arc::new(case.clone()), Arc::new(assertions)));
        }
        Ok(prepared)
    }

    /// Run `suite` against `target`, calling `on_result` as each test finishes
    /// (in completion order). The report lists results in suite order.
    pub async fn run<T>(
        &self,
        suite: &EvalSuite,
        target: &T,
        mut on_result: impl FnMut(&TestResult),
    ) -> Result<EvalReport, EvalError>
    where
        T: Target + ?Sized,
    {
        let prepared = self.prepare(suite)?;
        let started_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or_default();
        let started = Instant::now();
        let trials = self.options.trials.max(1);
        let jobs: Vec<Job> = prepared
            .iter()
            .enumerate()
            .flat_map(|(index, (case, assertions))| {
                (0..trials).map(move |trial| Job {
                    index,
                    trial,
                    case: Arc::clone(case),
                    assertions: Arc::clone(assertions),
                })
            })
            .collect();
        let keep_events = self.options.keep_events;
        let mut results: Vec<(usize, TestResult)> = Vec::with_capacity(jobs.len());
        let mut pending = futures::stream::iter(jobs)
            .map(|job| async move {
                let result = run_job(target, &job, keep_events).await;
                result.map(|r| (job.index, r))
            })
            .buffer_unordered(self.options.concurrency.max(1));
        while let Some(finished) = pending.next().await {
            let (index, result) = finished?;
            on_result(&result);
            results.push((index, result));
        }
        drop(pending);
        results.sort_by_key(|(index, result)| (*index, result.trial));
        let tests: Vec<TestResult> = results.into_iter().map(|(_, r)| r).collect();
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let pass_rate = self.options.pass_rate.unwrap_or(suite.gate.pass_rate);
        let mut workflow = target.workflow();
        if workflow.path.is_empty() {
            workflow.path = suite.workflow.display().to_string();
        }
        let mut summary = Summary::of(&tests, pass_rate, duration_ms);
        let gate = &suite.gate;
        let cases = if trials > 1 || gate.pass_hat_k.is_some() {
            let k = gate.k.unwrap_or(trials).clamp(1, trials);
            let cases = case_summaries(&tests, k);
            let stats = trial_summary(&cases, k);
            if let Some(required) = gate.pass_hat_k {
                summary.gate.pass_hat_k = Some(required);
                let actual = stats.pass_hat_k;
                summary.gate.require(actual + 1e-9 >= required, || format!("pass^{k} {actual:.3} < {required:.3}"));
            }
            summary.trials = Some(stats);
            cases
        } else {
            Vec::new()
        };
        let calibration = suite.calibration.as_ref().map(|config| {
            let calibration = crate::calibration::calibrate(&tests, config);
            crate::calibration::gate(&calibration, config, &mut summary.gate);
            calibration
        });
        Ok(EvalReport {
            schema_version: REPORT_SCHEMA_VERSION,
            graphrt_version: env!("CARGO_PKG_VERSION").to_owned(),
            suite: suite.path.display().to_string(),
            description: suite.description.clone(),
            workflow,
            started_at_ms,
            summary,
            cases,
            calibration,
            baseline: None,
            tests,
        })
    }
}

/// Values `{{...}}` references in assertions can use: the test's vars (also at
/// the top level, as in Promptfoo), `expected`, `metadata`, `id` and `tags`.
fn template_context(case: &TestCase) -> Json {
    let mut context = case.vars.clone();
    context.insert("vars".into(), Json::Object(case.vars.clone()));
    context.insert("expected".into(), case.expected.clone().unwrap_or(Json::Null));
    context.insert("metadata".into(), Json::Object(case.metadata.clone()));
    context.insert("id".into(), Json::String(case.id.clone()));
    context.insert("tags".into(), Json::from(case.tags.clone()));
    Json::Object(context)
}

fn render_spec(spec: &AssertionSpec, context: &Json) -> Result<AssertionSpec, EvalError> {
    let mut rendered = spec.clone();
    rendered.value = spec.value.as_ref().map(|v| render(v, context)).transpose()?;
    for value in rendered.config.values_mut() {
        *value = render(value, context)?;
    }
    Ok(rendered)
}

async fn run_job<T: Target + ?Sized>(target: &T, job: &Job, keep_events: bool) -> Result<TestResult, EvalError> {
    let case = &job.case;
    let capture = target.run(case).await?;
    let ctx = AssertionContext::new(case, &capture);
    let mut assertions = Vec::with_capacity(job.assertions.len());
    for assertion in job.assertions.iter() {
        assertions.push(assertion.run(ctx).await);
    }
    let (pass, score) = verdict(&assertions, case.threshold);
    let status = match (capture.status, pass) {
        (RunStatus::Succeeded, true) => TestStatus::Passed,
        (RunStatus::Succeeded, false) => TestStatus::Failed,
        _ => TestStatus::Error,
    };
    let reason = match status {
        TestStatus::Passed => None,
        TestStatus::Error => Some(format!(
            "run {}: {}",
            serde_json::to_value(capture.status).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default(),
            capture.error.as_deref().unwrap_or("no details")
        )),
        TestStatus::Failed => {
            let failed: Vec<String> = assertions
                .iter()
                .filter(|a| !a.pass && a.gates())
                .map(|a| format!("{}: {}", a.kind, a.reason))
                .collect();
            Some(if failed.is_empty() {
                format!("score {score:.2} below threshold {:.2}", case.threshold.unwrap_or(1.0))
            } else {
                failed.join("; ")
            })
        }
    };
    let tool_calls = capture.tool_calls();
    let retrieval = capture.retrieval_hits();
    let output = capture.output();
    let RunCapture { run_id, status: run_status, error, error_type, outputs, answer, usage, latency_ms, events } =
        capture;
    Ok(TestResult {
        id: case.id.clone(),
        description: case.description.clone(),
        source: case.source.clone(),
        tags: case.tags.clone(),
        trial: job.trial,
        status,
        pass: status == TestStatus::Passed,
        score,
        reason,
        vars: case.vars.clone(),
        expected: case.expected.clone(),
        run: RunSummary {
            run_id,
            status: run_status,
            error,
            error_type,
            latency_ms,
            usage,
            output,
            outputs,
            answer,
            tool_calls,
            retrieval,
            events: keep_events.then_some(events),
        },
        assertions,
    })
}

/// Promptfoo scoring: the score is the weighted mean of the assertion scores
/// (weight 0 = report only). Without a threshold every gating assertion must
/// pass; with one, the score must reach it. Excluded results (a judge's
/// `unknown` with `unknown: exclude`) take no part; a test whose results are
/// all excluded passes with score 1.
pub fn verdict(assertions: &[AssertionResult], threshold: Option<f64>) -> (bool, f64) {
    let counted = || assertions.iter().filter(|a| !a.excluded);
    let total_weight: f64 = counted().map(|a| a.weight).sum();
    let count = counted().count();
    let score = if total_weight > 0.0 {
        counted().map(|a| a.weight * a.score).sum::<f64>() / total_weight
    } else if count == 0 {
        1.0
    } else {
        counted().map(|a| a.score).sum::<f64>() / count as f64
    };
    let pass = match threshold {
        Some(t) => score + 1e-9 >= t,
        None => counted().filter(|a| a.weight > 0.0).all(|a| a.pass),
    };
    (pass, score)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;
    use serde_json::json;

    fn result(pass: bool, score: f64, weight: f64) -> AssertionResult {
        AssertionResult::new("x", pass, score, weight, "")
    }

    #[test]
    fn scoring_follows_promptfoo() {
        let mixed = [result(true, 1.0, 1.0), result(false, 0.0, 0.0)];
        assert_eq!(verdict(&mixed, None), (true, 1.0), "weight 0 is report only");
        let weighted = [result(true, 1.0, 3.0), result(false, 0.0, 1.0)];
        assert_eq!(verdict(&weighted, None), (false, 0.75));
        assert_eq!(verdict(&weighted, Some(0.7)), (true, 0.75));
        assert_eq!(verdict(&weighted, Some(0.8)), (false, 0.75));
        assert!(verdict(&[result(false, 0.5, 0.0)], None).0);
        let mut unknown = result(false, 0.0, 1.0);
        unknown.excluded = true;
        assert_eq!(verdict(&[result(true, 0.8, 1.0), unknown.clone()], None), (true, 0.8), "excluded is ignored");
        assert_eq!(verdict(&[unknown], None), (true, 1.0));
    }

    /// A fake target answering `Hello, <name>.`, failing for `name: fail`.
    #[derive(Debug)]
    struct Greeter;
    impl Target for Greeter {
        fn run<'a>(&'a self, case: &'a TestCase) -> BoxFuture<'a, Result<RunCapture, EvalError>> {
            Box::pin(async move {
                let name = case.vars.get("name").and_then(Json::as_str).unwrap_or_default();
                let mut capture = RunCapture { latency_ms: 3, ..RunCapture::default() };
                if name == "fail" {
                    capture.status = RunStatus::Failed;
                    capture.error = Some("boom".into());
                } else {
                    capture.outputs.insert("greeting".into(), json!(format!("Hello, {name}.")));
                }
                Ok(capture)
            })
        }
    }

    fn suite() -> EvalSuite {
        EvalSuite::parse(
            r#"
workflow: w.yaml
gate: { pass_rate: 0.5 }
defaultTest:
  assert: [{ type: latency, threshold: 1000 }]
tests:
  - id: ada
    tags: [smoke]
    vars: { name: Ada }
    assert: [{ type: equals, value: "Hello, {{name}}." }]
  - id: bob
    vars: { name: Bob }
    assert: [{ type: contains, value: Alice }]
  - id: broken
    vars: { name: fail }
    assert: [{ type: contains, value: x }]
"#,
            std::path::Path::new(""),
        )
        .unwrap()
    }

    fn run(options: RunnerOptions) -> (EvalReport, Vec<String>) {
        let mut seen = Vec::new();
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let report = rt.block_on(Runner::new(options).run(&suite(), &Greeter, |r| seen.push(r.display_id()))).unwrap();
        (report, seen)
    }

    #[test]
    fn runs_scores_and_gates() {
        let (report, seen) = run(RunnerOptions::default());
        assert_eq!(seen.len(), 3);
        let ids: Vec<&str> = report.tests.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["ada", "bob", "broken"], "suite order");
        assert_eq!(report.tests[0].status, TestStatus::Passed);
        assert_eq!(report.tests[0].assertions.len(), 2);
        assert_eq!(report.tests[1].status, TestStatus::Failed);
        assert!(report.tests[1].reason.as_deref().unwrap().contains("Alice"));
        assert_eq!(report.tests[2].status, TestStatus::Error);
        assert!(report.tests[2].reason.as_deref().unwrap().contains("boom"));
        assert_eq!(report.summary.passed, 1);
        assert!(!report.gate_passed(), "1/3 < 0.5");

        let mut options = RunnerOptions { pass_rate: Some(0.3), ..RunnerOptions::default() };
        assert!(run(options.clone()).0.gate_passed());
        options.filters = vec!["smoke".into()];
        options.trials = 3;
        options.concurrency = 2;
        let (report, seen) = run(options);
        assert_eq!(report.tests.len(), 3);
        assert!(report.tests.iter().all(|t| t.id == "ada"));
        assert_eq!(report.tests.iter().map(|t| t.trial).collect::<Vec<_>>(), [0, 1, 2]);
        assert!(seen.contains(&"ada#2".to_owned()));
        assert!(report.gate_passed());
        assert_eq!(report.cases.len(), 1);
        let trials = report.summary.trials.as_ref().unwrap();
        assert_eq!((trials.trials, trials.k, trials.pass_hat_k), (3, 3, 1.0));
    }

    #[test]
    fn trials_report_pass_at_k_and_gate_on_pass_hat_k() {
        let mut suite = suite();
        suite.gate.pass_rate = 0.0;
        suite.gate.pass_hat_k = Some(0.5);
        suite.gate.k = Some(2);
        let options = RunnerOptions { trials: 4, concurrency: 3, ..RunnerOptions::default() };
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let report = rt.block_on(Runner::new(options).run(&suite, &Greeter, |_| {})).unwrap();
        assert_eq!(report.tests.len(), 12);
        let ids: Vec<&str> = report.cases.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["ada", "bob", "broken"]);
        assert_eq!((report.cases[0].passed, report.cases[0].pass_hat_k), (4, 1.0));
        assert_eq!((report.cases[1].passed, report.cases[1].pass_at_k), (0, 0.0));
        let trials = report.summary.trials.as_ref().unwrap();
        assert!((trials.pass_hat_k - 1.0 / 3.0).abs() < 1e-12, "{trials:?}");
        assert!(!report.gate_passed());
        assert_eq!(report.summary.gate.failures, ["pass^2 0.333 < 0.500"]);
        let text = crate::report::format_summary(&report);
        assert!(text.contains("pass@2 0.333, pass^2 0.333 over 3 case(s)"), "{text}");
        assert!(text.contains("gate: FAILED (pass^2 0.333 < 0.500)"), "{text}");
    }

    #[test]
    fn invalid_assertions_fail_before_running() {
        let mut suite = suite();
        suite.cases[1].assertions.push(AssertionSpec::new("regex", Some(json!("("))));
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let err = rt.block_on(Runner::default().run(&suite, &Greeter, |_| {})).unwrap_err().to_string();
        assert!(err.contains("test `bob`") && err.contains("regex"), "{err}");
    }
}
