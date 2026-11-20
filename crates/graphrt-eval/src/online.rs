//! Online evaluation: score a sample of production runs after they finish.
//!
//! An evaluator config (`graphrt serve --evaluators <file>`; the workflow
//! format is frozen, so it cannot live in the workflow) maps served workflows
//! to evaluators. Each evaluator is one assertion of the offline harness
//! (`judge`, `regex`, `json-schema`, `latency`, `tokens`, `trajectory:*`...)
//! that needs no `expected` answer, plus a deterministic sample rate and
//! filters on the run's status, tags and metadata:
//!
//! ```yaml
//! concurrency: 2        # evaluations running at once (default 2)
//! queue: 256            # sampled runs waiting; more are dropped and counted (default 256)
//! evaluators:
//!   - name: helpfulness               # the score name
//!     workflows: [chat]               # served workflow names, `*` wildcards (default: all)
//!     sample: 0.1                     # fraction of matching runs (default 1)
//!     status: [succeeded]             # default: succeeded and failed
//!     tags: [prod]                    # the run has at least one of these tags
//!     metadata: { tier: gold }        # and every one of these metadata values
//!     assert: { type: judge, workflow: ../judges/helpfulness.yaml }
//!   - name: no-apology
//!     assert: { type: not-icontains, value: "as an ai" }
//! ```
//!
//! Sampling hashes the run id with the evaluator name, so whether a run is
//! scored is reproducible (the same run id always gives the same decision)
//! and evaluators sample independently. Relative paths (judge workflows) are
//! resolved against the config file's directory.

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde_json::{Map, Value as Json};

use crate::assertion::{AssertionContext, AssertionRegistry, AssertionResult, PreparedAssertion};
use crate::capture::{RunCapture, RunStatus};
use crate::config::{AssertionSpec, TestCase, parse_structured};
use crate::error::EvalError;
use crate::path::render;

/// Default evaluations running at once.
pub const DEFAULT_CONCURRENCY: usize = 2;
/// Default sampled runs waiting for an evaluation slot.
pub const DEFAULT_QUEUE: usize = 256;
/// Upper bounds on the pool settings.
const MAX_CONCURRENCY: usize = 256;
const MAX_QUEUE: usize = 100_000;

/// Assertion types that make sense on a production run (no `expected`
/// answer, no labelled documents). `not-` forms are allowed too.
pub const ONLINE_TYPES: &[&str] = &[
    "judge",
    "regex",
    "contains",
    "icontains",
    "contains-any",
    "icontains-any",
    "contains-all",
    "icontains-all",
    "starts-with",
    "is-json",
    "json-schema",
    "latency",
    "tokens",
    "trajectory:tool-used",
    "trajectory:tool-sequence",
    "trajectory:tool-args-match",
    "trajectory:step-count",
];

/// A parsed evaluator config.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct OnlineConfig {
    /// Where the config was loaded from (judge paths are relative to its directory).
    pub path: PathBuf,
    pub concurrency: usize,
    pub queue: usize,
    pub evaluators: Vec<EvaluatorSpec>,
}

/// One evaluator as written.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct EvaluatorSpec {
    /// Score name, unique in the file.
    pub name: String,
    /// Workflow name patterns (`*` wildcards); empty = every workflow.
    pub workflows: Vec<String>,
    /// Fraction of matching runs to score, `0..=1`.
    pub sample: f64,
    pub status: Vec<RunStatus>,
    /// Any of these tags (empty = no tag filter).
    pub tags: Vec<String>,
    /// Every one of these metadata values.
    pub metadata: IndexMap<String, String>,
    pub assertion: AssertionSpec,
}

/// What an evaluator filter looks at: the finished run's record.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct RunInfo<'a> {
    pub run_id: &'a str,
    pub workflow: &'a str,
    pub status: RunStatus,
    pub tags: &'a [String],
    pub metadata: &'a IndexMap<String, String>,
}

impl<'a> RunInfo<'a> {
    pub fn new(
        run_id: &'a str,
        workflow: &'a str,
        status: RunStatus,
        tags: &'a [String],
        metadata: &'a IndexMap<String, String>,
    ) -> Self {
        Self { run_id, workflow, status, tags, metadata }
    }
}

