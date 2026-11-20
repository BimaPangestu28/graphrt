//! Statistics over repeated trials and judge calibration.
//!
//! - [`pass_at_k`] and [`pass_hat_k`]: unbiased estimators from `n` trials with
//!   `c` successes of the chance that at least one of `k` trials passes
//!   (capability) and that all `k` pass (reliability, τ-bench's pass^k).
//! - [`Agreement`]: a judge's binary verdicts against human labels: confusion
//!   matrix, raw agreement, TPR, TNR and Cohen's kappa.

use serde::{Deserialize, Serialize};

/// Unbiased pass@k from `n` trials with `c` passes: `1 - C(n-c, k) / C(n, k)`
/// (Chen et al. 2021, the Codex estimator of `1 - (1 - p)^k`).
///
/// `k` is clamped to `1..=n`; with no trials the result is 0.
///
/// ```
/// // 5 trials, 2 passed: 1 - C(3,2)/C(5,2) = 1 - 3/10
/// assert!((graphrt_eval::stats::pass_at_k(5, 2, 2) - 0.7).abs() < 1e-12);
/// ```
pub fn pass_at_k(n: u32, c: u32, k: u32) -> f64 {
    if n == 0 {
        return 0.0;
    }
    1.0 - choose_ratio(n - c.min(n), n, k.clamp(1, n))
}

/// Unbiased pass^k from `n` trials with `c` passes: `C(c, k) / C(n, k)`, the
/// probability that `k` trials drawn without replacement all passed; its
/// expectation is `p^k` for a per-trial pass probability `p`.
///
/// `k` is clamped to `1..=n`; with no trials the result is 0.
///
/// ```
/// // 5 trials, 2 passed: C(2,2)/C(5,2) = 1/10
/// assert!((graphrt_eval::stats::pass_hat_k(5, 2, 2) - 0.1).abs() < 1e-12);
/// ```
pub fn pass_hat_k(n: u32, c: u32, k: u32) -> f64 {
    if n == 0 {
        return 0.0;
    }
    choose_ratio(c.min(n), n, k.clamp(1, n))
}

/// `C(a, k) / C(n, k)` for `a <= n`, as the product `Π (a - i) / (n - i)` over
/// `i < k` (no factorials, so no overflow); 0 when `a < k`.
fn choose_ratio(a: u32, n: u32, k: u32) -> f64 {
    if a < k {
        return 0.0;
    }
    (0..k).map(|i| f64::from(a - i) / f64::from(n - i)).product()
}

/// A binary judge against binary human labels (positive = `pass`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Confusion {
    /// Judge pass, label pass.
    pub tp: u32,
    /// Judge pass, label fail.
    pub fp: u32,
    /// Judge fail, label fail.
    pub tn: u32,
    /// Judge fail, label pass.
    #[serde(rename = "fn")]
    pub fn_: u32,
}

impl Confusion {
    /// Count one judged example.
    pub fn add(&mut self, predicted: bool, label: bool) {
        match (predicted, label) {
            (true, true) => self.tp += 1,
            (true, false) => self.fp += 1,
            (false, false) => self.tn += 1,
            (false, true) => self.fn_ += 1,
        }
    }

    pub fn total(&self) -> u32 {
        self.tp + self.fp + self.tn + self.fn_
    }

    /// The metrics of this matrix.
    pub fn agreement(&self) -> Agreement {
        let n = f64::from(self.total());
        let ratio = |num: u32, den: u32| (den > 0).then(|| f64::from(num) / f64::from(den));
        let observed = ratio(self.tp + self.tn, self.total());
        // Chance agreement from the marginals: P(both pass) + P(both fail).
        let kappa = observed.and_then(|po| {
            let judge_pass = f64::from(self.tp + self.fp) / n;
            let label_pass = f64::from(self.tp + self.fn_) / n;
            let pe = judge_pass * label_pass + (1.0 - judge_pass) * (1.0 - label_pass);
            // Kappa is undefined when chance agreement is certain (one class only).
            (pe < 1.0 - 1e-12).then(|| (po - pe) / (1.0 - pe))
        });
        Agreement {
            agreement: observed,
            tpr: ratio(self.tp, self.tp + self.fn_),
            tnr: ratio(self.tn, self.tn + self.fp),
            kappa,
        }
    }
}

