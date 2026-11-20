//! Judge calibration: an eval whose workflow is a judge and whose `expected`
//! values are human labels. Every result is one (judge verdict, label) pair;
//! the report adds the confusion matrix, raw agreement, TPR, TNR and Cohen's
//! kappa (raw agreement alone flatters a judge on imbalanced labels).

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::capture::RunStatus;
use crate::config::Calibration;
use crate::judge::{Judgement, UnknownPolicy, Verdict};
use crate::report::{GateResult, TestResult};
use crate::stats::Confusion;

/// How well the judge agrees with the labels.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CalibrationSummary {
    /// Results with a `pass` / `fail` label.
    pub labelled: u32,
    /// Results in the confusion matrix.
    pub judged: u32,
    /// The judge said `unknown` (outside the matrix with `unknown: exclude`).
    pub unknown: u32,
    /// The judge run failed or returned no verdict.
    pub errors: u32,
    pub unknown_policy: UnknownPolicy,
    pub confusion: Confusion,
    pub agreement: Option<f64>,
    pub tpr: Option<f64>,
    pub tnr: Option<f64>,
    pub kappa: Option<f64>,
}

/// Compare every result's judge verdict with its label (`expected`).
pub fn calibrate(tests: &[TestResult], config: &Calibration) -> CalibrationSummary {
    let threshold = config.threshold.unwrap_or(0.5);
    let mut summary = CalibrationSummary { unknown_policy: config.unknown, ..CalibrationSummary::default() };
    for test in tests {
        let label = match test.expected.as_ref().and_then(Verdict::parse) {
            Some(Verdict::Pass) => true,
            Some(Verdict::Fail) => false,
            _ => continue,
        };
        summary.labelled += 1;
        let judgement =
            (test.run.status == RunStatus::Succeeded).then(|| Judgement::read(&test.run.outputs).ok()).flatten();
        let predicted = match judgement.map(|j| j.decide(threshold)) {
            None => {
                summary.errors += 1;
                continue;
            }
            Some(Verdict::Unknown) => {
                summary.unknown += 1;
                if config.unknown == UnknownPolicy::Exclude {
                    continue;
                }
                false
            }
            Some(verdict) => verdict == Verdict::Pass,
        };
        summary.confusion.add(predicted, label);
    }
    summary.judged = summary.confusion.total();
    let metrics = summary.confusion.agreement();
    summary.agreement = metrics.agreement;
    summary.tpr = metrics.tpr;
    summary.tnr = metrics.tnr;
    summary.kappa = metrics.kappa;
    summary
}

/// Apply the calibration thresholds (`min_kappa`, `min_tpr`, `min_tnr`) to the
/// gate. An undefined metric (no examples of a class) fails its threshold.
pub fn gate(summary: &CalibrationSummary, config: &Calibration, gate: &mut GateResult) {
    let mut check = |name: &str, value: Option<f64>, min: Option<f64>| {
        if let Some(min) = min {
            gate.require(value.is_some_and(|v| v + 1e-9 >= min), || match value {
                Some(v) => format!("{name} {v:.3} < {min:.3}"),
                None => format!("{name} undefined (needs {min:.3})"),
            });
        }
    };
    check("kappa", summary.kappa, config.min_kappa);
    check("TPR", summary.tpr, config.min_tpr);
    check("TNR", summary.tnr, config.min_tnr);
}

