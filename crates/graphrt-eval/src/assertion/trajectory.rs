//! Trajectory assertions over the agent tool calls of a run (the `tool_call`
//! progress events, enriched with the agent's `tool_calls` output). Value
//! shapes follow Promptfoo's `trajectory:*` assertions.
//!
//! Tool names match the exposed name, the tool name or `provider/tool`, with
//! `*` wildcards.

use serde_json::Value as Json;

use super::{AssertionRegistry, Outcome, only_keys, sync_assertion};
use crate::capture::ToolCallRecord;
use crate::config::AssertionSpec;
use crate::error::EvalError;

pub(super) fn register(registry: &mut AssertionRegistry) {
    registry.register("trajectory:tool-used", tool_used);
    registry.register("trajectory:tool-sequence", tool_sequence);
    registry.register("trajectory:tool-args-match", tool_args_match);
    registry.register("trajectory:step-count", step_count);
}

fn names(calls: &[ToolCallRecord]) -> String {
    if calls.is_empty() {
        return "no tool calls".to_owned();
    }
    calls.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(" -> ")
}

fn string_list(value: &Json, what: &str) -> Result<Vec<String>, EvalError> {
    match value {
        Json::String(s) => Ok(vec![s.clone()]),
        Json::Array(items) => items
            .iter()
            .map(|v| v.as_str().map(str::to_owned).ok_or_else(|| EvalError::invalid(format!("{what} must be strings"))))
            .collect(),
        _ => Err(EvalError::invalid(format!("{what} must be a string or a list of strings"))),
    }
}

fn count_bound(map: &serde_json::Map<String, Json>, key: &str) -> Result<Option<usize>, EvalError> {
    match map.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| EvalError::invalid(format!("`{key}` must be a non-negative integer"))),
    }
}

fn check_object_keys(map: &serde_json::Map<String, Json>, allowed: &[&str]) -> Result<(), EvalError> {
    match map.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(key) => Err(EvalError::invalid(format!("unknown key `{key}` in `value`"))),
        None => Ok(()),
    }
}

struct Requirement {
    pattern: String,
    min: usize,
    max: Option<usize>,
}

/// `value: name`, `value: [a, b]` (each used at least once) or
/// `value: {name, min?, max?}` (number of matching calls in range).
fn tool_used(spec: &AssertionSpec) -> Result<Box<dyn super::Assertion>, EvalError> {
    only_keys(spec, &[])?;
    let value = spec.value.as_ref().ok_or_else(|| EvalError::invalid("`value` (tool name or names) is required"))?;
    let requirements = match value {
        Json::Object(map) => {
            check_object_keys(map, &["name", "min", "max"])?;
            let pattern = map
                .get("name")
                .and_then(Json::as_str)
                .ok_or_else(|| EvalError::invalid("`value.name` is required"))?
                .to_owned();
            vec![Requirement { pattern, min: count_bound(map, "min")?.unwrap_or(1), max: count_bound(map, "max")? }]
        }
        other => string_list(other, "tool names")?
            .into_iter()
            .map(|pattern| Requirement { pattern, min: 1, max: None })
            .collect(),
    };
    if requirements.is_empty() {
        return Err(EvalError::invalid("`value` must name at least one tool"));
    }
    Ok(sync_assertion("tool-used", move |ctx| {
        let calls = ctx.capture.tool_calls();
        let mut problems = Vec::new();
        for req in &requirements {
            let n = calls.iter().filter(|c| c.matches(&req.pattern)).count();
            if n < req.min || req.max.is_some_and(|max| n > max) {
                let range = match req.max {
                    Some(max) => format!("{}..={max}", req.min),
                    None => format!(">= {}", req.min),
                };
                problems.push(format!("`{}` called {n} time(s), expected {range}", req.pattern));
            }
        }
        let satisfied = requirements.len() - problems.len();
        let score = satisfied as f64 / requirements.len() as f64;
        let reason = if problems.is_empty() {
            format!("used as expected ({})", names(&calls))
        } else {
            format!("{} ({})", problems.join("; "), names(&calls))
        };
        Outcome::graded(problems.is_empty(), score, reason)
    }))
}

