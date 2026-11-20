//! Budgets: `latency` (milliseconds) and `tokens` (usage).

use serde_json::Value as Json;

use super::{AssertionRegistry, Outcome, only_keys, sync_assertion};
use crate::config::AssertionSpec;
use crate::error::EvalError;

pub(super) fn register(registry: &mut AssertionRegistry) {
    registry.register("latency", latency);
    registry.register("tokens", tokens);
}

/// The limit: `threshold`, or a numeric `value`.
fn limit(spec: &AssertionSpec, unit: &str) -> Result<f64, EvalError> {
    let limit = spec.threshold.or_else(|| spec.value.as_ref().and_then(Json::as_f64));
    limit.filter(|l| *l >= 0.0).ok_or_else(|| EvalError::invalid(format!("`threshold` (maximum {unit}) is required")))
}

fn latency(spec: &AssertionSpec) -> Result<Box<dyn super::Assertion>, EvalError> {
    only_keys(spec, &[])?;
    let max = limit(spec, "milliseconds")?;
    Ok(sync_assertion("latency", move |ctx| {
        let ms = ctx.capture.latency_ms;
        let pass = ms as f64 <= max;
        Outcome::check(pass, format!("latency {ms} ms {} {max} ms", if pass { "<=" } else { ">" }))
    }))
}

fn tokens(spec: &AssertionSpec) -> Result<Box<dyn super::Assertion>, EvalError> {
    only_keys(spec, &[])?;
    let max = spec
        .threshold
        .filter(|l| *l >= 0.0)
        .ok_or_else(|| EvalError::invalid("`threshold` (maximum tokens) is required"))?;
    let which = match spec.value.as_ref() {
        None | Some(Json::Null) => "total".to_owned(),
        Some(Json::String(s)) if matches!(s.as_str(), "total" | "prompt" | "completion") => s.clone(),
        Some(_) => return Err(EvalError::invalid("`value` must be `total`, `prompt` or `completion`")),
    };
    Ok(sync_assertion("tokens", move |ctx| {
        let usage = &ctx.capture.usage;
        let used = match which.as_str() {
            "prompt" => usage.prompt_tokens,
            "completion" => usage.completion_tokens,
            _ => usage.total_tokens,
        };
        let pass = used as f64 <= max;
        Outcome::check(pass, format!("{used} {which} tokens {} {max}", if pass { "<=" } else { ">" }))
    }))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use crate::capture::RunCapture;
    use graphrt_core::event::Usage;
    use serde_json::json;

    #[test]
    fn latency_and_token_budgets() {
        let capture = RunCapture {
            latency_ms: 120,
            usage: Usage { prompt_tokens: 900, completion_tokens: 100, total_tokens: 1000 },
            ..RunCapture::default()
        };
        assert!(eval(json!({"type": "latency", "threshold": 200}), &capture).pass);
        assert!(!eval(json!({"type": "latency", "threshold": 100}), &capture).pass);
        assert!(eval(json!({"type": "latency", "value": 120}), &capture).pass);
        assert!(eval(json!({"type": "tokens", "threshold": 1000}), &capture).pass);
        let over = eval(json!({"type": "tokens", "threshold": 500, "value": "prompt"}), &capture);
        assert!(!over.pass && over.reason.contains("900 prompt"), "{over:?}");
        assert!(eval(json!({"type": "tokens", "threshold": 100, "value": "completion"}), &capture).pass);
        assert!(try_eval(json!({"type": "tokens"}), &capture).is_err());
        assert!(try_eval(json!({"type": "tokens", "threshold": 1, "value": "cost"}), &capture).is_err());
        assert!(try_eval(json!({"type": "latency"}), &capture).is_err());
    }
}
