//! Results: per-test results, the suite summary and gate, and the JSON,
//! JUnit XML and terminal renderings.
//!
//! The JSON report (`--output results.json`) is versioned by
//! [`REPORT_SCHEMA_VERSION`]; fields are only ever added within a version.
//! Version 2 renamed `repeat` to `trial` and added per-case trial statistics,
//! calibration and baseline sections, judge records and excluded results;
//! version 1 reports still load as baselines ([`crate::baseline::Baseline`]).

use std::fmt::Write as _;

use graphrt_core::Event;
use graphrt_core::event::Usage;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json};

use crate::assertion::AssertionResult;
use crate::baseline::BaselineComparison;
use crate::calibration::CalibrationSummary;
use crate::capture::{RetrievalHit, RunStatus, ToolCallRecord};
use crate::stats::{pass_at_k, pass_hat_k};

/// Version of the JSON report format.
pub const REPORT_SCHEMA_VERSION: u32 = 2;

/// Verdict of one test.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestStatus {
    /// Every gating assertion passed (or the weighted score reached the threshold).
    Passed,
    /// The run finished but assertions failed.
    Failed,
    /// The run did not succeed (failed, aborted, paused or timed out).
    Error,
}

/// What the run produced, as reported.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RunSummary {
    pub run_id: String,
    pub status: RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
    pub latency_ms: u64,
    pub usage: Usage,
    /// The value assertions check by default (see `RunCapture::output`).
    pub output: Json,
    pub outputs: Map<String, Json>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub answer: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retrieval: Vec<RetrievalHit>,
    /// Every event but `node_chunk` (only with `keep_events`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events: Option<Vec<Event>>,
}

/// Result of one test (one repetition of one test case).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TestResult {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `inline` or `<dataset>:<line>`.
    pub source: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Trial index (0-based; see `RunnerOptions::trials`).
    #[serde(alias = "repeat")]
    pub trial: u32,
    pub status: TestStatus,
    pub pass: bool,
    /// Weighted mean of the gating assertion scores (0..1).
    pub score: f64,
    /// Why the test failed, in one line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub vars: Map<String, Json>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<Json>,
    pub run: RunSummary,
    pub assertions: Vec<AssertionResult>,
}

impl TestResult {
    /// `id`, with `#n` appended for trials after the first.
    pub fn display_id(&self) -> String {
        if self.trial == 0 { self.id.clone() } else { format!("{}#{}", self.id, self.trial) }
    }
}

/// Aggregate of one metric (an assertion's `metric`, else its type).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MetricSummary {
    /// Results counted (excluded ones are not).
    pub count: usize,
    pub passed: usize,
    pub mean_score: f64,
    /// Results left out (a judge's `unknown` with `unknown: exclude`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub excluded: usize,
}

fn no_tokens(usage: &Usage) -> bool {
    usage.total_tokens == 0
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// Latency distribution over the tests, in milliseconds.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LatencySummary {
    pub mean: f64,
    pub p50: u64,
    pub p95: u64,
    pub max: u64,
}

/// The gate verdict.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GateResult {
    /// Required pass rate.
    pub pass_rate: f64,
    /// Required mean pass^k, when gated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass_hat_k: Option<f64>,
    pub passed: bool,
    /// Why the gate failed, one entry per unmet condition.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<String>,
}

impl GateResult {
    /// Fail the gate with `failure` unless `ok`.
    pub fn require(&mut self, ok: bool, failure: impl FnOnce() -> String) {
        if !ok {
            self.passed = false;
            self.failures.push(failure());
        }
    }
}

/// pass@k and pass^k over the test cases (`--trials`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TrialSummary {
    /// Most trials of any test case.
    pub trials: u32,
    pub k: u32,
    /// Mean over the cases of the unbiased pass@k.
    pub pass_at_k: f64,
    /// Mean over the cases of the unbiased pass^k.
    pub pass_hat_k: f64,
}

