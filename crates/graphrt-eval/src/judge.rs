//! LLM judges written as ordinary graphrt workflows.
//!
//! `{type: judge, workflow: judges/faithfulness.yaml}` runs the judge workflow
//! on what the test produced and reads a verdict from its outputs. A judge is
//! versioned by its workflow fingerprint (recorded with every result), runs on
//! any provider (including `replay` cassettes), and is calibrated like any
//! other workflow: an eval whose target is the judge and whose `expected`
//! values are human labels (see `calibration` in the eval file).
//!
//! **Inputs.** The judge's start node receives, by name (only the variables it
//! declares):
//!
//! | Input | Value |
//! | --- | --- |
//! | `output` | The value under test (the run output, or the `transform` path). |
//! | `vars` | The test's vars (an object). |
//! | `expected` | The test's `expected` value (absent when there is none). |
//! | `contexts` | The text of every chunk the run's `knowledge-retrieval` nodes returned, in rank order. |
//! | `trajectory` | The run's agent tool calls: `[{node_id, name, provider?, tool?, arguments, is_error?}]`. |
//! | anything in `inputs` | Extra inputs from the assertion, e.g. `inputs: { question: "{{question}}" }`. |
//!
//! Text inputs (`paragraph`, `text-input`, `string`...) get text: strings as
//! is, a list of strings joined by blank lines, anything else as JSON. `json`,
//! `object` and array inputs get the value itself.
//!
//! **Outputs.** The judge's outputs (or its single object output, e.g. an
//! `llm` node's `structured_output`) carry `verdict: pass | fail | unknown`
//! and/or a numeric `score` in `0..=1`, plus an optional `reasoning`. A verdict
//! decides; a score alone passes at `threshold` (default 0.5). `unknown`
//! fails the assertion unless `unknown: exclude`, which reports it without
//! scoring or gating it.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use futures::future::BoxFuture;
use graphrt_core::event::Usage;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json};

use crate::assertion::{Assertion, AssertionContext, AssertionRegistry, Outcome, brief, only_keys, transform_of};
use crate::capture::RunStatus;
use crate::config::{AssertionSpec, TestCase};
use crate::error::EvalError;
use crate::path::JsonPath;
use crate::report::WorkflowInfo;
use crate::runner::Target;

/// A judge's binary verdict, with an explicit way out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
    /// The judge could not tell (missing context, ambiguous case...).
    Unknown,
}

