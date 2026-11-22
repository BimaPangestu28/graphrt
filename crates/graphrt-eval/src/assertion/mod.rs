//! Assertions: the [`Assertion`] trait, the registry of assertion types and the
//! built-in deterministic types.
//!
//! An assertion type is a factory turning an [`AssertionSpec`] (one `assert`
//! entry, with `{{var}}` references already rendered) into an [`Assertion`].
//! Evaluation is asynchronous so types that call models or run workflows (an
//! LLM judge) fit the same interface as the string checks.
//!
//! Every type can be negated with a `not-` prefix (`not-contains`,
//! `not-trajectory:tool-used`): the pass flag flips and the score becomes
//! `1 - score`.

mod perf;
mod retrieval;
mod text;
mod trajectory;
mod verdict;

use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::capture::RunCapture;
use crate::config::{AssertionSpec, TestCase};
use crate::error::EvalError;
use crate::judge::{JudgeRecord, Verdict};
use crate::path::JsonPath;

/// What an assertion sees: the test case and the run it produced.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct AssertionContext<'a> {
    pub case: &'a TestCase,
    pub capture: &'a RunCapture,
}

impl<'a> AssertionContext<'a> {
    pub fn new(case: &'a TestCase, capture: &'a RunCapture) -> Self {
        Self { case, capture }
    }

    /// The value under test: the run's [`RunCapture::output`], or the value at
    /// `transform` (resolved against [`RunCapture::subject_root`]).
    pub fn subject(&self, transform: Option<&JsonPath>) -> Option<Json> {
        match transform {
            None => Some(self.capture.output()),
            Some(path) => path.get(&self.capture.subject_root(&self.case.vars)),
        }
    }
}

/// The verdict of one assertion.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Outcome {
    pub pass: bool,
    /// In `0..=1`.
    pub score: f64,
    pub reason: String,
    /// Leave the result out of the test score, the gate and the metric means
    /// (a judge's `unknown` verdict with `unknown: exclude`).
    pub excluded: bool,
    /// What a judge workflow decided (judge assertions only).
    pub judge: Option<Box<JudgeRecord>>,
}

impl Outcome {
    /// A binary verdict (score 1 or 0).
    pub fn check(pass: bool, reason: impl Into<String>) -> Self {
        Self::graded(pass, if pass { 1.0 } else { 0.0 }, reason)
    }

    /// A graded verdict; `score` is clamped to `0..=1` (NaN becomes 0).
    pub fn graded(pass: bool, score: f64, reason: impl Into<String>) -> Self {
        let score = if score.is_nan() { 0.0 } else { score.clamp(0.0, 1.0) };
        Self { pass, score, reason: reason.into(), excluded: false, judge: None }
    }

    /// A result that is reported but neither scored nor gated.
    pub fn excluded(reason: impl Into<String>) -> Self {
        Self { excluded: true, ..Self::check(false, reason) }
    }

    /// Attach the judge's details.
    pub fn with_judge(mut self, record: JudgeRecord) -> Self {
        self.judge = Some(Box::new(record));
        self
    }
}

/// A check of one aspect of a run.
pub trait Assertion: Send + Sync + fmt::Debug {
    /// Evaluate against a finished run.
    fn evaluate<'a>(&'a self, ctx: AssertionContext<'a>) -> BoxFuture<'a, Outcome>;
}

/// Builds an assertion from its spec (the `not-` prefix already removed).
pub type AssertionFactory = Arc<dyn Fn(&AssertionSpec) -> Result<Box<dyn Assertion>, EvalError> + Send + Sync>;

/// The result of one assertion, as reported.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AssertionResult {
    /// The assertion type as written (with any `not-` prefix).
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric: Option<String>,
    pub pass: bool,
    pub score: f64,
    pub weight: f64,
    pub reason: String,
    /// Reported only: not part of the test score, the gate or the metric means.
    #[serde(default, skip_serializing_if = "is_false")]
    pub excluded: bool,
    /// The judge workflow (path and fingerprint) and its verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge: Option<Box<JudgeRecord>>,
}

fn is_false(flag: &bool) -> bool {
    !*flag
}

impl AssertionResult {
    /// A result with no judge record, not excluded.
    pub fn new(kind: impl Into<String>, pass: bool, score: f64, weight: f64, reason: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            metric: None,
            pass,
            score,
            weight,
            reason: reason.into(),
            excluded: false,
            judge: None,
        }
    }

    /// Whether this result counts towards the test verdict.
    pub fn gates(&self) -> bool {
        self.weight > 0.0 && !self.excluded
    }

    /// The name the summary aggregates this result under: `metric`, else the type.
    pub fn metric_name(&self) -> &str {
        self.metric.as_deref().unwrap_or(&self.kind)
    }

    /// The result as a score label: `unknown` when excluded (it decided
    /// nothing), else `pass` or `fail`. Offline trace evaluations and online
    /// scores label results the same way through this.
    pub fn verdict(&self) -> Verdict {
        if self.excluded {
            Verdict::Unknown
        } else if self.pass {
            Verdict::Pass
        } else {
            Verdict::Fail
        }
    }
}