/// `value: [a, b, c]` or `value: {steps: [...], mode: in_order|exact}`.
/// `in_order` (default) needs the steps as an ordered subsequence of the calls;
/// `exact` needs exactly these calls in this order.
fn tool_sequence(spec: &AssertionSpec) -> Result<Box<dyn super::Assertion>, EvalError> {
    only_keys(spec, &[])?;
    let value = spec.value.as_ref().ok_or_else(|| EvalError::invalid("`value` (the tool sequence) is required"))?;
    let (steps, exact) = match value {
        Json::Object(map) => {
            check_object_keys(map, &["steps", "mode"])?;
            let steps =
                string_list(map.get("steps").ok_or_else(|| EvalError::invalid("`value.steps` is required"))?, "steps")?;
            let exact = match map.get("mode").and_then(Json::as_str) {
                None | Some("in_order") => false,
                Some("exact") => true,
                Some(other) => {
                    return Err(EvalError::invalid(format!("unknown mode `{other}` (expected `in_order` or `exact`)")));
                }
            };
            (steps, exact)
        }
        other => (string_list(other, "steps")?, false),
    };
    if steps.is_empty() {
        return Err(EvalError::invalid("the sequence must not be empty"));
    }
    Ok(sync_assertion("tool-sequence", move |ctx| {
        let calls = ctx.capture.tool_calls();
        let (pass, matched) = if exact {
            let matched = steps.iter().zip(&calls).take_while(|(step, call)| call.matches(step)).count();
            (calls.len() == steps.len() && matched == steps.len(), matched)
        } else {
            let mut next = 0;
            for call in &calls {
                if steps.get(next).is_some_and(|step| call.matches(step)) {
                    next += 1;
                }
            }
            (next == steps.len(), next)
        };
        let score = matched as f64 / steps.len() as f64;
        let mode = if exact { "exactly" } else { "in order" };
        let reason = if pass {
            format!("called {} {mode}", steps.join(" -> "))
        } else {
            format!("expected {} {mode}, got {} ({matched} step(s) matched)", steps.join(" -> "), names(&calls))
        };
        Outcome::graded(pass, score, reason)
    }))
}

/// Whether every key of `expected` is in `actual` with a matching value
/// (recursively for objects; numbers compare by value).
fn subset(expected: &Json, actual: &Json) -> bool {
    match (expected, actual) {
        (Json::Object(e), Json::Object(a)) => e.iter().all(|(k, v)| a.get(k).is_some_and(|w| subset(v, w))),
        (Json::Number(x), Json::Number(y)) => x.as_f64() == y.as_f64(),
        (Json::Array(e), Json::Array(a)) => e.len() == a.len() && e.iter().zip(a).all(|(x, y)| subset(x, y)),
        (Json::String(e), Json::Number(n)) => e.trim().parse::<f64>().ok() == n.as_f64(),
        (Json::String(e), Json::Bool(b)) => e == if *b { "true" } else { "false" },
        _ => expected == actual,
    }
}

/// `value: {name, args: {...}, mode: partial|exact}`: some call of the tool has
/// these arguments (`partial`, default: a subset; `exact`: equal).
fn tool_args_match(spec: &AssertionSpec) -> Result<Box<dyn super::Assertion>, EvalError> {
    only_keys(spec, &[])?;
    let Some(Json::Object(map)) = spec.value.as_ref() else {
        return Err(EvalError::invalid("`value` must be `{name, args, mode?}`"));
    };
    check_object_keys(map, &["name", "args", "mode"])?;
    let pattern = map
        .get("name")
        .and_then(Json::as_str)
        .ok_or_else(|| EvalError::invalid("`value.name` is required"))?
        .to_owned();
    let args = map.get("args").cloned().unwrap_or_else(|| Json::Object(serde_json::Map::new()));
    if !args.is_object() {
        return Err(EvalError::invalid("`value.args` must be a mapping"));
    }
    let exact = match map.get("mode").and_then(Json::as_str) {
        None | Some("partial") => false,
        Some("exact") => true,
        Some(other) => {
            return Err(EvalError::invalid(format!("unknown mode `{other}` (expected `partial` or `exact`)")));
        }
    };
    Ok(sync_assertion("tool-args-match", move |ctx| {
        let calls = ctx.capture.tool_calls();
        let candidates: Vec<&ToolCallRecord> = calls.iter().filter(|c| c.matches(&pattern)).collect();
        if candidates.is_empty() {
            return Outcome::check(false, format!("`{pattern}` was not called ({})", names(&calls)));
        }
        let hit = candidates.iter().any(|c| {
            if exact { subset(&args, &c.arguments) && subset(&c.arguments, &args) } else { subset(&args, &c.arguments) }
        });
        if hit {
            Outcome::check(true, format!("`{pattern}` called with {}", super::brief(&args)))
        } else {
            let seen: Vec<String> = candidates.iter().map(|c| super::brief(&c.arguments)).collect();
            Outcome::check(
                false,
                format!("`{pattern}` never called with {}; got {}", super::brief(&args), seen.join(", ")),
            )
        }
    }))
}