impl Verdict {
    /// `pass` / `fail` / `unknown` (any case), `yes` / `no`, `true` / `false`.
    pub fn parse(value: &Json) -> Option<Self> {
        match value {
            Json::Bool(pass) => Some(if *pass { Self::Pass } else { Self::Fail }),
            Json::String(text) => match text.trim().to_ascii_lowercase().as_str() {
                "pass" | "passed" | "yes" | "true" => Some(Self::Pass),
                "fail" | "failed" | "no" | "false" => Some(Self::Fail),
                "unknown" | "unsure" => Some(Self::Unknown),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a judge decided, recorded with the assertion result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct JudgeRecord {
    /// The judge workflow file.
    pub workflow: String,
    /// Its `graphrt.workflow.fingerprint`: changes whenever the judge's prompt,
    /// model or schema does.
    pub fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub run_id: String,
    pub latency_ms: u64,
    pub usage: Usage,
}

/// A verdict and/or score read from a judge's outputs.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct Judgement {
    pub verdict: Option<Verdict>,
    pub score: Option<f64>,
    pub reasoning: Option<String>,
}

impl Judgement {
    /// Read `verdict`, `score` and `reasoning` from a judge's outputs, or from
    /// its single output when that is an object (or JSON text of one).
    pub fn read(outputs: &Map<String, Json>) -> Result<Self, String> {
        let has_fields = |map: &Map<String, Json>| map.contains_key("verdict") || map.contains_key("score");
        if has_fields(outputs) {
            return Self::from_fields(outputs);
        }
        let only = match (outputs.len(), outputs.values().next()) {
            (1, Some(Json::String(text))) => serde_json::from_str::<Json>(text).ok(),
            (1, Some(value)) => Some(value.clone()),
            _ => None,
        };
        match only {
            Some(Json::Object(map)) if has_fields(&map) => Self::from_fields(&map),
            _ => {
                let keys: Vec<&str> = outputs.keys().map(String::as_str).collect();
                Err(format!("no `verdict` or `score` in the judge outputs ({})", keys.join(", ")))
            }
        }
    }

    fn from_fields(map: &Map<String, Json>) -> Result<Self, String> {
        let verdict = match map.get("verdict") {
            None | Some(Json::Null) => None,
            Some(raw) => Some(
                Verdict::parse(raw).ok_or_else(|| format!("verdict {} is not pass, fail or unknown", brief(raw)))?,
            ),
        };
        let score = match map.get("score") {
            None | Some(Json::Null) => None,
            Some(raw) => {
                let number = raw.as_f64().or_else(|| raw.as_str().and_then(|s| s.trim().parse().ok()));
                Some(
                    number
                        .filter(|n| (0.0..=1.0).contains(n))
                        .ok_or_else(|| format!("score {} is not a number between 0 and 1", brief(raw)))?,
                )
            }
        };
        if verdict.is_none() && score.is_none() {
            return Err("the judge returned neither a verdict nor a score".into());
        }
        let reasoning = ["reasoning", "reason", "explanation"]
            .iter()
            .find_map(|key| map.get(*key).and_then(Json::as_str))
            .map(str::to_owned);
        Ok(Self { verdict, score, reasoning })
    }

    /// The binary decision: the verdict, else `score >= threshold`.
    pub fn decide(&self, threshold: f64) -> Verdict {
        match (self.verdict, self.score) {
            (Some(verdict), _) => verdict,
            (None, Some(score)) if score + 1e-9 >= threshold => Verdict::Pass,
            _ => Verdict::Fail,
        }
    }
}

/// What to do with a judge's `unknown` verdict.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownPolicy {
    /// Count it as a failure (the default for judge assertions).
    #[default]
    Fail,
    /// Report it, but leave it out of scores, gates and agreement metrics.
    Exclude,
}

impl UnknownPolicy {
    /// The `unknown` key of an assertion or calibration block.
    pub fn from_config(value: Option<&Json>) -> Result<Self, EvalError> {
        match value.and_then(Json::as_str) {
            None if value.is_none() => Ok(Self::Fail),
            Some("fail") => Ok(Self::Fail),
            Some("exclude") => Ok(Self::Exclude),
            _ => Err(EvalError::invalid("`unknown` must be `fail` or `exclude`")),
        }
    }
}

/// Compiles judge workflows for `judge` assertions. Called once per distinct
/// workflow file; the result is shared by every test.
pub trait JudgeLoader: Send + Sync {
    /// Load and compile the judge at `path` (already resolved against the
    /// eval file's directory).
    fn load(&self, path: &Path) -> Result<Arc<dyn Target>, EvalError>;
}

impl<F> JudgeLoader for F
where
    F: Fn(&Path) -> Result<Arc<dyn Target>, EvalError> + Send + Sync,
{
    fn load(&self, path: &Path) -> Result<Arc<dyn Target>, EvalError> {
        self(path)
    }
}

/// A compiled judge with what the harness needs to call it.
struct Judge {
    target: Arc<dyn Target>,
    info: WorkflowInfo,
    /// Declared start variables and their input types; empty = pass every input as is.
    inputs: Vec<(String, String)>,
}

/// Compiled judges by path.
struct Judges {
    base: PathBuf,
    loader: Box<dyn JudgeLoader>,
    cache: Mutex<HashMap<PathBuf, Arc<Judge>>>,
}

impl Judges {
    fn get(&self, workflow: &str) -> Result<Arc<Judge>, EvalError> {
        let path = normalize(&self.base.join(workflow));
        let key = path.canonicalize().unwrap_or_else(|_| path.clone());
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(judge) = cache.get(&key) {
            return Ok(Arc::clone(judge));
        }
        let target = self.loader.load(&path)?;
        let mut info = target.workflow();
        if info.path.is_empty() {
            info.path = path.display().to_string();
        }
        let judge = Arc::new(Judge { inputs: target.start_variables(), target, info });
        cache.insert(key, Arc::clone(&judge));
        Ok(judge)
    }
}

/// Drop `.` and fold `dir/..` (lexically, no file system access), so reports
/// name `evals/judges/x.yaml` rather than `evals/suite/../judges/x.yaml`.
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir if matches!(out.components().next_back(), Some(Component::Normal(_))) => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Register the `judge` assertion type. Judge `workflow` paths are resolved
/// against `base` (the eval file's directory); each judge is compiled once by
/// `loader` and reused by every test.
pub fn register(registry: &mut AssertionRegistry, base: impl Into<PathBuf>, loader: impl JudgeLoader + 'static) {
    let judges = Arc::new(Judges { base: base.into(), loader: Box::new(loader), cache: Mutex::default() });
    registry.register("judge", move |spec| prepare(&judges, spec));
}

fn prepare(judges: &Judges, spec: &AssertionSpec) -> Result<Box<dyn Assertion>, EvalError> {
    only_keys(spec, &["workflow", "inputs", "unknown"])?;
    let workflow = match spec.config.get("workflow") {
        Some(Json::String(path)) if !path.trim().is_empty() => path,
        _ => return Err(EvalError::invalid("`workflow` (the judge workflow file) is required")),
    };
    let inputs = match spec.config.get("inputs") {
        None | Some(Json::Null) => Map::new(),
        Some(Json::Object(map)) => map.clone(),
        Some(_) => return Err(EvalError::invalid("`inputs` must be a mapping of judge start inputs")),
    };
    let threshold = spec.threshold.unwrap_or(0.5);
    if !(0.0..=1.0).contains(&threshold) {
        return Err(EvalError::invalid("`threshold` must be between 0 and 1"));
    }
    Ok(Box::new(JudgeAssertion {
        judge: judges.get(workflow)?,
        inputs,
        transform: transform_of(spec)?,
        unknown: UnknownPolicy::from_config(spec.config.get("unknown"))?,
        threshold,
    }))
}

struct JudgeAssertion {
    judge: Arc<Judge>,
    inputs: Map<String, Json>,
    transform: Option<JsonPath>,
    unknown: UnknownPolicy,
    threshold: f64,
}

impl fmt::Debug for JudgeAssertion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JudgeAssertion").field("workflow", &self.judge.info.path).finish()
    }
}

