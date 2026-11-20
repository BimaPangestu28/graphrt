//! The eval file format (`*.eval.yaml`) and JSONL datasets.
//!
//! The shape follows Promptfoo (`tests: [{vars, assert: [{type, value,
//! threshold, weight, metric}]}]`, `defaultTest`) so test cases move between
//! the two tools. Keys starting with `x-` are ignored at the file and test level.
//!
//! ```yaml
//! workflow: ../hello.yaml
//! providers: providers.replay.yaml   # optional
//! dataset: cases.jsonl               # optional: {id, vars, expected?, tags?, metadata?} per line
//! defaultTest:
//!   assert: [{ type: latency, threshold: 2000 }]
//! tests:
//!   - id: ada
//!     vars: { name: Ada }
//!     assert:
//!       - { type: equals, value: "Hello, Ada." }
//! gate: { pass_rate: 1.0 }
//! ```
//!
//! With `calibration:` the workflow under test is a judge and each test's
//! `expected` is a human label (`pass` / `fail`): the report adds agreement,
//! TPR, TNR and Cohen's kappa.

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{Map, Value as Json};

use crate::error::EvalError;

/// A parsed eval file, with relative paths resolved against its directory.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct EvalSuite {
    /// Where the suite was loaded from (empty for in-memory suites).
    pub path: PathBuf,
    pub description: Option<String>,
    /// The workflow under test.
    pub workflow: PathBuf,
    /// Model providers file layered over the environment providers.
    pub providers: Option<PathBuf>,
    /// Knowledge base config for knowledge-retrieval nodes.
    pub knowledge: Option<PathBuf>,
    /// MCP servers file.
    pub mcp: Option<PathBuf>,
    /// Plugin directories.
    pub plugins: Vec<PathBuf>,
    /// Every test case: inline tests first, then dataset rows, with `defaultTest` merged in.
    pub cases: Vec<TestCase>,
    pub gate: Gate,
    /// Judge calibration mode (the workflow is a judge, `expected` are labels).
    pub calibration: Option<Calibration>,
}

/// The CI gate.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Gate {
    /// Minimum fraction of passing test runs (0..=1, every trial counts).
    /// Default 1.0: every test must pass.
    #[serde(default = "one")]
    pub pass_rate: f64,
    /// Minimum mean pass^k over the test cases (with `--trials`).
    #[serde(default)]
    pub pass_hat_k: Option<f64>,
    /// `k` of pass@k and pass^k (default: the number of trials).
    #[serde(default)]
    pub k: Option<u32>,
    /// Tolerances when comparing against `--baseline`.
    #[serde(default)]
    pub baseline: Tolerance,
}

impl Default for Gate {
    fn default() -> Self {
        Self { pass_rate: 1.0, pass_hat_k: None, k: None, baseline: Tolerance::default() }
    }
}

/// How far results may fall below the baseline before it is a regression.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Tolerance {
    /// Largest allowed drop of a metric's mean score (default 0.05: LLM metrics are noisy).
    #[serde(default = "five_percent")]
    pub tolerance: f64,
    /// Largest allowed drop of a test's pass rate across its trials (default 0:
    /// a test that passed and now fails is a regression).
    #[serde(default)]
    pub test_tolerance: f64,
    /// Per-metric tolerances, overriding `tolerance`.
    #[serde(default)]
    pub metrics: IndexMap<String, f64>,
}

impl Default for Tolerance {
    fn default() -> Self {
        Self { tolerance: 0.05, test_tolerance: 0.0, metrics: IndexMap::new() }
    }
}

impl Tolerance {
    /// The tolerance of `metric`.
    pub fn of(&self, metric: &str) -> f64 {
        self.metrics.get(metric).copied().unwrap_or(self.tolerance)
    }
}

/// Judge calibration settings (`calibration:` in the eval file).
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Calibration {
    /// `unknown` verdicts: `exclude` (default: counted apart, outside the
    /// confusion matrix) or `fail` (counted as the judge saying fail).
    #[serde(default = "exclude")]
    pub unknown: crate::judge::UnknownPolicy,
    /// Score at which a judge that returns only a `score` says pass (default 0.5).
    #[serde(default)]
    pub threshold: Option<f64>,
    /// Gate: minimum Cohen's kappa.
    #[serde(default)]
    pub min_kappa: Option<f64>,
    /// Gate: minimum true positive rate.
    #[serde(default)]
    pub min_tpr: Option<f64>,
    /// Gate: minimum true negative rate.
    #[serde(default)]
    pub min_tnr: Option<f64>,
}

