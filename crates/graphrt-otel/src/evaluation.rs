//! Evaluation results as spans: one `evaluate {name}` span per score, carrying
//! a `gen_ai.evaluation.result` event.
//!
//! Online evaluation runs after the evaluated run finished, when its spans are
//! already exported, so the result cannot be an event on the run's own span.
//! Instead it becomes a new child span of the run's root span (from the trace
//! and span ids the run record keeps), in the same trace. The GenAI
//! conventions define `gen_ai.evaluation.result` as a log event; graphrt's
//! exporter has no logs signal, so it is sent as a span event.

use crate::semconv::{self, gen_ai, graphrt as gr};
use crate::span::{AttrValue, Attributes, Span, SpanId, TraceId, truncate};

/// Largest explanation kept, in bytes.
pub const MAX_EXPLANATION_BYTES: usize = 1024;

/// One evaluation score of a run.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct EvaluationResult {
    /// Evaluator (score) name: `gen_ai.evaluation.name`.
    pub name: String,
    /// `gen_ai.evaluation.score.value`.
    pub score: Option<f64>,
    /// `gen_ai.evaluation.score.label`, e.g. `pass` / `fail`.
    pub label: Option<String>,
    /// `gen_ai.evaluation.explanation` (cut at [`MAX_EXPLANATION_BYTES`]).
    pub explanation: Option<String>,
    /// `online`, `human` or `offline` (`graphrt.evaluation.source`).
    pub source: Option<String>,
    /// Assertion type behind the score (`graphrt.evaluation.type`).
    pub kind: Option<String>,
}

impl EvaluationResult {
    /// A result named `name`.
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), ..Self::default() }
    }

    /// The `gen_ai.evaluation.result` event attributes.
    pub fn event_attributes(&self) -> Attributes {
        let mut attributes: Attributes =
            vec![(gen_ai::EVALUATION_NAME.to_owned(), AttrValue::from(self.name.as_str()))];
        if let Some(score) = self.score.filter(|s| s.is_finite()) {
            attributes.push((gen_ai::EVALUATION_SCORE_VALUE.to_owned(), AttrValue::Double(score)));
        }
        if let Some(label) = &self.label {
            attributes.push((gen_ai::EVALUATION_SCORE_LABEL.to_owned(), AttrValue::from(label.as_str())));
        }
        if let Some(explanation) = self.explanation.as_deref().filter(|e| !e.is_empty()) {
            let text = truncate(explanation, MAX_EXPLANATION_BYTES);
            attributes.push((gen_ai::EVALUATION_EXPLANATION.to_owned(), AttrValue::Str(text)));
        }
        attributes
    }
}

/// An `evaluate {name}` span, child of `parent` (the evaluated run's root span)
/// in `trace_id`, lasting from `start_unix_nano` to `end_unix_nano`, with one
/// `gen_ai.evaluation.result` event.
pub fn evaluation_span(
    trace_id: TraceId,
    parent: Option<SpanId>,
    result: &EvaluationResult,
    start_unix_nano: u64,
    end_unix_nano: u64,
) -> Span {
    let name = semconv::span_name(semconv::EVALUATE_SPAN, Some(&result.name));
    let mut span = Span::new(trace_id, parent, name, start_unix_nano);
    span.end_unix_nano = end_unix_nano.max(start_unix_nano);
    span.set(gen_ai::EVALUATION_NAME, result.name.as_str());
    if let Some(source) = &result.source {
        span.set(gr::EVALUATION_SOURCE, source.as_str());
    }
    if let Some(kind) = &result.kind {
        span.set(gr::EVALUATION_TYPE, kind.as_str());
    }
    span.add_event(semconv::EVALUATION_RESULT_EVENT, span.end_unix_nano, result.event_attributes());
    span
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_carries_the_evaluation_event() {
        let trace = TraceId::random();
        let parent = SpanId::random();
        let mut result = EvaluationResult::new("helpfulness");
        result.score = Some(0.8);
        result.label = Some("pass".into());
        result.explanation = Some("x".repeat(5000));
        result.source = Some("online".into());
        result.kind = Some("judge".into());
        let span = evaluation_span(trace, Some(parent), &result, 10, 5);
        assert_eq!(span.name, "evaluate helpfulness");
        assert_eq!((span.trace_id, span.parent_span_id), (trace, Some(parent)));
        assert_eq!(span.end_unix_nano, 10, "end is never before start");
        assert_eq!(span.attr(gr::EVALUATION_SOURCE), Some(&AttrValue::from("online")));
        let event = &span.events[0];
        assert_eq!(event.name, "gen_ai.evaluation.result");
        let attr = |key: &str| event.attributes.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        assert_eq!(attr("gen_ai.evaluation.name"), Some(AttrValue::from("helpfulness")));
        assert_eq!(attr("gen_ai.evaluation.score.value"), Some(AttrValue::Double(0.8)));
        assert_eq!(attr("gen_ai.evaluation.score.label"), Some(AttrValue::from("pass")));
        let Some(AttrValue::Str(explanation)) = attr("gen_ai.evaluation.explanation") else { panic!("explanation") };
        assert!(explanation.len() <= MAX_EXPLANATION_BYTES + '…'.len_utf8());
    }

    #[test]
    fn missing_values_are_left_out() {
        let mut result = EvaluationResult::new("tone");
        result.score = Some(f64::NAN);
        result.explanation = Some(String::new());
        let attributes = result.event_attributes();
        assert_eq!(attributes.len(), 1, "{attributes:?}");
    }
}