impl Assertion for JudgeAssertion {
    fn evaluate<'a>(&'a self, ctx: AssertionContext<'a>) -> BoxFuture<'a, Outcome> {
        Box::pin(self.judge(ctx))
    }
}

impl JudgeAssertion {
    async fn judge(&self, ctx: AssertionContext<'_>) -> Outcome {
        let Some(subject) = ctx.subject(self.transform.as_ref()) else {
            return Outcome::check(false, "transform path not found in the run");
        };
        let mut record = JudgeRecord {
            workflow: self.judge.info.path.clone(),
            fingerprint: self.judge.info.fingerprint.clone(),
            ..JudgeRecord::default()
        };
        let case = TestCase::new(format!("{}:judge", ctx.case.id), self.judge_inputs(ctx, subject));
        let capture = match self.judge.target.run(&case).await {
            Ok(capture) => capture,
            Err(e) => return Outcome::check(false, format!("judge could not run: {e}")).with_judge(record),
        };
        record.run_id.clone_from(&capture.run_id);
        record.latency_ms = capture.latency_ms;
        record.usage = capture.usage;
        if capture.status != RunStatus::Succeeded {
            let error = capture.error.as_deref().unwrap_or("no details");
            return Outcome::check(false, format!("judge run did not succeed: {error}")).with_judge(record);
        }
        let judgement = match Judgement::read(&capture.outputs) {
            Ok(judgement) => judgement,
            Err(why) => return Outcome::check(false, format!("judge: {why}")).with_judge(record),
        };
        let verdict = judgement.decide(self.threshold);
        let reason = match (&judgement.reasoning, judgement.score) {
            (Some(why), _) => format!("judge: {verdict}: {}", shorten(why)),
            (None, Some(score)) => format!("judge: {verdict} (score {score:.2})"),
            (None, None) => format!("judge: {verdict}"),
        };
        record.verdict = judgement.verdict;
        record.score = judgement.score;
        record.reasoning = judgement.reasoning;
        let outcome = match (verdict, self.unknown) {
            (Verdict::Unknown, UnknownPolicy::Exclude) => Outcome::excluded(format!("{reason} (excluded)")),
            (Verdict::Pass, _) => Outcome::graded(true, judgement.score.unwrap_or(1.0), reason),
            _ => Outcome::graded(false, judgement.score.unwrap_or(0.0), reason),
        };
        outcome.with_judge(record)
    }