impl Default for Calibration {
    fn default() -> Self {
        Self { unknown: exclude(), threshold: None, min_kappa: None, min_tpr: None, min_tnr: None }
    }
}

fn one() -> f64 {
    1.0
}

fn five_percent() -> f64 {
    0.05
}

fn exclude() -> crate::judge::UnknownPolicy {
    crate::judge::UnknownPolicy::Exclude
}

/// One assertion as written in the file.
///
/// `config` holds every key other than the common ones (`type`, `value`,
/// `threshold`, `weight`, `metric`, `transform`), e.g. `k` for retrieval
/// metrics. Assertion factories validate it.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct AssertionSpec {
    /// The assertion type, including any `not-` prefix.
    pub kind: String,
    pub value: Option<Json>,
    pub threshold: Option<f64>,
    /// Contribution to the test score; `0` reports the result without gating the test.
    pub weight: f64,
    /// Name to aggregate the score under in the summary.
    pub metric: Option<String>,
    /// Path selecting the value under test (`output.field`, `outputs.text`...).
    pub transform: Option<String>,
    pub config: Map<String, Json>,
}

impl AssertionSpec {
    /// An assertion of `kind` with `value`.
    pub fn new(kind: impl Into<String>, value: Option<Json>) -> Self {
        Self {
            kind: kind.into(),
            value,
            threshold: None,
            weight: 1.0,
            metric: None,
            transform: None,
            config: Map::new(),
        }
    }

    /// Parse one `assert` entry.
    pub fn from_json(raw: Json) -> Result<Self, EvalError> {
        let Json::Object(mut map) = raw else {
            return Err(EvalError::invalid("an assertion must be a mapping with a `type`"));
        };
        let kind = match map.remove("type") {
            Some(Json::String(kind)) if !kind.trim().is_empty() => kind,
            _ => return Err(EvalError::invalid("an assertion needs a `type` string")),
        };
        let number = |map: &mut Map<String, Json>, key: &str| -> Result<Option<f64>, EvalError> {
            match map.remove(key) {
                None | Some(Json::Null) => Ok(None),
                Some(v) => v
                    .as_f64()
                    .filter(|n| n.is_finite())
                    .map(Some)
                    .ok_or_else(|| EvalError::invalid(format!("assertion `{kind}`: `{key}` must be a number"))),
            }
        };
        let threshold = number(&mut map, "threshold")?;
        let weight = number(&mut map, "weight")?.unwrap_or(1.0);
        if weight < 0.0 {
            return Err(EvalError::invalid(format!("assertion `{kind}`: `weight` must not be negative")));
        }
        let text = |map: &mut Map<String, Json>, key: &str| -> Result<Option<String>, EvalError> {
            match map.remove(key) {
                None | Some(Json::Null) => Ok(None),
                Some(Json::String(s)) => Ok(Some(s)),
                Some(_) => Err(EvalError::invalid(format!("assertion `{kind}`: `{key}` must be a string"))),
            }
        };
        let metric = text(&mut map, "metric")?;
        let transform = text(&mut map, "transform")?;
        let value = map.remove("value");
        Ok(Self { kind, value, threshold, weight, metric, transform, config: map })
    }
}

/// A test case ready to run.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct TestCase {
    /// Unique within the suite.
    pub id: String,
    pub description: Option<String>,
    /// Start inputs of the workflow.
    pub vars: Map<String, Json>,
    /// Expected answer (dataset rows); available to assertion values as `{{expected}}`.
    pub expected: Option<Json>,
    pub tags: Vec<String>,
    pub metadata: Map<String, Json>,
    pub assertions: Vec<AssertionSpec>,
    /// Pass when the weighted score reaches this, instead of requiring every gating assertion.
    pub threshold: Option<f64>,
    /// `inline` or `<dataset file>:<line>`.
    pub source: String,
}

impl TestCase {
    /// A test case with `id` and start inputs.
    pub fn new(id: impl Into<String>, vars: Map<String, Json>) -> Self {
        Self { id: id.into(), vars, source: "inline".into(), ..Self::default() }
    }

