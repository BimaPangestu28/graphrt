//! Comparing a report with a baseline (an earlier `--output` file).
//!
//! LLM metrics are noisy, so the comparison allows a tolerance instead of
//! demanding equality: a metric regresses when its mean score drops by more
//! than its tolerance, a test when its pass rate over its trials drops by more
//! than `test_tolerance`. Tests and metrics only on one side are reported as
//! `new` / `missing`, never as regressions.

use std::collections::HashMap;
use std::fmt::Write as _;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::config::Tolerance;
use crate::report::{EvalReport, TestResult, TestStatus};

/// How one metric or test moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Regressed,
    Improved,
    Unchanged,
    /// Not in the baseline.
    New,
    /// In the baseline only (e.g. filtered out of this run).
    Missing,
}

/// One compared value: a metric's mean score or a test's pass rate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Delta {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<f64>,
    pub tolerance: f64,
    pub change: Change,
}

/// The comparison with a baseline report.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BaselineComparison {
    /// The baseline file.
    pub path: String,
    /// The baseline's workflow fingerprint (differs when the workflow changed).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workflow_fingerprint: String,
    /// Judge workflows whose fingerprint differs from the baseline's: their
    /// scores are not strictly comparable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub judges_changed: Vec<String>,
    pub regressions: usize,
    pub improvements: usize,
    /// Every metric of either report.
    pub metrics: Vec<Delta>,
    /// Tests that changed, are new or are missing (unchanged ones are left out).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<Delta>,
}

impl BaselineComparison {
    /// Compare `current` with `baseline` (loaded from `path`).
    pub fn compare(current: &EvalReport, baseline: &EvalReport, path: &str, tolerance: &Tolerance) -> Self {
        let mut comparison = Self {
            path: path.to_owned(),
            workflow_fingerprint: baseline.workflow.fingerprint.clone(),
            judges_changed: judges_changed(&current.tests, &baseline.tests),
            ..Self::default()
        };
        let metric_means = |report: &EvalReport| -> IndexMap<String, f64> {
            report.summary.metrics.iter().filter(|(_, m)| m.count > 0).map(|(k, m)| (k.clone(), m.mean_score)).collect()
        };
        comparison.metrics = diff(&metric_means(current), &metric_means(baseline), |name| tolerance.of(name));
        comparison.tests =
            diff(&pass_rates(&current.tests), &pass_rates(&baseline.tests), |_| tolerance.test_tolerance);
        comparison.tests.retain(|d| d.change != Change::Unchanged);
        let count = |change| comparison.metrics.iter().chain(&comparison.tests).filter(|d| d.change == change).count();
        comparison.regressions = count(Change::Regressed);
        comparison.improvements = count(Change::Improved);
        comparison
    }
}

impl EvalReport {
    /// Compare with `baseline` (read from `path`), record the comparison and
    /// fail the gate on any regression beyond `tolerance`.
    pub fn apply_baseline(&mut self, baseline: &EvalReport, path: &str, tolerance: &Tolerance) {
        let comparison = BaselineComparison::compare(self, baseline, path, tolerance);
        let regressions = comparison.regressions;
        self.summary.gate.require(regressions == 0, || format!("{regressions} regression(s) vs the baseline"));
        self.baseline = Some(comparison);
    }
}

/// Each test's pass rate over its trials, in suite order.
fn pass_rates(tests: &[TestResult]) -> IndexMap<String, f64> {
    let mut counts: IndexMap<String, (u32, u32)> = IndexMap::new();
    for test in tests {
        let entry = counts.entry(test.id.clone()).or_default();
        entry.0 += u32::from(test.status == TestStatus::Passed);
        entry.1 += 1;
    }
    counts.into_iter().map(|(id, (passed, n))| (id, f64::from(passed) / f64::from(n.max(1)))).collect()
}

fn diff(
    current: &IndexMap<String, f64>,
    baseline: &IndexMap<String, f64>,
    tolerance: impl Fn(&str) -> f64,
) -> Vec<Delta> {
    let mut deltas: Vec<Delta> = current
        .iter()
        .map(|(name, &now)| {
            let tolerance = tolerance(name);
            let before = baseline.get(name).copied();
            let change = match before {
                None => Change::New,
                Some(before) if now < before - tolerance - 1e-9 => Change::Regressed,
                Some(before) if now > before + tolerance + 1e-9 => Change::Improved,
                Some(_) => Change::Unchanged,
            };
            Delta { name: name.clone(), baseline: before, current: Some(now), tolerance, change }
        })
        .collect();
    deltas.extend(baseline.iter().filter(|(name, _)| !current.contains_key(*name)).map(|(name, &before)| Delta {
        name: name.clone(),
        baseline: Some(before),
        current: None,
        tolerance: tolerance(name),
        change: Change::Missing,
    }));
    deltas
}

/// Judge workflows (by path) whose fingerprint is not one the baseline used.
fn judges_changed(current: &[TestResult], baseline: &[TestResult]) -> Vec<String> {
    let fingerprints = |tests: &[TestResult]| -> HashMap<String, String> {
        tests
            .iter()
            .flat_map(|t| &t.assertions)
            .filter_map(|a| a.judge.as_deref())
            .map(|j| (j.workflow.clone(), j.fingerprint.clone()))
            .collect()
    };
    let before = fingerprints(baseline);
    let mut changed: Vec<String> = fingerprints(current)
        .into_iter()
        .filter(|(path, fingerprint)| before.get(path).is_some_and(|old| old != fingerprint))
        .map(|(path, _)| path)
        .collect();
    changed.sort();
    changed
}