/// One test case over its trials.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CaseSummary {
    pub id: String,
    pub trials: u32,
    pub passed: u32,
    /// `passed / trials`.
    pub pass_rate: f64,
    /// `1 - C(n-c, k) / C(n, k)`: at least one of k trials passes.
    pub pass_at_k: f64,
    /// `C(c, k) / C(n, k)`: all k trials pass.
    pub pass_hat_k: f64,
    pub mean_score: f64,
}

/// Group results by test id (suite order) into per-case trial statistics.
pub fn case_summaries(tests: &[TestResult], k: u32) -> Vec<CaseSummary> {
    let mut cases: IndexMap<&str, CaseSummary> = IndexMap::new();
    for test in tests {
        let case =
            cases.entry(&test.id).or_insert_with(|| CaseSummary { id: test.id.clone(), ..CaseSummary::default() });
        case.trials += 1;
        case.passed += u32::from(test.status == TestStatus::Passed);
        case.mean_score += test.score;
    }
    cases
        .into_values()
        .map(|mut case| {
            let n = case.trials.max(1);
            case.pass_rate = f64::from(case.passed) / f64::from(n);
            case.mean_score /= f64::from(n);
            case.pass_at_k = pass_at_k(case.trials, case.passed, k);
            case.pass_hat_k = pass_hat_k(case.trials, case.passed, k);
            case
        })
        .collect()
}

/// Means of pass@k and pass^k over `cases`.
pub fn trial_summary(cases: &[CaseSummary], k: u32) -> TrialSummary {
    let n = cases.len().max(1) as f64;
    TrialSummary {
        trials: cases.iter().map(|c| c.trials).max().unwrap_or_default(),
        k,
        pass_at_k: cases.iter().map(|c| c.pass_at_k).sum::<f64>() / n,
        pass_hat_k: cases.iter().map(|c| c.pass_hat_k).sum::<f64>() / n,
    }
}

/// Suite-level numbers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Summary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub errors: usize,
    /// `passed / total` (1 when there are no tests).
    pub pass_rate: f64,
    /// Wall time of the whole evaluation.
    pub duration_ms: u64,
    pub usage: Usage,
    /// Tokens the judge workflows used (not part of `usage`).
    #[serde(default, skip_serializing_if = "no_tokens")]
    pub judge_usage: Usage,
    pub latency_ms: LatencySummary,
    pub metrics: IndexMap<String, MetricSummary>,
    /// pass@k / pass^k (with more than one trial or a `pass_hat_k` gate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trials: Option<TrialSummary>,
    pub gate: GateResult,
}

impl Summary {
    /// Summarize `tests` against a required pass rate.
    pub fn of(tests: &[TestResult], required_pass_rate: f64, duration_ms: u64) -> Self {
        let total = tests.len();
        let passed = tests.iter().filter(|t| t.status == TestStatus::Passed).count();
        let errors = tests.iter().filter(|t| t.status == TestStatus::Error).count();
        let pass_rate = if total == 0 { 1.0 } else { passed as f64 / total as f64 };
        let mut usage = Usage::default();
        for test in tests {
            usage.add(&test.run.usage);
        }
        let mut judge_usage = Usage::default();
        let mut metrics: IndexMap<String, MetricSummary> = IndexMap::new();
        for result in tests.iter().flat_map(|t| &t.assertions) {
            if let Some(judge) = &result.judge {
                judge_usage.add(&judge.usage);
            }
            let entry = metrics.entry(result.metric_name().to_owned()).or_default();
            if result.excluded {
                entry.excluded += 1;
                continue;
            }
            entry.count += 1;
            entry.passed += usize::from(result.pass);
            entry.mean_score += result.score;
        }
        for metric in metrics.values_mut() {
            metric.mean_score /= metric.count.max(1) as f64;
        }
        Self {
            total,
            passed,
            failed: total - passed - errors,
            errors,
            pass_rate,
            duration_ms,
            usage,
            judge_usage,
            latency_ms: latency_summary(tests),
            metrics,
            trials: None,
            gate: {
                let mut gate =
                    GateResult { pass_rate: required_pass_rate, pass_hat_k: None, passed: true, failures: Vec::new() };
                gate.require(pass_rate + 1e-9 >= required_pass_rate, || {
                    format!("pass rate {pass_rate:.3} < {required_pass_rate:.3}")
                });
                gate
            },
        }
    }
}