    /// Whether `filter` matches the id or one of the tags (`*` wildcards allowed).
    pub fn matches(&self, filter: &str) -> bool {
        crate::glob::matches(filter, &self.id) || self.tags.iter().any(|t| crate::glob::matches(filter, t))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    description: Option<String>,
    workflow: PathBuf,
    #[serde(default)]
    providers: Option<PathBuf>,
    #[serde(default)]
    knowledge: Option<PathBuf>,
    #[serde(default)]
    mcp: Option<PathBuf>,
    #[serde(default)]
    plugins: Vec<PathBuf>,
    #[serde(default)]
    dataset: Option<PathBuf>,
    #[serde(default, rename = "defaultTest")]
    default_test: Option<RawTest>,
    #[serde(default)]
    tests: Vec<RawTest>,
    #[serde(default)]
    gate: Gate,
    #[serde(default)]
    calibration: Option<Calibration>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTest {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    vars: Map<String, Json>,
    #[serde(default)]
    expected: Option<Json>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    metadata: Map<String, Json>,
    #[serde(default)]
    assert: Vec<Json>,
    #[serde(default)]
    threshold: Option<f64>,
}

impl EvalSuite {
    /// Load an eval file (YAML or JSON) and its dataset.
    pub fn load(path: &Path) -> Result<Self, EvalError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| EvalError::Io(format!("cannot read eval file `{}`: {e}", path.display())))?;
        let base = path.parent().unwrap_or_else(|| Path::new(""));
        Self::parse(&text, base)
            .map(|mut suite| {
                suite.path = path.to_path_buf();
                suite
            })
            .map_err(|e| match e {
                EvalError::Invalid(m) => EvalError::Invalid(format!("{}: {m}", path.display())),
                other => other,
            })
    }

    /// Parse eval file text; relative paths are resolved against `base`.
    pub fn parse(text: &str, base: &Path) -> Result<Self, EvalError> {
        let mut value = parse_structured(text)?;
        strip_extensions(&mut value);
        if let Some(Json::Array(tests)) = value.get_mut("tests") {
            tests.iter_mut().for_each(strip_extensions);
        }
        let raw: RawFile = serde_json::from_value(value).map_err(|e| EvalError::invalid(e.to_string()))?;
        validate_gate(&raw.gate, raw.calibration.as_ref())?;
        let resolve = |p: PathBuf| if p.is_relative() { base.join(p) } else { p };
        let default_test = raw.default_test.unwrap_or_default();
        if default_test.id.is_some() || default_test.expected.is_some() {
            return Err(EvalError::invalid("`defaultTest` cannot set `id` or `expected`"));
        }
        let mut cases = Vec::new();
        let implicit = if raw.calibration.is_some() { "verdict" } else { "equals" };
        for (index, test) in raw.tests.into_iter().enumerate() {
            let id = test.id.clone().unwrap_or_else(|| format!("test-{}", index + 1));
            cases.push(build_case(test, &default_test, id, "inline".into(), implicit)?);
        }
        if let Some(dataset) = raw.dataset {
            let path = resolve(dataset);
            for row in read_dataset(&path)? {
                cases.push(build_case(row.test, &default_test, row.id, row.source, implicit)?);
            }
        }
        let mut seen = std::collections::HashSet::new();
        for case in &cases {
            if !seen.insert(case.id.as_str()) {
                return Err(EvalError::invalid(format!("duplicate test id `{}`", case.id)));
            }
        }
        Ok(Self {
            path: PathBuf::new(),
            description: raw.description,
            workflow: resolve(raw.workflow),
            providers: raw.providers.map(resolve),
            knowledge: raw.knowledge.map(resolve),
            mcp: raw.mcp.map(resolve),
            plugins: raw.plugins.into_iter().map(resolve).collect(),
            cases,
            gate: raw.gate,
            calibration: raw.calibration,
        })
    }
}