/// The terminal block for a baseline comparison.
pub fn format_baseline(b: &BaselineComparison) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "\nbaseline {}: {} regression(s), {} improvement(s)", b.path, b.regressions, b.improvements);
    for judge in &b.judges_changed {
        let _ = writeln!(out, "  note: judge {judge} changed since the baseline");
    }
    let value = |v: Option<f64>| v.map_or_else(|| "-".to_owned(), |v| format!("{v:.3}"));
    for (kind, deltas) in [("metric", &b.metrics), ("test", &b.tests)] {
        for d in deltas.iter().filter(|d| matches!(d.change, Change::Regressed | Change::Improved)) {
            let label = if d.change == Change::Regressed { "REGRESSED" } else { "improved " };
            let _ = writeln!(
                out,
                "  {label} {kind} {:<28} {} -> {} (tolerance {:.2})",
                d.name,
                value(d.baseline),
                value(d.current),
                d.tolerance
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;
    use crate::assertion::AssertionResult;
    use crate::judge::JudgeRecord;
    use crate::report::{REPORT_SCHEMA_VERSION, RunSummary, Summary, WorkflowInfo};
    use serde_json::Map;

    fn test(id: &str, trial: u32, pass: bool, faithfulness: f64, judge_fingerprint: &str) -> TestResult {
        let mut judged = AssertionResult::new("judge", faithfulness >= 0.5, faithfulness, 1.0, "");
        judged.metric = Some("faithfulness".into());
        judged.judge = Some(Box::new(JudgeRecord {
            workflow: "judges/faithfulness.yaml".into(),
            fingerprint: judge_fingerprint.into(),
            ..JudgeRecord::default()
        }));
        TestResult {
            id: id.into(),
            description: None,
            source: "inline".into(),
            tags: vec![],
            trial,
            status: if pass { TestStatus::Passed } else { TestStatus::Failed },
            pass,
            score: f64::from(u8::from(pass)),
            reason: None,
            vars: Map::new(),
            expected: None,
            run: RunSummary::default(),
            assertions: vec![judged],
        }
    }

    fn report(tests: Vec<TestResult>) -> EvalReport {
        EvalReport {
            schema_version: REPORT_SCHEMA_VERSION,
            graphrt_version: "0".into(),
            suite: "s".into(),
            description: None,
            workflow: WorkflowInfo { path: "w".into(), name: String::new(), fingerprint: "f".into() },
            started_at_ms: 0,
            summary: Summary::of(&tests, 0.0, 1),
            cases: vec![],
            calibration: None,
            baseline: None,
            tests,
        }
    }

    #[test]
    fn detects_regressions_beyond_tolerance() {
        let baseline = report(vec![
            test("a", 0, true, 1.0, "j1"),
            test("a", 1, true, 1.0, "j1"),
            test("b", 0, true, 0.9, "j1"),
            test("gone", 0, true, 0.8, "j1"),
        ]);
        // Faithfulness mean: baseline 0.925, now 0.7; `a` drops from 2/2 to 1/2.
        let current = report(vec![
            test("a", 0, true, 1.0, "j2"),
            test("a", 1, false, 0.2, "j2"),
            test("b", 0, true, 0.9, "j2"),
            test("new", 0, true, 0.7, "j2"),
        ]);
        let tolerance = Tolerance::default();
        let c = BaselineComparison::compare(&current, &baseline, "prev.json", &tolerance);
        assert_eq!(c.judges_changed, ["judges/faithfulness.yaml"]);
        let faithfulness = c.metrics.iter().find(|d| d.name == "faithfulness").unwrap();
        assert_eq!(faithfulness.change, Change::Regressed, "{faithfulness:?}");
        let changes: Vec<(&str, Change)> = c.tests.iter().map(|d| (d.name.as_str(), d.change)).collect();
        assert_eq!(changes, [("a", Change::Regressed), ("new", Change::New), ("gone", Change::Missing)]);
        assert_eq!((c.regressions, c.improvements), (2, 0));
        let text = format_baseline(&c);
        assert!(text.contains("REGRESSED metric faithfulness") && text.contains("REGRESSED test a"), "{text}");
        assert!(text.contains("judge judges/faithfulness.yaml changed"), "{text}");

        // Generous tolerances absorb the drops; the reverse direction improves.
        let mut loose = Tolerance { test_tolerance: 0.5, ..Tolerance::default() };
        loose.metrics.insert("faithfulness".into(), 0.3);
        let c = BaselineComparison::compare(&current, &baseline, "prev.json", &loose);
        assert_eq!(c.regressions, 0, "{c:?}");
        let c = BaselineComparison::compare(&baseline, &current, "prev.json", &tolerance);
        assert_eq!((c.regressions, c.improvements), (0, 2));
    }

    #[test]
    fn identical_reports_do_not_change() {
        let r = report(vec![test("a", 0, true, 0.9, "j")]);
        let c = BaselineComparison::compare(&r, &r, "same.json", &Tolerance::default());
        assert_eq!((c.regressions, c.improvements), (0, 0));
        assert!(c.tests.is_empty() && c.judges_changed.is_empty());
        assert!(c.metrics.iter().all(|d| d.change == Change::Unchanged));
    }
}