fn latency_summary(tests: &[TestResult]) -> LatencySummary {
    let mut latencies: Vec<u64> = tests.iter().map(|t| t.run.latency_ms).collect();
    if latencies.is_empty() {
        return LatencySummary::default();
    }
    latencies.sort_unstable();
    let percentile = |p: f64| {
        let rank = ((p * latencies.len() as f64).ceil() as usize).clamp(1, latencies.len());
        latencies.get(rank - 1).copied().unwrap_or_default()
    };
    LatencySummary {
        mean: latencies.iter().sum::<u64>() as f64 / latencies.len() as f64,
        p50: percentile(0.5),
        p95: percentile(0.95),
        max: latencies.last().copied().unwrap_or_default(),
    }
}

/// The workflow under test.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkflowInfo {
    pub path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// `graphrt.workflow.fingerprint`: changes whenever the workflow does.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fingerprint: String,
}

/// The full report (`--output results.json`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EvalReport {
    pub schema_version: u32,
    /// graphrt-eval version that produced the report.
    pub graphrt_version: String,
    /// The eval file.
    pub suite: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub workflow: WorkflowInfo,
    /// Unix time in milliseconds when the evaluation started.
    pub started_at_ms: u64,
    pub summary: Summary,
    /// Per test case over its trials (with more than one trial or a `pass_hat_k` gate).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cases: Vec<CaseSummary>,
    /// Judge agreement with the human labels (calibration suites).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration: Option<CalibrationSummary>,
    /// The comparison with `--baseline`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<BaselineComparison>,
    /// Results in suite order (trials next to each other).
    pub tests: Vec<TestResult>,
}

impl EvalReport {
    /// Whether the gate passed.
    pub fn gate_passed(&self) -> bool {
        self.summary.gate.passed
    }

    /// Pretty JSON.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let mut text = serde_json::to_string_pretty(self)?;
        text.push('\n');
        Ok(text)
    }

    /// JUnit XML: one `<testsuite>` with a `<testcase>` per test. Failed tests
    /// carry a `<failure>`, run errors an `<error>`.
    pub fn to_junit(&self) -> String {
        let suite_name = if self.workflow.name.is_empty() { &self.suite } else { &self.workflow.name };
        let summary = &self.summary;
        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        let _ = writeln!(
            xml,
            "<testsuites name=\"graphrt eval\" tests=\"{}\" failures=\"{}\" errors=\"{}\" time=\"{}\">",
            summary.total,
            summary.failed,
            summary.errors,
            seconds(summary.duration_ms)
        );
        let _ = writeln!(
            xml,
            "  <testsuite name=\"{}\" tests=\"{}\" failures=\"{}\" errors=\"{}\" time=\"{}\">",
            xml_escape(suite_name),
            summary.total,
            summary.failed,
            summary.errors,
            seconds(summary.duration_ms)
        );
        for test in &self.tests {
            let _ = write!(
                xml,
                "    <testcase name=\"{}\" classname=\"{}\" time=\"{}\"",
                xml_escape(&test.display_id()),
                xml_escape(suite_name),
                seconds(test.run.latency_ms)
            );
            let details = failure_details(test);
            match test.status {
                TestStatus::Passed => xml.push_str(" />\n"),
                TestStatus::Failed | TestStatus::Error => {
                    let tag = if test.status == TestStatus::Error { "error" } else { "failure" };
                    let message = test.reason.as_deref().unwrap_or("failed");
                    let _ = writeln!(
                        xml,
                        ">\n      <{tag} message=\"{}\" type=\"{}\">{}</{tag}>\n    </testcase>",
                        xml_escape(message),
                        if tag == "error" { test.run.error_type.as_deref().unwrap_or("error") } else { "assertion" },
                        xml_escape(&details)
                    );
                }
            }
        }
        xml.push_str("  </testsuite>\n</testsuites>\n");
        xml
    }
}

