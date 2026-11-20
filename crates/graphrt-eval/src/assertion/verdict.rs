//! `verdict`: the run is a judge, and its verdict must equal a label. The
//! implicit assertion of calibration suites (`expected` holds the human label).

use serde_json::Value as Json;

use super::{AssertionRegistry, Outcome, only_keys, sync_assertion, transform_of};
use crate::config::AssertionSpec;
use crate::error::EvalError;
use crate::judge::{Judgement, Verdict};

pub(super) fn register(registry: &mut AssertionRegistry) {
    registry.register("verdict", verdict);
}

fn verdict(spec: &AssertionSpec) -> Result<Box<dyn super::Assertion>, EvalError> {
    only_keys(spec, &[])?;
    let wanted = spec
        .value
        .as_ref()
        .and_then(Verdict::parse)
        .ok_or_else(|| EvalError::invalid("`value` must be a label: pass, fail or unknown"))?;
    let threshold = spec.threshold.unwrap_or(0.5);
    let transform = transform_of(spec)?;
    Ok(sync_assertion("verdict", move |ctx| {
        let judgement = match &transform {
            None => Judgement::read(&ctx.capture.outputs),
            Some(path) => match path.get(&ctx.capture.subject_root(&ctx.case.vars)) {
                Some(Json::Object(map)) => Judgement::read(&map),
                Some(raw) => Verdict::parse(&raw)
                    .map(|v| Judgement { verdict: Some(v), ..Judgement::default() })
                    .ok_or_else(|| format!("{} is not a verdict", super::brief(&raw))),
                None => Err("transform path not found in the run".into()),
            },
        };
        match judgement {
            Ok(judgement) => {
                let got = judgement.decide(threshold);
                Outcome::check(got == wanted, format!("judge said {got}, label {wanted}"))
            }
            Err(why) => Outcome::check(false, why),
        }
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::super::test_support::*;
    use crate::capture::RunCapture;
    use serde_json::json;

    fn judged(outputs: serde_json::Value) -> RunCapture {
        RunCapture { outputs: outputs.as_object().cloned().unwrap(), ..RunCapture::default() }
    }

    #[test]
    fn compares_the_judge_verdict_with_the_label() {
        let capture = judged(json!({"verdict": "fail", "reasoning": "made up"}));
        assert!(eval(json!({"type": "verdict", "value": "fail"}), &capture).pass);
        let miss = eval(json!({"type": "verdict", "value": "pass"}), &capture);
        assert!(!miss.pass && miss.reason == "judge said fail, label pass", "{miss:?}");
        assert!(eval(json!({"type": "verdict", "value": false}), &capture).pass);
        let scored = judged(json!({"score": 0.8}));
        assert!(eval(json!({"type": "verdict", "value": "pass"}), &scored).pass);
        assert!(!eval(json!({"type": "verdict", "value": "pass", "threshold": 0.9}), &scored).pass);
        let nested = judged(json!({"out": {"label": "pass"}}));
        assert!(eval(json!({"type": "verdict", "value": "pass", "transform": "outputs.out.label"}), &nested).pass);
        assert!(try_eval(json!({"type": "verdict", "value": "maybe"}), &capture).is_err());
        assert!(!eval(json!({"type": "verdict", "value": "pass"}), &judged(json!({"x": 1}))).pass);
    }
}