/// The raw config, read by hand from the parsed YAML / JSON: derived
/// deserializers would add code to every binary that serves.
type Object = Map<String, Json>;

/// Reject keys of `object` other than `allowed` (typos would silently do nothing).
fn known_keys(object: &Object, allowed: &[&str]) -> Result<(), EvalError> {
    match object.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(key) => Err(EvalError::invalid(format!("unknown field `{key}`, expected one of {}", allowed.join(", ")))),
        None => Ok(()),
    }
}

fn count_field(object: &Object, key: &str, default: usize) -> Result<usize, EvalError> {
    match object.get(key) {
        None | Some(Json::Null) => Ok(default),
        Some(value) => value
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| EvalError::invalid(format!("`{key}` must be a positive integer"))),
    }
}

fn strings_field(object: &Object, key: &str) -> Result<Vec<String>, EvalError> {
    let list = match object.get(key) {
        None | Some(Json::Null) => return Ok(Vec::new()),
        Some(Json::Array(items)) => items,
        Some(_) => return Err(EvalError::invalid(format!("`{key}` must be a list of strings"))),
    };
    list.iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| EvalError::invalid(format!("`{key}` must be a list of strings")))
        })
        .collect()
}

fn status_of(name: &str) -> Option<RunStatus> {
    match name {
        "succeeded" => Some(RunStatus::Succeeded),
        "failed" => Some(RunStatus::Failed),
        "aborted" => Some(RunStatus::Aborted),
        "paused" => Some(RunStatus::Paused),
        _ => None,
    }
}

impl OnlineConfig {
    /// Load an evaluator config (YAML or JSON).
    ///
    /// @throws EvalError - When the file cannot be read or is invalid.
    pub fn load(path: &Path) -> Result<Self, EvalError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| EvalError::Io(format!("cannot read evaluator config `{}`: {e}", path.display())))?;
        Self::parse(&text)
            .map(|mut config| {
                config.path = path.to_path_buf();
                config
            })
            .map_err(|e| match e {
                EvalError::Invalid(m) => EvalError::Invalid(format!("{}: {m}", path.display())),
                other => other,
            })
    }

    /// Parse evaluator config text.
    ///
    /// @throws EvalError - On unknown keys, duplicate names, a sample rate
    ///   outside `0..=1`, or an assertion type that needs an expected answer.
    pub fn parse(text: &str) -> Result<Self, EvalError> {
        let Json::Object(raw) = parse_structured(text)? else {
            return Err(EvalError::invalid("an evaluator config must be a mapping with `evaluators`"));
        };
        known_keys(&raw, &["concurrency", "queue", "evaluators"])?;
        let concurrency = count_field(&raw, "concurrency", DEFAULT_CONCURRENCY)?;
        if !(1..=MAX_CONCURRENCY).contains(&concurrency) {
            return Err(EvalError::invalid(format!("`concurrency` must be 1-{MAX_CONCURRENCY}")));
        }
        let queue = count_field(&raw, "queue", DEFAULT_QUEUE)?;
        if !(1..=MAX_QUEUE).contains(&queue) {
            return Err(EvalError::invalid(format!("`queue` must be 1-{MAX_QUEUE}")));
        }
        let list = match raw.get("evaluators") {
            Some(Json::Array(list)) if !list.is_empty() => list,
            Some(Json::Array(_)) => return Err(EvalError::invalid("`evaluators` is empty")),
            _ => return Err(EvalError::invalid("`evaluators` (a list) is required")),
        };
        let mut evaluators = Vec::with_capacity(list.len());
        for (index, evaluator) in list.iter().enumerate() {
            let Json::Object(evaluator) = evaluator else {
                return Err(EvalError::invalid(format!("evaluator {}: must be a mapping", index + 1)));
            };
            let name = evaluator.get("name").and_then(Json::as_str).unwrap_or_default().to_owned();
            let spec = evaluator_spec(evaluator).map_err(|e| {
                let label = if name.is_empty() { format!("{}", index + 1) } else { format!("`{name}`") };
                EvalError::invalid(format!("evaluator {label}: {e}"))
            })?;
            if evaluators.iter().any(|e: &EvaluatorSpec| e.name == spec.name) {
                return Err(EvalError::invalid(format!("duplicate evaluator name `{}`", spec.name)));
            }
            evaluators.push(spec);
        }
        Ok(Self { path: PathBuf::new(), concurrency, queue, evaluators })
    }

    /// The directory judge workflow paths are resolved against.
    pub fn base_dir(&self) -> PathBuf {
        self.path.parent().map(Path::to_path_buf).unwrap_or_default()
    }

    /// Workflow patterns that match none of `served` (likely typos).
    pub fn unmatched_workflows<'a>(&'a self, served: &[&str]) -> Vec<&'a str> {
        self.evaluators
            .iter()
            .flat_map(|e| e.workflows.iter())
            .filter(|pattern| !served.iter().any(|name| crate::glob::matches(pattern, name)))
            .map(String::as_str)
            .collect()
    }
}