fn validate_gate(gate: &Gate, calibration: Option<&Calibration>) -> Result<(), EvalError> {
    let fraction = |name: &str, value: Option<f64>| match value {
        Some(v) if !(0.0..=1.0).contains(&v) => Err(EvalError::invalid(format!("`{name}` must be between 0 and 1"))),
        _ => Ok(()),
    };
    fraction("gate.pass_rate", Some(gate.pass_rate))?;
    fraction("gate.pass_hat_k", gate.pass_hat_k)?;
    fraction("gate.baseline.tolerance", Some(gate.baseline.tolerance))?;
    fraction("gate.baseline.test_tolerance", Some(gate.baseline.test_tolerance))?;
    for (metric, tolerance) in &gate.baseline.metrics {
        fraction(&format!("gate.baseline.metrics.{metric}"), Some(*tolerance))?;
    }
    if gate.k == Some(0) {
        return Err(EvalError::invalid("`gate.k` must be at least 1"));
    }
    if let Some(c) = calibration {
        fraction("calibration.threshold", c.threshold)?;
        fraction("calibration.min_tpr", c.min_tpr)?;
        fraction("calibration.min_tnr", c.min_tnr)?;
        if c.min_kappa.is_some_and(|k| !(-1.0..=1.0).contains(&k)) {
            return Err(EvalError::invalid("`calibration.min_kappa` must be between -1 and 1"));
        }
    }
    Ok(())
}

fn build_case(
    test: RawTest,
    defaults: &RawTest,
    id: String,
    source: String,
    implicit: &str,
) -> Result<TestCase, EvalError> {
    let context = |e: EvalError| EvalError::invalid(format!("test `{id}`: {e}"));
    let mut vars = defaults.vars.clone();
    vars.extend(test.vars);
    let mut metadata = defaults.metadata.clone();
    metadata.extend(test.metadata);
    let mut tags = defaults.tags.clone();
    tags.extend(test.tags.into_iter().filter(|t| !defaults.tags.contains(t)));
    let own_assertions = !test.assert.is_empty();
    let mut assertions = Vec::with_capacity(defaults.assert.len() + test.assert.len() + 1);
    for raw in defaults.assert.iter().cloned().chain(test.assert) {
        assertions.push(AssertionSpec::from_json(raw).map_err(context)?);
    }
    // Promptfoo's `__expected` convention: an expected value on a test without its
    // own assertions means `equals` (after the `defaultTest` assertions). In
    // calibration suites the expected value is a label the judge's verdict must match.
    if !own_assertions && let Some(expected) = &test.expected {
        assertions.push(AssertionSpec::new(implicit, Some(expected.clone())));
    }
    if assertions.is_empty() {
        return Err(EvalError::invalid(format!("test `{id}` has no assertions (and no `expected` value)")));
    }
    let threshold = test.threshold.or(defaults.threshold);
    if threshold.is_some_and(|t| !(0.0..=1.0).contains(&t)) {
        return Err(EvalError::invalid(format!("test `{id}`: `threshold` must be between 0 and 1")));
    }
    Ok(TestCase {
        description: test.description.or_else(|| defaults.description.clone()),
        vars,
        expected: test.expected,
        tags,
        metadata,
        assertions,
        threshold,
        source,
        id,
    })
}

struct DatasetRow {
    id: String,
    source: String,
    test: RawTest,
}

/// Read a JSONL dataset: one `{id?, vars, expected?, tags?, metadata?, assert?}`
/// object per line; blank lines and lines starting with `//` are skipped.
fn read_dataset(path: &Path) -> Result<Vec<DatasetRow>, EvalError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| EvalError::Io(format!("cannot read dataset `{}`: {e}", path.display())))?;
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "dataset".into());
    let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut rows = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line_no = index + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }
        let mut value: Json = serde_json::from_str(trimmed)
            .map_err(|e| EvalError::invalid(format!("{}:{line_no}: invalid JSON: {e}", path.display())))?;
        strip_extensions(&mut value);
        let test: RawTest = serde_json::from_value(value)
            .map_err(|e| EvalError::invalid(format!("{}:{line_no}: {e}", path.display())))?;
        let id = test.id.clone().unwrap_or_else(|| format!("{stem}:{line_no}"));
        rows.push(DatasetRow { id, source: format!("{name}:{line_no}"), test });
    }
    Ok(rows)
}

/// Drop `x-` keys of a mapping (tool-specific extensions).
fn strip_extensions(value: &mut Json) {
    if let Json::Object(map) = value {
        map.retain(|k, _| !k.starts_with("x-"));
    }
}

/// YAML or JSON text to JSON (YAML 1.2 booleans).
pub fn parse_structured(text: &str) -> Result<Json, EvalError> {
    if text.trim_start().starts_with(['{', '[']) {
        return serde_json::from_str(text).map_err(|e| EvalError::invalid(format!("invalid JSON: {e}")));
    }
    let options = serde_saphyr::options! { strict_booleans: true };
    serde_saphyr::from_str_with_options(text, options).map_err(|e| EvalError::invalid(format!("invalid YAML: {e}")))
}