/// The terminal block for a calibration summary.
pub fn format_calibration(c: &CalibrationSummary) -> String {
    let metric = |v: Option<f64>| v.map_or_else(|| "n/a".to_owned(), |v| format!("{v:.3}"));
    let m = &c.confusion;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "\ncalibration: {} labelled, {} judged, {} unknown ({}), {} error(s)",
        c.labelled,
        c.judged,
        c.unknown,
        if c.unknown_policy == UnknownPolicy::Exclude { "excluded" } else { "counted as fail" },
        c.errors
    );
    let _ = writeln!(
        out,
        "  agreement {}  TPR {}  TNR {}  kappa {}",
        metric(c.agreement),
        metric(c.tpr),
        metric(c.tnr),
        metric(c.kappa)
    );
    let _ = writeln!(out, "                 label pass  label fail");
    let _ = writeln!(out, "  judge pass     {:>10}  {:>10}", m.tp, m.fp);
    let _ = writeln!(out, "  judge fail     {:>10}  {:>10}", m.fn_, m.tn);
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;
    use crate::report::{RunSummary, TestStatus};
    use serde_json::{Map, json};

    fn labelled(label: serde_json::Value, outputs: serde_json::Value, status: RunStatus) -> TestResult {
        TestResult {
            id: "t".into(),
            description: None,
            source: "inline".into(),
            tags: vec![],
            trial: 0,
            status: TestStatus::Passed,
            pass: true,
            score: 1.0,
            reason: None,
            vars: Map::new(),
            expected: Some(label),
            run: RunSummary { status, outputs: outputs.as_object().cloned().unwrap(), ..RunSummary::default() },
            assertions: vec![],
        }
    }

    fn suite() -> Vec<TestResult> {
        let ok = RunStatus::Succeeded;
        vec![
            labelled(json!("pass"), json!({"verdict": "pass"}), ok), // tp
            labelled(json!("pass"), json!({"verdict": "pass"}), ok), // tp
            labelled(json!(true), json!({"verdict": "fail"}), ok),   // fn
            labelled(json!("fail"), json!({"verdict": "fail"}), ok), // tn
            labelled(json!("fail"), json!({"score": 0.9}), ok),      // fp (score >= 0.5)
            labelled(json!("fail"), json!({"verdict": "unknown"}), ok),
            labelled(json!("pass"), json!({"text": "no verdict"}), ok),
            labelled(json!("pass"), json!({"verdict": "pass"}), RunStatus::Failed),
            labelled(json!("n/a"), json!({"verdict": "pass"}), ok), // not a label
        ]
    }

    #[test]
    fn computes_the_confusion_matrix_and_agreement() {
        let c = calibrate(&suite(), &Calibration::default());
        assert_eq!((c.labelled, c.judged, c.unknown, c.errors), (8, 5, 1, 2));
        assert_eq!(c.confusion, Confusion { tp: 2, fp: 1, tn: 1, fn_: 1 });
        assert!((c.agreement.unwrap() - 0.6).abs() < 1e-12);
        assert!((c.tpr.unwrap() - 2.0 / 3.0).abs() < 1e-12);
        assert!((c.tnr.unwrap() - 0.5).abs() < 1e-12);
        // po = 0.6, pe = 0.6*0.6 + 0.4*0.4 = 0.52: kappa = 0.08 / 0.48.
        assert!((c.kappa.unwrap() - 0.08 / 0.48).abs() < 1e-12);
        let text = format_calibration(&c);
        assert!(text.contains("8 labelled, 5 judged, 1 unknown (excluded), 2 error(s)"), "{text}");
        assert!(text.contains("kappa 0.167"), "{text}");
    }

    #[test]
    fn unknown_can_count_as_fail_and_thresholds_gate() {
        let config = Calibration { unknown: UnknownPolicy::Fail, min_kappa: Some(0.5), ..Calibration::default() };
        let c = calibrate(&suite(), &config);
        assert_eq!(c.confusion, Confusion { tp: 2, fp: 1, tn: 2, fn_: 1 });
        let mut result = GateResult { pass_rate: 0.0, pass_hat_k: None, passed: true, failures: vec![] };
        gate(&c, &config, &mut result);
        assert!(!result.passed);
        assert!(result.failures[0].starts_with("kappa 0.333 < 0.500"), "{:?}", result.failures);
        let none = calibrate(&[], &Calibration { min_tnr: Some(0.5), ..Calibration::default() });
        let mut result = GateResult { pass_rate: 0.0, pass_hat_k: None, passed: true, failures: vec![] };
        gate(&none, &Calibration { min_tnr: Some(0.5), ..Calibration::default() }, &mut result);
        assert!(result.failures[0].contains("TNR undefined"), "{:?}", result.failures);
    }
}