#[derive(Clone, Copy)]
enum StepKind {
    Tool,
    AgentStep,
    Node,
}

/// `value: {type: tool|step|node, min?, max?, pattern?}`, or a number meaning
/// "at most this many tool calls". `tool` counts tool calls (optionally only
/// those matching `pattern`), `step` agent model turns, `node` node executions.
fn step_count(spec: &AssertionSpec) -> Result<Box<dyn super::Assertion>, EvalError> {
    only_keys(spec, &[])?;
    let (kind, pattern, min, max) = match spec.value.as_ref() {
        Some(Json::Object(map)) => {
            check_object_keys(map, &["type", "min", "max", "pattern"])?;
            let kind = match map.get("type").and_then(Json::as_str) {
                None | Some("tool") => StepKind::Tool,
                Some("step" | "agent_step") => StepKind::AgentStep,
                Some("node") => StepKind::Node,
                Some(other) => {
                    return Err(EvalError::invalid(format!(
                        "unknown step type `{other}` (expected tool, step or node)"
                    )));
                }
            };
            let pattern = map.get("pattern").and_then(Json::as_str).map(str::to_owned);
            (kind, pattern, count_bound(map, "min")?, count_bound(map, "max")?)
        }
        Some(Json::Number(n)) => {
            let max = n.as_u64().and_then(|n| usize::try_from(n).ok());
            (StepKind::Tool, None, None, Some(max.ok_or_else(|| EvalError::invalid("`value` must be a count"))?))
        }
        _ => return Err(EvalError::invalid("`value` must be `{type, min?, max?}` or a maximum count")),
    };
    if min.is_none() && max.is_none() {
        return Err(EvalError::invalid("set `min`, `max` or both"));
    }
    Ok(sync_assertion("step-count", move |ctx| {
        let (n, what) = match kind {
            StepKind::Tool => {
                let calls = ctx.capture.tool_calls();
                (calls.iter().filter(|c| pattern.as_deref().is_none_or(|p| c.matches(p))).count(), "tool call(s)")
            }
            StepKind::AgentStep => (ctx.capture.agent_steps(), "agent step(s)"),
            StepKind::Node => (ctx.capture.node_executions(), "node execution(s)"),
        };
        let pass = min.is_none_or(|m| n >= m) && max.is_none_or(|m| n <= m);
        let range = match (min, max) {
            (Some(a), Some(b)) => format!("{a}..={b}"),
            (Some(a), None) => format!(">= {a}"),
            (None, Some(b)) => format!("<= {b}"),
            (None, None) => String::new(),
        };
        Outcome::check(pass, format!("{n} {what}, expected {range}"))
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use graphrt_core::{Event, Value};
    use serde_json::json;

    use super::super::test_support::*;
    use crate::capture::RunCapture;

    fn progress(name: &str, data: serde_json::Value) -> Event {
        Event::NodeProgress {
            frame: Arc::from(""),
            node_id: Arc::from("agent"),
            exec_id: Arc::from("agent#1"),
            name: name.into(),
            data: Value::from_json(data),
        }
    }

    /// An agent that calls `search_orders(order_id=123)` then `send_email`.
    fn agent_run() -> RunCapture {
        let mut outputs = graphrt_core::variables::Variables::new();
        outputs.insert(
            "tool_calls",
            Value::from_json(json!([
                {"name": "search_orders", "provider": "orders", "tool": "search", "arguments": {"order_id": 123}, "result": "shipped", "is_error": false},
                {"name": "send_email", "provider": "mail", "tool": "send", "arguments": {"to": "a@b.c"}, "result": "ok", "is_error": false}
            ])),
        );
        let events = vec![
            progress("agent_step", json!({"iteration": 1})),
            progress(
                "tool_call",
                json!({"id": "c1", "name": "search_orders", "arguments": {"order_id": 123, "verbose": true}}),
            ),
            progress("tool_result", json!({"id": "c1", "name": "search_orders", "is_error": false})),
            progress("agent_step", json!({"iteration": 2})),
            progress("tool_call", json!({"id": "c2", "name": "send_email", "arguments": {"to": "a@b.c"}})),
            progress("agent_step", json!({"iteration": 3})),
            Event::NodeSucceeded {
                frame: Arc::from(""),
                node_id: Arc::from("agent"),
                exec_id: Arc::from("agent#1"),
                outputs,
                branch: None,
                usage: None,
                metadata: Default::default(),
                elapsed_ms: 1,
            },
        ];
        RunCapture::from_events(events, String::new(), 5)
    }

    #[test]
    fn tool_used() {
        let run = agent_run();
        let calls = run.tool_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].provider, "orders", "enriched from the tool_calls output");
        let ok = |v| eval(json!({"type": "trajectory:tool-used", "value": v}), &run).pass;
        assert!(ok(json!("search_orders")));
        assert!(ok(json!("orders/search")), "provider/tool");
        assert!(ok(json!("search")), "tool name");
        assert!(ok(json!(["search_*", "send_email"])));
        assert!(!ok(json!(["search_orders", "refund"])));
        assert!(ok(json!({"name": "send_email", "min": 1, "max": 1})));
        assert!(!ok(json!({"name": "*", "max": 1})));
        assert!(eval(json!({"type": "not-trajectory:tool-used", "value": "refund"}), &run).pass);
        let partial = eval(json!({"type": "trajectory:tool-used", "value": ["search_orders", "refund"]}), &run);
        assert_eq!(partial.score, 0.5);
    }

    #[test]
    fn tool_sequence() {
        let run = agent_run();
        let ok = |v| eval(json!({"type": "trajectory:tool-sequence", "value": v}), &run).pass;
        assert!(ok(json!(["search_orders", "send_email"])));
        assert!(ok(json!(["send_email"])), "a subsequence is enough in order mode");
        assert!(!ok(json!(["send_email", "search_orders"])));
        assert!(ok(json!({"steps": ["search_orders", "send_email"], "mode": "exact"})));
        assert!(!ok(json!({"steps": ["send_email"], "mode": "exact"})));
        assert!(
            try_eval(json!({"type": "trajectory:tool-sequence", "value": {"steps": ["a"], "mode": "fuzzy"}}), &run)
                .is_err()
        );
    }

    #[test]
    fn tool_args_match() {
        let run = agent_run();
        let ok = |v| eval(json!({"type": "trajectory:tool-args-match", "value": v}), &run).pass;
        assert!(ok(json!({"name": "search_orders", "args": {"order_id": 123}})));
        assert!(ok(json!({"name": "search_orders", "args": {"order_id": "123"}})), "strings match numbers");
        assert!(!ok(json!({"name": "search_orders", "args": {"order_id": 124}})));
        assert!(!ok(json!({"name": "search_orders", "args": {"order_id": 123}, "mode": "exact"})));
        assert!(ok(json!({"name": "send_email", "args": {"to": "a@b.c"}, "mode": "exact"})));
        assert!(!ok(json!({"name": "refund", "args": {}})));
        assert!(try_eval(json!({"type": "trajectory:tool-args-match", "value": "x"}), &run).is_err());
    }

    #[test]
    fn step_count() {
        let run = agent_run();
        let ok = |v| eval(json!({"type": "trajectory:step-count", "value": v}), &run).pass;
        assert!(ok(json!({"type": "tool", "min": 1, "max": 2})));
        assert!(!ok(json!({"type": "tool", "max": 1})));
        assert!(ok(json!({"type": "tool", "pattern": "send_*", "max": 1})));
        assert!(ok(json!({"type": "step", "min": 3, "max": 3})));
        assert!(ok(json!({"type": "node", "min": 1})));
        assert!(ok(json!(2)));
        assert!(!ok(json!(1)));
        assert!(try_eval(json!({"type": "trajectory:step-count", "value": {"type": "tool"}}), &run).is_err());
    }
}