/// Start inputs for the engine.
pub fn inputs_of(vars: &Map<String, Json>) -> IndexMap<String, graphrt_core::Value> {
    vars.iter().map(|(k, v)| (k.clone(), graphrt_core::Value::from_json(v.clone()))).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn parses_tests_defaults_and_resolves_paths() {
        let suite = EvalSuite::parse(
            r#"
workflow: ../wf.yaml
providers: p.yaml
x-owner: me
defaultTest:
  vars: { lang: en }
  tags: [smoke]
  assert: [{ type: latency, threshold: 100, weight: 0 }]
tests:
  - vars: { name: Ada }
    assert: [{ type: equals, value: "Hi", metric: exact }]
  - id: named
    tags: [slow]
    x-note: ignored
    vars: { name: Bob, lang: id }
    threshold: 0.5
    assert: [{ type: retrieval:recall-at-k, value: [d1], k: 3 }]
gate: { pass_rate: 0.9 }
"#,
            Path::new("/evals"),
        )
        .unwrap();
        assert_eq!(suite.workflow, PathBuf::from("/evals/../wf.yaml"));
        assert_eq!(suite.providers, Some(PathBuf::from("/evals/p.yaml")));
        assert_eq!(suite.gate.pass_rate, 0.9);
        assert_eq!(suite.cases.len(), 2);
        let first = &suite.cases[0];
        assert_eq!(first.id, "test-1");
        assert_eq!(first.vars["lang"], "en");
        assert_eq!(first.assertions.len(), 2, "defaultTest assertions come first");
        assert_eq!(first.assertions[0].weight, 0.0);
        assert_eq!(first.assertions[1].metric.as_deref(), Some("exact"));
        let second = &suite.cases[1];
        assert_eq!(second.vars["lang"], "id");
        assert_eq!(second.tags, ["smoke", "slow"]);
        assert_eq!(second.assertions[1].config["k"], 3);
        assert!(second.matches("slow") && second.matches("nam*") && !second.matches("other"));
    }

    #[test]
    fn rejects_bad_files() {
        let bad = |text: &str| EvalSuite::parse(text, Path::new("")).unwrap_err().to_string();
        assert!(bad("tests: []").contains("workflow"));
        assert!(bad("workflow: w.yaml\ntestz: []").contains("testz"));
        assert!(bad("workflow: w.yaml\ntests: [{vars: {}}]").contains("no assertions"));
        assert!(bad("workflow: w.yaml\ntests: [{assert: [{value: 1}]}]").contains("type"));
        assert!(bad("workflow: w.yaml\ntests: [{assert: [{type: equals, weight: -1}]}]").contains("weight"));
        assert!(bad("workflow: w.yaml\ngate: {pass_rate: 2}").contains("pass_rate"));
        assert!(
            bad("workflow: w.yaml\ntests: [{id: a, assert: [{type: equals}]}, {id: a, assert: [{type: equals}]}]")
                .contains("duplicate")
        );
    }

    #[test]
    fn reads_jsonl_datasets() {
        let dir = std::env::temp_dir().join(format!("graphrt-eval-dataset-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("cases.jsonl"),
            "{\"id\": \"one\", \"vars\": {\"q\": \"a\"}, \"expected\": \"A\", \"tags\": [\"t\"]}\n\n// comment\n{\"vars\": {\"q\": \"b\"}, \"expected\": \"B\", \"metadata\": {\"expected_docs\": [\"d\"]}}\n",
        )
        .unwrap();
        let suite = EvalSuite::parse("workflow: w.yaml\ndataset: cases.jsonl\n", &dir).unwrap();
        assert_eq!(suite.cases.len(), 2);
        assert_eq!(suite.cases[0].id, "one");
        assert_eq!(suite.cases[0].assertions[0].kind, "equals", "bare expected means equals");
        assert_eq!(suite.cases[1].id, "cases:4");
        assert_eq!(suite.cases[1].source, "cases.jsonl:4");
        std::fs::write(dir.join("bad.jsonl"), "{\"vars\": {}, \"nope\": 1}\n").unwrap();
        let err = EvalSuite::parse("workflow: w.yaml\ndataset: bad.jsonl\n", &dir).unwrap_err().to_string();
        assert!(err.contains("bad.jsonl:1") && err.contains("nope"), "{err}");
        let _cleanup = std::fs::remove_dir_all(&dir);
    }
}
