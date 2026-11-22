//! `graphrt eval --otel`: one OpenTelemetry trace per test run, shaped as a
//! Langfuse experiment over OTLP (`langfuse.experiment.*` attributes, one
//! trace per dataset item, no enclosing experiment span), with every
//! assertion result as an `evaluate {metric}` span carrying a
//! `gen_ai.evaluation.result` event.
//!
//! On every span: `langfuse.experiment.id` (`<suite>-<start time>`, one per
//! `graphrt eval` invocation), `langfuse.experiment.name` (the suite file
//! stem) and `langfuse.experiment.dataset.id` (the suite path: a stable local
//! id). On the root span: `langfuse.experiment.item.id` (the test id),
//! `langfuse.experiment.item.root_observation_id` (the root span's own id),
//! `langfuse.experiment.item.expected_output` and `.item.metadata.*`, and
//! `langfuse.experiment.description`. Inputs and outputs follow the content
//! capture setting as for any run. Other backends ignore the `langfuse.*`
//! attributes and still get one trace per test; Braintrust routes them into an
//! experiment with `BRAINTRUST_PARENT=experiment_id:<id>` (`--otel-preset braintrust`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use futures::StreamExt;
use futures::future::BoxFuture;
use graphrt_core::RunInput;
use graphrt_core::response::ResponseAssembler;
use graphrt_eval::report::WorkflowInfo;
use graphrt_eval::{EngineTarget, EvalError, RunCapture, Target, TestCase, TestResult};
use graphrt_otel::semconv::langfuse;
use graphrt_otel::{AttrValue, EvaluationResult, SpanId};

use crate::otel::{RunContext, Telemetry};

/// Experiment-wide attributes.
#[derive(Clone, Debug)]
pub struct Experiment {
    pub id: String,
    pub name: String,
    pub dataset_id: String,
    pub description: Option<String>,
}

/// Runs test cases like [`EngineTarget`], tracing each run.
#[derive(Debug)]
pub struct TracedTarget {
    inner: EngineTarget,
    telemetry: Arc<Telemetry>,
    experiment: Experiment,
    /// `(trace id, root span id)` by run id, for the assertion spans.
    runs: Mutex<HashMap<String, (String, String)>>,
}

impl TracedTarget {
    /// Trace the runs of `inner` (its engine and env) with `telemetry`.
    pub fn new(inner: EngineTarget, telemetry: Arc<Telemetry>, experiment: Experiment) -> Self {
        Self { inner, telemetry, experiment, runs: Mutex::default() }
    }

    fn span_attributes(&self) -> Vec<(String, AttrValue)> {
        let mut attributes = vec![
            (langfuse::EXPERIMENT_ID.to_owned(), AttrValue::from(self.experiment.id.as_str())),
            (langfuse::EXPERIMENT_NAME.to_owned(), AttrValue::from(self.experiment.name.as_str())),
            (langfuse::EXPERIMENT_DATASET_ID.to_owned(), AttrValue::from(self.experiment.dataset_id.as_str())),
        ];
        attributes.push((
            format!("{}graphrt_version", langfuse::EXPERIMENT_METADATA_PREFIX),
            env!("CARGO_PKG_VERSION").into(),
        ));
        attributes
    }

    fn root_attributes(&self, case: &TestCase, root: SpanId) -> Vec<(String, AttrValue)> {
        let mut attributes = vec![
            (langfuse::EXPERIMENT_ITEM_ID.to_owned(), AttrValue::from(case.id.as_str())),
            (langfuse::EXPERIMENT_ITEM_ROOT_OBSERVATION_ID.to_owned(), AttrValue::from(root.to_hex())),
        ];
        if let Some(description) = &self.experiment.description {
            attributes.push((langfuse::EXPERIMENT_DESCRIPTION.to_owned(), AttrValue::from(description.as_str())));
        }
        if let Some(expected) = &case.expected {
            let text = match expected {
                serde_json::Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            attributes.push((langfuse::EXPERIMENT_ITEM_EXPECTED_OUTPUT.to_owned(), AttrValue::Str(text)));
        }
        for (key, value) in &case.metadata {
            let text = value.as_str().map_or_else(|| value.to_string(), str::to_owned);
            attributes.push((format!("{}{key}", langfuse::EXPERIMENT_ITEM_METADATA_PREFIX), AttrValue::Str(text)));
        }
        attributes
    }

    async fn run_traced(&self, case: &TestCase) -> RunCapture {
        let engine = self.inner.engine();
        let mut input = RunInput::new();
        input.inputs = graphrt_eval::config::inputs_of(&case.vars);
        input.env.clone_from(self.inner.env());
        let context = RunContext { tags: case.tags.clone(), ..RunContext::default() };
        let root = SpanId::random();
        let mut tracer = self.telemetry.tracer_with(
            engine.graph(),
            &context,
            (self.span_attributes(), self.root_attributes(case, root)),
            root,
        );
        let started = Instant::now();
        let mut assembler = ResponseAssembler::new(engine.graph());
        let mut events = Vec::new();
        let mut stream = engine.run(input);
        while let Some(event) = stream.next().await {
            tracer.on_event(&event);
            let _chunks = assembler.push(&event);
            events.push(event);
        }
        let ids = tracer.ids();
        tracer.finish();
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let capture = RunCapture::from_events(events, assembler.text_so_far().to_owned(), latency_ms);
        if let Some(ids) = ids {
            self.runs.lock().unwrap_or_else(PoisonError::into_inner).insert(capture.run_id.clone(), ids);
        }
        capture
    }

    /// Emit one `evaluate {metric}` span per assertion of a finished test,
    /// under the test run's root span.
    pub fn record_result(&self, result: &TestResult) {
        let ids = self.runs.lock().unwrap_or_else(PoisonError::into_inner).remove(&result.run.run_id);
        let Some((trace_id, span_id)) = ids else { return };
        let workflow = self.inner.workflow().name;
        for assertion in &result.assertions {
            let mut evaluation = EvaluationResult::new(assertion.metric_name());
            evaluation.score = Some(assertion.score);
            evaluation.label = Some(assertion.verdict().as_str().to_owned());
            evaluation.explanation = Some(assertion.reason.clone());
            evaluation.source = Some("offline".to_owned());
            evaluation.kind = Some(assertion.kind.clone());
            self.telemetry.record_evaluation((Some(&trace_id), Some(&span_id)), &workflow, &evaluation, None);
        }
    }
}

impl Target for TracedTarget {
    fn run<'a>(&'a self, case: &'a TestCase) -> BoxFuture<'a, Result<RunCapture, EvalError>> {
        Box::pin(async move { Ok(self.run_traced(case).await) })
    }

    fn workflow(&self) -> WorkflowInfo {
        self.inner.workflow()
    }

    fn start_variables(&self) -> Vec<(String, String)> {
        self.inner.start_variables()
    }
}