/// Agreement metrics; `None` where the denominator is zero (or, for kappa,
/// when chance agreement is 1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Agreement {
    /// `(tp + tn) / n`.
    pub agreement: Option<f64>,
    /// Sensitivity: `tp / (tp + fn)`, passes the judge recognises.
    pub tpr: Option<f64>,
    /// Specificity: `tn / (tn + fp)`, failures the judge catches.
    pub tnr: Option<f64>,
    /// Cohen's kappa: `(po - pe) / (1 - pe)`, agreement corrected for chance.
    pub kappa: Option<f64>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    #[test]
    fn pass_at_k_matches_hand_computed_values() {
        // n=5, c=2, k=2: 1 - C(3,2)/C(5,2) = 1 - 3/10.
        assert!(close(pass_at_k(5, 2, 2), 0.7));
        // n=10, c=3, k=3: 1 - C(7,3)/C(10,3) = 1 - 35/120.
        assert!(close(pass_at_k(10, 3, 3), 1.0 - 35.0 / 120.0));
        // k=1 is the plain pass rate.
        assert!(close(pass_at_k(4, 1, 1), 0.25));
        // Too few failures to fill k draws: certain success.
        assert!(close(pass_at_k(5, 4, 2), 1.0));
        assert!(close(pass_at_k(3, 0, 3), 0.0));
        assert!(close(pass_at_k(3, 3, 3), 1.0));
        // k is clamped to n; no trials is 0.
        assert!(close(pass_at_k(2, 1, 5), 1.0));
        assert!(close(pass_at_k(0, 0, 1), 0.0));
    }

    #[test]
    fn pass_hat_k_matches_hand_computed_values() {
        // n=5, c=2, k=2: C(2,2)/C(5,2) = 1/10.
        assert!(close(pass_hat_k(5, 2, 2), 0.1));
        // n=10, c=7, k=3: C(7,3)/C(10,3) = 35/120.
        assert!(close(pass_hat_k(10, 7, 3), 35.0 / 120.0));
        // n=4, c=3, k=2: C(3,2)/C(4,2) = 3/6.
        assert!(close(pass_hat_k(4, 3, 2), 0.5));
        assert!(close(pass_hat_k(4, 1, 1), 0.25));
        assert!(close(pass_hat_k(3, 2, 3), 0.0));
        assert!(close(pass_hat_k(3, 3, 3), 1.0));
        assert!(close(pass_hat_k(2, 1, 5), 0.0), "k clamped to n=2");
        assert!(close(pass_hat_k(0, 0, 1), 0.0));
    }

    #[test]
    fn pass_hat_k_is_unbiased_for_p_to_the_k() {
        // Average the estimator over every outcome of n=4 Bernoulli(0.75) trials.
        let (n, k, p) = (4u32, 3u32, 0.75f64);
        let mut expected = 0.0;
        for c in 0..=n {
            let ways = (0..c).map(|i| f64::from(n - i) / f64::from(i + 1)).product::<f64>();
            let probability = ways * p.powi(c as i32) * (1.0 - p).powi((n - c) as i32);
            expected += probability * pass_hat_k(n, c, k);
        }
        assert!((expected - p.powi(3)).abs() < 1e-12, "{expected}");
    }

    #[test]
    fn agreement_metrics_match_hand_computed_values() {
        let mut m = Confusion::default();
        for (predicted, label, times) in [(true, true, 40), (true, false, 10), (false, false, 40), (false, true, 10)] {
            for _ in 0..times {
                m.add(predicted, label);
            }
        }
        let a = m.agreement();
        assert!(close(a.agreement.unwrap(), 0.8));
        assert!(close(a.tpr.unwrap(), 0.8));
        assert!(close(a.tnr.unwrap(), 0.8));
        // po = 0.8, pe = 0.5*0.5 + 0.5*0.5 = 0.5: kappa = 0.3 / 0.5.
        assert!(close(a.kappa.unwrap(), 0.6));

        // Skewed: tp 45, fp 15, tn 30, fn 10 (n=100).
        let m = Confusion { tp: 45, fp: 15, tn: 30, fn_: 10 };
        let a = m.agreement();
        assert!(close(a.agreement.unwrap(), 0.75));
        assert!(close(a.tpr.unwrap(), 45.0 / 55.0));
        assert!(close(a.tnr.unwrap(), 30.0 / 45.0));
        // pe = 0.6*0.55 + 0.4*0.45 = 0.51: kappa = 0.24 / 0.49.
        assert!(close(a.kappa.unwrap(), 0.24 / 0.49));
    }

    #[test]
    fn agreement_handles_degenerate_matrices() {
        let empty = Confusion::default().agreement();
        assert_eq!(empty, Agreement::default());
        // Everything labelled and judged pass: perfect agreement, kappa undefined.
        let all_pass = Confusion { tp: 5, ..Confusion::default() }.agreement();
        assert_eq!(all_pass.agreement, Some(1.0));
        assert_eq!(all_pass.tnr, None);
        assert_eq!(all_pass.kappa, None);
        // Always the opposite: kappa -1.
        let opposite = Confusion { fp: 3, fn_: 3, ..Confusion::default() }.agreement();
        assert!(close(opposite.kappa.unwrap(), -1.0));
    }
}