    /// The judge's start inputs: the input contract (module docs), then the
    /// assertion's `inputs`, typed for the variables the judge declares.
    fn judge_inputs(&self, ctx: AssertionContext<'_>, subject: Json) -> Map<String, Json> {
        let mut all = Map::new();
        all.insert("output".into(), subject);
        all.insert("vars".into(), Json::Object(ctx.case.vars.clone()));
        if let Some(expected) = &ctx.case.expected {
            all.insert("expected".into(), expected.clone());
        }
        let hits = ctx.capture.retrieval_hits();
        if !hits.is_empty() {
            all.insert("contexts".into(), hits.into_iter().map(|h| Json::String(h.content)).collect());
        }
        let calls = ctx.capture.tool_calls();
        all.insert("trajectory".into(), serde_json::to_value(calls).unwrap_or_default());
        all.extend(self.inputs.clone());
        if self.judge.inputs.is_empty() {
            return all;
        }
        self.judge
            .inputs
            .iter()
            .filter_map(|(name, kind)| all.remove(name).map(|value| (name.clone(), typed_input(kind, value))))
            .collect()
    }
}

/// Shape a value for a start variable of input type `kind`.
fn typed_input(kind: &str, value: Json) -> Json {
    match (kind, value) {
        ("text-input" | "paragraph" | "select" | "string" | "text", value) => Json::String(as_text(value)),
        // `json` inputs parse strings as JSON: send a string as a JSON string literal.
        ("json", Json::String(text)) => Json::String(Json::String(text).to_string()),
        (_, value) => value,
    }
}

fn as_text(value: Json) -> String {
    match value {
        Json::String(text) => text,
        Json::Null => String::new(),
        Json::Array(items) if items.iter().all(Json::is_string) => {
            items.iter().filter_map(Json::as_str).collect::<Vec<_>>().join("\n\n")
        }
        other => serde_json::to_string_pretty(&other).unwrap_or_default(),
    }
}

/// The first 200 characters of a judge's reasoning, on one line.
fn shorten(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= 200 { line } else { format!("{}...", line.chars().take(197).collect::<String>()) }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use serde_json::json;

    fn outputs(value: Json) -> Map<String, Json> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn reads_verdicts_scores_and_nested_outputs() {
        let read = |v: Json| Judgement::read(&outputs(v));
        let j = read(json!({"verdict": "PASS", "reasoning": "grounded"})).unwrap();
        assert_eq!((j.verdict, j.reasoning.as_deref()), (Some(Verdict::Pass), Some("grounded")));
        let j = read(json!({"result": {"verdict": "unknown", "explanation": "no context"}})).unwrap();
        assert_eq!(j.verdict, Some(Verdict::Unknown));
        assert_eq!(j.reasoning.as_deref(), Some("no context"));
        let j = read(json!({"text": "{\"score\": 0.4}"})).unwrap();
        assert_eq!((j.verdict, j.score), (None, Some(0.4)));
        assert_eq!(j.decide(0.5), Verdict::Fail);
        assert_eq!(j.decide(0.4), Verdict::Pass);
        assert_eq!(read(json!({"verdict": true})).unwrap().verdict, Some(Verdict::Pass));
        assert!(read(json!({"verdict": "maybe"})).unwrap_err().contains("not pass, fail or unknown"));
        assert!(read(json!({"score": 3})).unwrap_err().contains("between 0 and 1"));
        assert!(read(json!({"a": 1, "b": 2})).unwrap_err().contains("no `verdict`"));
        assert!(read(json!({"verdict": null})).unwrap_err().contains("neither"));
    }

    #[test]
    fn inputs_are_typed_for_the_declared_variables() {
        assert_eq!(typed_input("paragraph", json!(["a", "b"])), json!("a\n\nb"));
        assert_eq!(typed_input("paragraph", json!({"k": 1})), json!("{\n  \"k\": 1\n}"));
        assert_eq!(typed_input("json", json!("hi")), json!("\"hi\""));
        assert_eq!(typed_input("json", json!({"k": 1})), json!({"k": 1}));
        assert_eq!(typed_input("array[string]", json!(["a"])), json!(["a"]));
    }

    #[test]
    fn unknown_policy_parses() {
        assert_eq!(UnknownPolicy::from_config(None).unwrap(), UnknownPolicy::Fail);
        assert_eq!(UnknownPolicy::from_config(Some(&json!("exclude"))).unwrap(), UnknownPolicy::Exclude);
        assert!(UnknownPolicy::from_config(Some(&json!("skip"))).is_err());
        assert!(UnknownPolicy::from_config(Some(&json!(null))).is_err());
    }
}