fn evaluator_spec(raw: &Object) -> Result<EvaluatorSpec, EvalError> {
    known_keys(raw, &["name", "workflows", "sample", "status", "tags", "metadata", "assert"])?;
    let name = raw.get("name").and_then(Json::as_str).unwrap_or_default().trim().to_owned();
    if name.is_empty() || name.len() > 200 {
        return Err(EvalError::invalid("`name` must be 1-200 characters"));
    }
    let sample = match raw.get("sample") {
        None | Some(Json::Null) => 1.0,
        Some(value) => value.as_f64().ok_or_else(|| EvalError::invalid("`sample` must be a number"))?,
    };
    if !(0.0..=1.0).contains(&sample) {
        return Err(EvalError::invalid("`sample` must be between 0 and 1"));
    }
    let status = match raw.get("status") {
        None | Some(Json::Null) => vec![RunStatus::Succeeded, RunStatus::Failed],
        Some(_) => strings_field(raw, "status")?
            .iter()
            .map(|s| {
                status_of(s).ok_or_else(|| {
                    EvalError::invalid(format!("unknown status `{s}` (expected succeeded, failed or aborted)"))
                })
            })
            .collect::<Result<_, _>>()?,
    };
    if status.contains(&RunStatus::Paused) {
        return Err(EvalError::invalid("`status` cannot include `paused`: paused runs have not finished"));
    }
    if status.is_empty() {
        return Err(EvalError::invalid("`status` is empty"));
    }
    let metadata = match raw.get("metadata") {
        None | Some(Json::Null) => IndexMap::new(),
        Some(Json::Object(map)) => map
            .iter()
            .map(|(k, v)| match v {
                Json::String(text) => Ok((k.clone(), text.clone())),
                _ => Err(EvalError::invalid(format!("`metadata.{k}` must be a string"))),
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(EvalError::invalid("`metadata` must be a mapping of strings")),
    };
    let assertion = raw.get("assert").cloned().ok_or_else(|| EvalError::invalid("`assert` is required"))?;
    let assertion = AssertionSpec::from_json(assertion)?;
    check_online(&assertion)?;
    Ok(EvaluatorSpec {
        name,
        workflows: strings_field(raw, "workflows")?,
        sample,
        status,
        tags: strings_field(raw, "tags")?,
        metadata,
        assertion,
    })
}

/// Reject assertion types and values that need a labelled test case.
fn check_online(spec: &AssertionSpec) -> Result<(), EvalError> {
    let base = spec.kind.strip_prefix("not-").unwrap_or(&spec.kind);
    if !ONLINE_TYPES.contains(&base) {
        return Err(EvalError::invalid(format!(
            "assertion type `{}` needs an expected answer or labels and cannot run online (online types: {})",
            spec.kind,
            ONLINE_TYPES.join(", ")
        )));
    }
    let values = spec.value.iter().chain(spec.config.values());
    if values.into_iter().any(mentions_expected) {
        return Err(EvalError::invalid(
            "`{{expected}}` is not available online: production runs have no expected answer",
        ));
    }
    Ok(())
}

fn mentions_expected(value: &Json) -> bool {
    match value {
        Json::String(text) => text.split("{{").skip(1).any(|rest| rest.trim_start().starts_with("expected")),
        Json::Array(items) => items.iter().any(mentions_expected),
        Json::Object(map) => map.values().any(mentions_expected),
        _ => false,
    }
}

fn has_template(spec: &AssertionSpec) -> bool {
    fn walk(value: &Json) -> bool {
        match value {
            Json::String(text) => text.contains("{{"),
            Json::Array(items) => items.iter().any(walk),
            Json::Object(map) => map.values().any(walk),
            _ => false,
        }
    }
    spec.value.iter().chain(spec.config.values()).any(walk)
}

impl EvaluatorSpec {
    /// Whether a finished run passes this evaluator's filters (not the sampling).
    pub fn matches(&self, run: &RunInfo<'_>) -> bool {
        self.status.contains(&run.status)
            && (self.workflows.is_empty() || self.workflows.iter().any(|p| crate::glob::matches(p, run.workflow)))
            && (self.tags.is_empty() || self.tags.iter().any(|t| run.tags.contains(t)))
            && self.metadata.iter().all(|(k, v)| run.metadata.get(k) == Some(v))
    }

    /// Whether `run_id` falls in this evaluator's sample.
    pub fn sampled(&self, run_id: &str) -> bool {
        sample_point(run_id, &self.name) < self.sample
    }
}

/// A run's deterministic position in `[0, 1)` for evaluator `name`: FNV-1a of
/// `run_id`, a separator and `name`, mixed, scaled to 53 bits.
pub fn sample_point(run_id: &str, name: &str) -> f64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in run_id.bytes().chain(std::iter::once(0)).chain(name.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    // FNV's low bits are weak for short keys; a final avalanche (splitmix64) fixes that.
    hash ^= hash >> 30;
    hash = hash.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    hash ^= hash >> 27;
    hash = hash.wrapping_mul(0x94d0_49bb_1331_11eb);
    hash ^= hash >> 31;
    #[allow(clippy::cast_precision_loss)]
    let point = (hash >> 11) as f64 / (1u64 << 53) as f64;
    point
}

/// An evaluator with its assertion built.
#[derive(Debug)]
pub struct Evaluator {
    pub spec: EvaluatorSpec,
    /// Built once at load, unless the assertion uses `{{...}}` references to
    /// the run (then built per run from `spec.assertion`).
    prepared: Option<PreparedAssertion>,
}

/// Every evaluator of a config, ready to score runs.
#[derive(Debug)]
pub struct OnlineEvaluators {
    evaluators: Vec<Evaluator>,
    registry: AssertionRegistry,
}

impl OnlineEvaluators {
    /// Build every evaluator's assertion with `registry` (built-ins plus
    /// `judge`), compiling judge workflows now so a broken judge fails startup.
    ///
    /// @throws EvalError - When an assertion is invalid.
    pub fn new(config: &OnlineConfig, registry: AssertionRegistry) -> Result<Self, EvalError> {
        let mut evaluators = Vec::with_capacity(config.evaluators.len());
        for spec in &config.evaluators {
            let context = |e: EvalError| EvalError::invalid(format!("evaluator `{}`: {e}", spec.name));
            let prepared = if has_template(&spec.assertion) {
                // Validate the shape with the references left in; values come per run.
                let mut probe = spec.assertion.clone();
                probe.value = None;
                if spec.assertion.kind.trim_start_matches("not-") == "judge" {
                    registry.prepare(&probe).map_err(context)?;
                }
                None
            } else {
                Some(registry.prepare(&spec.assertion).map_err(context)?)
            };
            evaluators.push(Evaluator { spec: spec.clone(), prepared });
        }
        Ok(Self { evaluators, registry })
    }

    pub fn evaluators(&self) -> &[Evaluator] {
        &self.evaluators
    }

    /// Indexes of the evaluators that match `run` and sample it.
    pub fn select(&self, run: &RunInfo<'_>) -> Vec<usize> {
        self.evaluators
            .iter()
            .enumerate()
            .filter(|(_, e)| e.spec.matches(run) && e.spec.sampled(run.run_id))
            .map(|(index, _)| index)
            .collect()
    }

    /// Whether any evaluator could score runs of `workflow` (to skip the
    /// bookkeeping for workflows nobody evaluates).
    pub fn covers(&self, workflow: &str) -> bool {
        self.evaluators.iter().any(|e| {
            e.spec.sample > 0.0
                && (e.spec.workflows.is_empty() || e.spec.workflows.iter().any(|p| crate::glob::matches(p, workflow)))
        })
    }

    /// Score `capture` with evaluator `index`. `vars` are the run inputs.
    pub async fn evaluate(
        &self,
        index: usize,
        run: &RunInfo<'_>,
        vars: Map<String, Json>,
        capture: &RunCapture,
    ) -> Option<AssertionResult> {
        let evaluator = self.evaluators.get(index)?;
        let mut case = TestCase::new(run.run_id, vars);
        case.tags = run.tags.to_vec();
        case.metadata = run.metadata.iter().map(|(k, v)| (k.clone(), Json::String(v.clone()))).collect();
        let ctx = AssertionContext::new(&case, capture);
        let result = match &evaluator.prepared {
            Some(prepared) => prepared.run(ctx).await,
            None => match self.prepare_for(&evaluator.spec.assertion, &case) {
                Ok(prepared) => prepared.run(ctx).await,
                Err(e) => AssertionResult::new(&evaluator.spec.assertion.kind, false, 0.0, 1.0, e.to_string()),
            },
        };
        Some(result)
    }

    fn prepare_for(&self, spec: &AssertionSpec, case: &TestCase) -> Result<PreparedAssertion, EvalError> {
        let mut context = case.vars.clone();
        context.insert("vars".into(), Json::Object(case.vars.clone()));
        context.insert("metadata".into(), Json::Object(case.metadata.clone()));
        context.insert("id".into(), Json::String(case.id.clone()));
        context.insert("tags".into(), Json::from(case.tags.clone()));
        let context = Json::Object(context);
        let mut rendered = spec.clone();
        rendered.value = spec.value.as_ref().map(|v| render(v, &context)).transpose()?;
        for value in rendered.config.values_mut() {
            *value = render(value, &context)?;
        }
        self.registry.prepare(&rendered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assertion::test_support::answered;

    const CONFIG: &str = r#"
concurrency: 3
queue: 10
evaluators:
  - name: polite
    workflows: [chat, "support-*"]
    sample: 0.5
    tags: [prod]
    metadata: { tier: gold }
    assert: { type: not-icontains, value: "as an ai" }
  - name: fast
    status: [succeeded]
    assert: { type: latency, threshold: 100 }
"#;

    fn meta(pairs: &[(&str, &str)]) -> IndexMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    #[test]
    fn parses_filters_and_defaults() {
        let config = OnlineConfig::parse(CONFIG).unwrap();
        assert_eq!((config.concurrency, config.queue), (3, 10));
        let polite = &config.evaluators[0];
        let gold = meta(&[("tier", "gold")]);
        let tags = vec!["prod".to_owned()];
        assert!(polite.matches(&RunInfo::new("r", "support-eu", RunStatus::Failed, &tags, &gold)));
        assert!(!polite.matches(&RunInfo::new("r", "other", RunStatus::Succeeded, &tags, &gold)));
        assert!(!polite.matches(&RunInfo::new("r", "chat", RunStatus::Succeeded, &[], &gold)));
        assert!(!polite.matches(&RunInfo::new("r", "chat", RunStatus::Succeeded, &tags, &meta(&[("tier", "free")]))));
        assert!(!polite.matches(&RunInfo::new("r", "chat", RunStatus::Aborted, &tags, &gold)), "default statuses");
        let fast = &config.evaluators[1];
        assert_eq!(fast.sample, 1.0);
        assert!(fast.matches(&RunInfo::new("r", "anything", RunStatus::Succeeded, &[], &IndexMap::new())));
        assert!(!fast.matches(&RunInfo::new("r", "anything", RunStatus::Failed, &[], &IndexMap::new())));
        assert_eq!(config.unmatched_workflows(&["chat"]), vec!["support-*"]);
    }

    #[test]
    fn sampling_is_deterministic_and_close_to_the_rate() {
        let config = OnlineConfig::parse(CONFIG).unwrap();
        let polite = &config.evaluators[0];
        let ids: Vec<String> = (0..10_000).map(|i| format!("run-18a2b{i:x}-{i:x}")).collect();
        let first: Vec<bool> = ids.iter().map(|id| polite.sampled(id)).collect();
        let again: Vec<bool> = ids.iter().map(|id| polite.sampled(id)).collect();
        assert_eq!(first, again, "same run id, same decision");
        let rate = first.iter().filter(|s| **s).count() as f64 / ids.len() as f64;
        assert!((rate - 0.5).abs() < 0.03, "rate {rate}");
        assert_eq!(sample_point("run-1", "polite"), sample_point("run-1", "polite"));
        assert_ne!(sample_point("run-1", "polite"), sample_point("run-1", "fast"), "independent per evaluator");
        assert!(ids.iter().all(|id| (0.0..1.0).contains(&sample_point(id, "x"))));
        let mut never = polite.clone();
        never.sample = 0.0;
        assert!(!ids.iter().any(|id| never.sampled(id)));
        let mut always = polite.clone();
        always.sample = 1.0;
        assert!(ids.iter().all(|id| always.sampled(id)));
    }

    #[test]
    fn rejects_offline_only_assertions_and_bad_settings() {
        let bad = |text: &str| OnlineConfig::parse(text).unwrap_err().to_string();
        let one = |assert: &str| format!("evaluators: [{{name: e, assert: {assert}}}]");
        assert!(bad(&one("{type: equals, value: x}")).contains("cannot run online"));
        assert!(bad(&one("{type: retrieval:mrr, value: [d]}")).contains("cannot run online"));
        assert!(bad(&one("{type: verdict, value: pass}")).contains("cannot run online"));
        assert!(bad(&one("{type: contains, value: \"{{ expected }}\"}")).contains("expected"));
        assert!(
            bad(&one("{type: judge, workflow: j.yaml, inputs: {ref: \"{{expected.answer}}\"}}")).contains("expected")
        );
        assert!(OnlineConfig::parse(&one("{type: not-regex, value: \"^sorry\"}")).is_ok());
        assert!(bad("evaluators: [{name: e, sample: 2, assert: {type: latency, threshold: 1}}]").contains("sample"));
        assert!(
            bad("evaluators: [{name: e, status: [paused], assert: {type: latency, threshold: 1}}]").contains("paused")
        );
        assert!(bad("evaluators: []").contains("empty"));
        assert!(bad("queue: 0\nevaluators: [{name: e, assert: {type: latency, threshold: 1}}]").contains("queue"));
        assert!(bad("evaluators: [{name: e, assert: {type: latency, threshold: 1}}, {name: e, assert: {type: latency, threshold: 2}}]").contains("duplicate"));
        assert!(bad("evaluators: [{name: e, sampel: 1, assert: {type: latency, threshold: 1}}]").contains("sampel"));
    }

    #[tokio::test]
    async fn evaluates_a_capture_with_run_references() {
        let config = OnlineConfig::parse(
            r#"
evaluators:
  - name: mentions-user
    assert: { type: icontains, value: "{{name}}" }
  - name: budget
    assert: { type: latency, threshold: 100 }
"#,
        )
        .unwrap();
        let evaluators = OnlineEvaluators::new(&config, AssertionRegistry::with_builtins()).unwrap();
        let metadata = IndexMap::new();
        let run = RunInfo::new("run-1", "chat", RunStatus::Succeeded, &[], &metadata);
        assert_eq!(evaluators.select(&run), vec![0, 1]);
        assert!(evaluators.covers("chat"));
        let mut capture = answered("Hello, Ada.");
        capture.latency_ms = 500;
        let vars: Map<String, Json> = serde_json::from_str(r#"{"name": "ada"}"#).unwrap();
        let mentions = evaluators.evaluate(0, &run, vars.clone(), &capture).await.unwrap();
        assert!(mentions.pass, "{mentions:?}");
        let budget = evaluators.evaluate(1, &run, vars, &capture).await.unwrap();
        assert!(!budget.pass && budget.score < 1.0, "{budget:?}");
        let missing = evaluators.evaluate(0, &run, Map::new(), &capture).await.unwrap();
        assert!(!missing.pass && missing.reason.contains("name"), "{missing:?}");
        assert!(evaluators.evaluate(9, &run, Map::new(), &capture).await.is_none());
    }
}