fn seconds(ms: u64) -> String {
    format!("{:.3}", ms as f64 / 1000.0)
}

fn failure_details(test: &TestResult) -> String {
    let mut lines = Vec::new();
    if let Some(error) = &test.run.error {
        lines.push(format!("run {}: {error}", status_word(test.run.status)));
    }
    for a in test.assertions.iter().filter(|a| !a.pass && !a.excluded) {
        let note = if a.weight == 0.0 { " (report only)" } else { "" };
        lines.push(format!("{}{note}: {}", a.kind, a.reason));
    }
    lines.join("\n")
}

fn status_word(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Aborted => "aborted",
        RunStatus::Paused => "paused",
    }
}

/// Escape text for XML attributes and character data (control characters
/// other than tab and newlines are dropped: XML 1.0 cannot represent them).
pub fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// One terminal line for a finished test, e.g. `  PASS  greets-ada   3 ms  1.00`.
pub fn format_result_line(test: &TestResult) -> String {
    let label = match test.status {
        TestStatus::Passed => "PASS ",
        TestStatus::Failed => "FAIL ",
        TestStatus::Error => "ERROR",
    };
    let mut line =
        format!("  {label} {:<28} {:>7} ms  score {:.2}", test.display_id(), test.run.latency_ms, test.score);
    if let Some(reason) = &test.reason {
        let _ = write!(line, "  {reason}");
    }
    line
}