/// An assertion ready to evaluate, with its reporting settings.
#[derive(Debug)]
pub struct PreparedAssertion {
    kind: String,
    metric: Option<String>,
    weight: f64,
    negated: bool,
    inner: Box<dyn Assertion>,
}

impl PreparedAssertion {
    /// Evaluate and wrap the outcome into a reported result.
    pub async fn run(&self, ctx: AssertionContext<'_>) -> AssertionResult {
        let mut outcome = self.inner.evaluate(ctx).await;
        if self.negated && !outcome.excluded {
            outcome.pass = !outcome.pass;
            outcome.score = 1.0 - outcome.score;
            outcome.reason = format!("negated: {}", outcome.reason);
        }
        AssertionResult {
            kind: self.kind.clone(),
            metric: self.metric.clone(),
            pass: outcome.pass,
            score: outcome.score,
            weight: self.weight,
            reason: outcome.reason,
            excluded: outcome.excluded,
            judge: outcome.judge,
        }
    }

    /// Weight in the test score (`0` = report only).
    pub fn weight(&self) -> f64 {
        self.weight
    }
}

/// Assertion types by name.
#[derive(Clone)]
pub struct AssertionRegistry {
    factories: IndexMap<String, AssertionFactory>,
}

impl fmt::Debug for AssertionRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AssertionRegistry").field("types", &self.factories.keys().collect::<Vec<_>>()).finish()
    }
}

impl Default for AssertionRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

impl AssertionRegistry {
    /// No types at all.
    pub fn empty() -> Self {
        Self { factories: IndexMap::new() }
    }

    /// The built-in deterministic, performance, trajectory, retrieval and
    /// `verdict` types. `judge` needs a workflow loader: see [`crate::judge::register`].
    pub fn with_builtins() -> Self {
        let mut registry = Self::empty();
        text::register(&mut registry);
        perf::register(&mut registry);
        trajectory::register(&mut registry);
        retrieval::register(&mut registry);
        verdict::register(&mut registry);
        registry
    }

    /// Register (or replace) an assertion type. Names must not start with `not-`.
    pub fn register(
        &mut self,
        kind: impl Into<String>,
        factory: impl Fn(&AssertionSpec) -> Result<Box<dyn Assertion>, EvalError> + Send + Sync + 'static,
    ) -> &mut Self {
        self.factories.insert(kind.into(), Arc::new(factory));
        self
    }

    /// Registered type names.
    pub fn kinds(&self) -> impl Iterator<Item = &str> {
        self.factories.keys().map(String::as_str)
    }

    /// Build an assertion from a spec whose values are already rendered.
    pub fn prepare(&self, spec: &AssertionSpec) -> Result<PreparedAssertion, EvalError> {
        let (negated, base) = match spec.kind.strip_prefix("not-") {
            Some(base) => (true, base),
            None => (false, spec.kind.as_str()),
        };
        let factory = self.factories.get(base).ok_or_else(|| {
            let known: Vec<&str> = self.kinds().collect();
            EvalError::invalid(format!("unknown assertion type `{}` (known: {})", spec.kind, known.join(", ")))
        })?;
        let mut base_spec = spec.clone();
        base_spec.kind = base.to_owned();
        let inner = factory(&base_spec).map_err(|e| EvalError::invalid(format!("assertion `{}`: {e}", spec.kind)))?;
        Ok(PreparedAssertion {
            kind: spec.kind.clone(),
            metric: spec.metric.clone(),
            weight: spec.weight,
            negated,
            inner,
        })
    }
}

/// Reject `config` keys other than `allowed` (typos would otherwise be ignored).
pub fn only_keys(spec: &AssertionSpec, allowed: &[&str]) -> Result<(), EvalError> {
    match spec.config.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(key) => Err(EvalError::invalid(format!("unknown key `{key}`"))),
        None => Ok(()),
    }
}

/// The parsed `transform` path of a spec.
pub fn transform_of(spec: &AssertionSpec) -> Result<Option<JsonPath>, EvalError> {
    spec.transform.as_deref().map(JsonPath::parse).transpose()
}

