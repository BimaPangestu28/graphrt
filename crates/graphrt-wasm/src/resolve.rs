//! Host-side config resolution before a guest runs.
//!
//! Guests never see the variable pool. Instead the host rewrites the node
//! config:
//!
//! - Every string containing `{{#ns.name#}}` placeholders is resolved. A
//!   string that is exactly one placeholder becomes the typed value (object,
//!   number, array...); mixed text renders to a string.
//! - Every key ending in `selector` whose value is a selector (`["ns","name"]`
//!   or `"ns.name"`) keeps its value and gains a sibling `<key>_value` with the
//!   resolved value (`null` when missing).
//!
//! Secrets are masked unless the plugin is trusted with them
//! ([`crate::WasmLimits::expose_secrets`]).

use graphrt_core::pool::{Selector, VariablePool};
use graphrt_core::template::Template;
use graphrt_core::value::Value;
use serde_json::Value as Json;

/// Resolve placeholders and selector keys in `config` against `pool`.
pub fn resolve_config(config: &Json, pool: &VariablePool, expose_secrets: bool) -> Json {
    match config {
        Json::String(s) => resolve_string(s, pool, expose_secrets),
        Json::Array(items) => Json::Array(items.iter().map(|v| resolve_config(v, pool, expose_secrets)).collect()),
        Json::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (key, value) in map {
                if is_selector_key(key) {
                    if let Some(selector) = parse_selector(value) {
                        out.insert(key.clone(), value.clone());
                        let resolved = pool.get(&selector).map(|v| to_json(&v, expose_secrets)).unwrap_or(Json::Null);
                        out.insert(format!("{key}_value"), resolved);
                        continue;
                    }
                }
                out.insert(key.clone(), resolve_config(value, pool, expose_secrets));
            }
            Json::Object(out)
        }
        other => other.clone(),
    }
}

/// Every selector a config references (placeholders and selector keys), for
/// build-time reference validation.
pub fn collect_selectors(config: &Json, out: &mut Vec<Selector>) {
    match config {
        Json::String(s) if s.contains("{{#") => out.extend(Template::parse(s).selectors().cloned()),
        Json::Array(items) => items.iter().for_each(|v| collect_selectors(v, out)),
        Json::Object(map) => {
            for (key, value) in map {
                match parse_selector(value).filter(|_| is_selector_key(key)) {
                    Some(selector) => out.push(selector),
                    None => collect_selectors(value, out),
                }
            }
        }
        _ => {}
    }
}

fn resolve_string(s: &str, pool: &VariablePool, expose_secrets: bool) -> Json {
    if !s.contains("{{#") {
        return Json::String(s.to_owned());
    }
    let template = Template::parse(s);
    if template.is_static() {
        return Json::String(s.to_owned());
    }
    if let [graphrt_core::template::Part::Var(selector)] = template.parts() {
        return pool.get(selector).map(|v| to_json(&v, expose_secrets)).unwrap_or(Json::Null);
    }
    let text = if expose_secrets { template.render_exposed(pool) } else { template.render(pool) };
    Json::String(text)
}

fn to_json(value: &Value, expose_secrets: bool) -> Json {
    if expose_secrets { value.to_json_exposed() } else { value.to_json() }
}

fn is_selector_key(key: &str) -> bool {
    key.ends_with("selector")
}

fn parse_selector(value: &Json) -> Option<Selector> {
    match value {
        Json::Array(parts) => {
            let parts: Option<Vec<&str>> = parts.iter().map(Json::as_str).collect();
            Selector::new(parts?).ok()
        }
        Json::String(s) if s.contains('.') && !s.contains("{{") => Selector::parse(s).ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn pool() -> VariablePool {
        let mut p = VariablePool::new();
        p.insert("start", "name", Value::from("Ada"));
        p.insert("start", "n", Value::Int(3));
        p.insert("env", "KEY", Value::Secret(Arc::from("sk-1")));
        p
    }

    #[test]
    fn resolves_placeholders_and_selectors() {
        let cfg = serde_json::json!({
            "text": "Hi {{#start.name#}}",
            "count": "{{#start.n#}}",
            "input_selector": ["start", "name"],
            "nested": [{"value_selector": "start.n"}],
            "missing": "{{#start.nope#}}",
            "plain": "no placeholders",
        });
        let out = resolve_config(&cfg, &pool(), false);
        assert_eq!(out["text"], "Hi Ada");
        assert_eq!(out["count"], 3);
        assert_eq!(out["input_selector_value"], "Ada");
        assert_eq!(out["nested"][0]["value_selector_value"], 3);
        assert_eq!(out["missing"], Json::Null);
        assert_eq!(out["plain"], "no placeholders");
    }

    #[test]
    fn secrets_masked_unless_exposed() {
        let cfg = serde_json::json!({"key": "{{#env.KEY#}}", "auth": "Bearer {{#env.KEY#}}"});
        let masked = resolve_config(&cfg, &pool(), false);
        assert_ne!(masked["key"], "sk-1");
        assert!(!masked["auth"].as_str().unwrap_or_default().contains("sk-1"));
        let exposed = resolve_config(&cfg, &pool(), true);
        assert_eq!(exposed["key"], "sk-1");
        assert_eq!(exposed["auth"], "Bearer sk-1");
    }

    #[test]
    fn collects_references() {
        let cfg =
            serde_json::json!({"a": "{{#x.y#}} {{#z.w#}}", "b_selector": ["q", "r"], "c": [{"value_selector": "m.n"}]});
        let mut out = Vec::new();
        collect_selectors(&cfg, &mut out);
        let names: Vec<String> = out.iter().map(ToString::to_string).collect();
        assert_eq!(names, ["x.y", "z.w", "q.r", "m.n"]);
    }
}