/// The closing summary block: metrics, trials, calibration, the baseline
/// comparison and totals with the gate verdict.
pub fn format_summary(report: &EvalReport) -> String {
    let s = &report.summary;
    let mut out = String::new();
    if !s.metrics.is_empty() {
        out.push_str("\n  metric                          pass      mean\n");
        for (name, m) in &s.metrics {
            let _ = write!(out, "  {name:<30} {:>3}/{:<3}  {:>6.3}", m.passed, m.count, m.mean_score);
            if m.excluded > 0 {
                let _ = write!(out, "  ({} excluded)", m.excluded);
            }
            out.push('\n');
        }
    }
    if let Some(t) = &s.trials {
        let _ = writeln!(out, "\n  test case                      passed  pass@{0}  pass^{0}", t.k);
        for c in &report.cases {
            let _ = writeln!(
                out,
                "  {:<30} {:>3}/{:<3} {:>6.3}  {:>6.3}",
                c.id, c.passed, c.trials, c.pass_at_k, c.pass_hat_k
            );
        }
        let _ = writeln!(
            out,
            "pass@{0} {1:.3}, pass^{0} {2:.3} over {3} case(s), up to {4} trial(s) each",
            t.k,
            t.pass_at_k,
            t.pass_hat_k,
            report.cases.len(),
            t.trials
        );
    }
    if let Some(c) = &report.calibration {
        out.push_str(&crate::calibration::format_calibration(c));
    }
    if let Some(b) = &report.baseline {
        out.push_str(&crate::baseline::format_baseline(b));
    }
    let _ = write!(
        out,
        "\n{} test(s): {} passed, {} failed, {} error(s); pass rate {:.1}% (gate {:.1}%) in {} ms, {} tokens",
        s.total,
        s.passed,
        s.failed,
        s.errors,
        s.pass_rate * 100.0,
        s.gate.pass_rate * 100.0,
        s.duration_ms,
        s.usage.total_tokens
    );
    if s.judge_usage.total_tokens > 0 {
        let _ = write!(out, " (+{} judge tokens)", s.judge_usage.total_tokens);
    }
    out.push('\n');
    if s.gate.passed {
        out.push_str("gate: PASSED\n");
    } else {
        let _ = writeln!(out, "gate: FAILED ({})", s.gate.failures.join("; "));
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    pub(crate) fn result(id: &str, status: TestStatus, latency_ms: u64) -> TestResult {
        TestResult {
            id: id.into(),
            description: None,
            source: "inline".into(),
            tags: vec![],
            trial: 0,
            status,
            pass: status == TestStatus::Passed,
            score: if status == TestStatus::Passed { 1.0 } else { 0.0 },
            reason: (status != TestStatus::Passed).then(|| "contains: \"x\" not found <in> & out".to_owned()),
            vars: Map::new(),
            expected: None,
            run: RunSummary { latency_ms, ..RunSummary::default() },
            assertions: vec![AssertionResult::new(
                "contains",
                status == TestStatus::Passed,
                if status == TestStatus::Passed { 1.0 } else { 0.0 },
                1.0,
                "r",
            )],
        }
    }

    fn report() -> EvalReport {
        let tests = vec![
            result("a", TestStatus::Passed, 10),
            result("b", TestStatus::Failed, 30),
            result("c", TestStatus::Error, 20),
        ];
        EvalReport {
            schema_version: REPORT_SCHEMA_VERSION,
            graphrt_version: "0".into(),
            suite: "s.eval.yaml".into(),
            description: None,
            workflow: WorkflowInfo { path: "w.yaml".into(), name: "wf".into(), fingerprint: "f".into() },
            started_at_ms: 0,
            summary: Summary::of(&tests, 0.5, 60),
            cases: Vec::new(),
            calibration: None,
            baseline: None,
            tests,
        }
    }

    #[test]
    fn summary_and_gate() {
        let r = report();
        let s = &r.summary;
        assert_eq!((s.total, s.passed, s.failed, s.errors), (3, 1, 1, 1));
        assert!((s.pass_rate - 1.0 / 3.0).abs() < 1e-9);
        assert!(!s.gate.passed);
        assert_eq!((s.latency_ms.p50, s.latency_ms.max), (20, 30));
        assert_eq!(s.metrics["contains"].count, 3);
        assert!(Summary::of(&r.tests[..1], 1.0, 1).gate.passed);
        assert!(Summary::of(&[], 1.0, 1).gate.passed);
        let text = format_summary(&r);
        assert!(text.contains("1 passed, 1 failed, 1 error(s)") && text.contains("gate: FAILED"), "{text}");
        assert!(format_result_line(&r.tests[1]).starts_with("  FAIL  b"));
    }

    #[test]
    fn json_round_trips() {
        let r = report();
        let text = r.to_json().unwrap();
        let back: EvalReport = serde_json::from_str(&text).unwrap();
        assert_eq!(back, r);
        let value: Json = serde_json::from_str(&text).unwrap();
        assert_eq!(value["schema_version"], 2);
        assert_eq!(value["tests"][1]["status"], "failed");
        assert_eq!(value["tests"][0]["assertions"][0]["type"], "contains");
    }

    #[test]
    fn junit_is_escaped_and_counts_failures() {
        let xml = report().to_junit();
        assert!(xml.starts_with("<?xml"));
        assert!(xml.contains("<testsuite name=\"wf\" tests=\"3\" failures=\"1\" errors=\"1\""), "{xml}");
        assert!(xml.contains("<testcase name=\"a\" classname=\"wf\" time=\"0.010\" />"), "{xml}");
        assert!(xml.contains("<failure message=\"contains: &quot;x&quot; not found &lt;in&gt; &amp; out\""), "{xml}");
        assert!(xml.contains("<error message="), "{xml}");
        assert_eq!(xml_escape("a\u{1}b<"), "ab&lt;");
        assert_eq!(xml.matches("<testcase").count(), 3);
    }
}