/// Display a JSON value briefly for reasons.
pub(crate) fn brief(value: &Json) -> String {
    let text = match value {
        Json::String(s) => format!("{s:?}"),
        other => other.to_string(),
    };
    if text.chars().count() > 120 {
        let cut: String = text.chars().take(117).collect();
        format!("{cut}...")
    } else {
        text
    }
}

/// An assertion implemented by a synchronous function.
pub(crate) struct FnAssertion<F> {
    name: &'static str,
    check: F,
}

impl<F> fmt::Debug for FnAssertion<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnAssertion").field("name", &self.name).finish()
    }
}

impl<F> Assertion for FnAssertion<F>
where
    F: Fn(AssertionContext<'_>) -> Outcome + Send + Sync,
{
    fn evaluate<'a>(&'a self, ctx: AssertionContext<'a>) -> BoxFuture<'a, Outcome> {
        let outcome = (self.check)(ctx);
        Box::pin(std::future::ready(outcome))
    }
}

/// Box a synchronous check as an [`Assertion`].
pub(crate) fn sync_assertion<F>(name: &'static str, check: F) -> Box<dyn Assertion>
where
    F: Fn(AssertionContext<'_>) -> Outcome + Send + Sync + 'static,
{
    Box::new(FnAssertion { name, check })
}

#[cfg(test)]
pub(crate) mod test_support {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// Evaluate a spec (given as JSON) against a capture.
    pub fn eval(spec: Json, capture: &RunCapture) -> AssertionResult {
        try_eval(spec, capture).unwrap()
    }

    pub fn try_eval(spec: Json, capture: &RunCapture) -> Result<AssertionResult, EvalError> {
        let spec = AssertionSpec::from_json(spec)?;
        let prepared = AssertionRegistry::with_builtins().prepare(&spec)?;
        let case = TestCase::new("t", serde_json::Map::new());
        let ctx = AssertionContext::new(&case, capture);
        Ok(tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(prepared.run(ctx)))
    }

    /// A capture whose answer is `answer`.
    pub fn answered(answer: &str) -> RunCapture {
        RunCapture { answer: answer.to_owned(), ..RunCapture::default() }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::test_support::*;
    use super::*;
    use serde_json::json;

    #[test]
    fn negation_weights_and_metrics() {
        let capture = answered("hello world");
        let result = eval(json!({"type": "not-contains", "value": "bye", "weight": 0, "metric": "polite"}), &capture);
        assert!(result.pass && result.score == 1.0, "{result:?}");
        assert_eq!(result.weight, 0.0);
        assert_eq!(result.metric_name(), "polite");
        assert_eq!(result.kind, "not-contains");
        let result = eval(json!({"type": "not-contains", "value": "hello"}), &capture);
        assert!(!result.pass && result.score == 0.0);
        assert_eq!(result.metric_name(), "not-contains");
    }

    #[test]
    fn verdict_labels_excluded_results_unknown() {
        let mut result = AssertionResult::new("contains", true, 1.0, 1.0, "");
        assert_eq!(result.verdict(), Verdict::Pass);
        result.pass = false;
        assert_eq!(result.verdict(), Verdict::Fail);
        result.excluded = true;
        assert_eq!(result.verdict(), Verdict::Unknown);
        result.pass = true;
        assert_eq!(result.verdict().as_str(), "unknown", "excluded wins over pass");
    }

    #[test]
    fn unknown_types_and_keys_are_rejected() {
        let capture = answered("x");
        let err = try_eval(json!({"type": "llm-rubric", "value": "x"}), &capture).unwrap_err().to_string();
        assert!(err.contains("unknown assertion type `llm-rubric`") && err.contains("equals"), "{err}");
        let err = try_eval(json!({"type": "contains", "value": "x", "k": 3}), &capture).unwrap_err().to_string();
        assert!(err.contains("unknown key `k`"), "{err}");
    }

    #[test]
    fn custom_types_can_be_registered() {
        #[derive(Debug)]
        struct Always;
        impl Assertion for Always {
            fn evaluate<'a>(&'a self, _ctx: AssertionContext<'a>) -> BoxFuture<'a, Outcome> {
                Box::pin(async { Outcome::graded(true, 0.75, "judged") })
            }
        }
        let mut registry = AssertionRegistry::with_builtins();
        registry.register("judge", |_spec| Ok(Box::new(Always) as Box<dyn Assertion>));
        let prepared = registry.prepare(&AssertionSpec::new("judge", None)).unwrap();
        let case = TestCase::new("t", serde_json::Map::new());
        let capture = answered("x");
        let result = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(prepared.run(AssertionContext::new(&case, &capture)));
        assert_eq!(result.score, 0.75);
    }
}
