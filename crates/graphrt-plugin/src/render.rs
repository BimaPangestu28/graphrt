//! Resolving variables in plugin node configuration.
//!
//! Before a `node.run` request, the host rewrites the node's config:
//!
//! - strings containing `{{#ns.name#}}` placeholders are rendered (a string that is
//!   exactly one placeholder becomes the typed value), and the original template
//!   is kept under `<key>_raw`;
//! - keys ending in `selector` holding a selector get a sibling `<key>_value` with
//!   the selected value (`null` when missing).
//!
//! The same walk collects every referenced selector so the graph can validate
//! references at load time.

use graphrt_core::pool::{Selector, VariablePool};
use graphrt_core::template::Template;
use graphrt_core::value::Value;

/// Render `config` against `pool`. Secrets are masked unless `expose_secrets`.
pub fn render_config(
    config: &serde_json::Map<String, serde_json::Value>,
    pool: &VariablePool,
    expose_secrets: bool,
) -> serde_json::Value {
    serde_json::Value::Object(render_object(config, pool, expose_secrets))
}

/// Every selector referenced by `config`.
pub fn collect_selectors(config: &serde_json::Map<String, serde_json::Value>) -> Vec<Selector> {
    let mut out = Vec::new();
    collect_object(config, &mut out);
    out
}

fn to_json(value: &Value, expose_secrets: bool) -> serde_json::Value {
    if expose_secrets { value.to_json_exposed() } else { value.to_json() }
}

fn render_object(
    map: &serde_json::Map<String, serde_json::Value>,
    pool: &VariablePool,
    expose: bool,
) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::with_capacity(map.len());
    for (key, value) in map {
        out.insert(key.clone(), render_value(value, pool, expose));
    }
    // Siblings are added after the walk so they never overwrite user keys.
    for (key, value) in map {
        if let serde_json::Value::String(s) = value
            && has_placeholder(s)
        {
            let raw_key = format!("{key}_raw");
            if !out.contains_key(&raw_key) {
                out.insert(raw_key, value.clone());
            }
        }
        if key.ends_with("selector")
            && let Some(selector) = as_selector(value)
        {
            let value_key = format!("{key}_value");
            if !out.contains_key(&value_key) {
                let resolved = pool.get(&selector).map(|v| to_json(&v, expose)).unwrap_or(serde_json::Value::Null);
                out.insert(value_key, resolved);
            }
        }
    }
    out
}

fn render_value(value: &serde_json::Value, pool: &VariablePool, expose: bool) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) if has_placeholder(s) => {
            let template = Template::parse(s);
            if template.is_static() {
                return value.clone();
            }
            match template.parts() {
                [graphrt_core::template::Part::Var(_)] => to_json(&template.resolve(pool), expose),
                _ => {
                    let text = if expose { template.render_exposed(pool) } else { template.render(pool) };
                    serde_json::Value::String(text)
                }
            }
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(render_object(map, pool, expose)),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(|v| render_value(v, pool, expose)).collect())
        }
        other => other.clone(),
    }
}

fn collect_object(map: &serde_json::Map<String, serde_json::Value>, out: &mut Vec<Selector>) {
    for (key, value) in map {
        if key.ends_with("selector")
            && let Some(selector) = as_selector(value)
        {
            out.push(selector);
            continue;
        }
        collect_value(value, out);
    }
}

fn collect_value(value: &serde_json::Value, out: &mut Vec<Selector>) {
    match value {
        serde_json::Value::String(s) if has_placeholder(s) => {
            out.extend(Template::parse(s).selectors().cloned());
        }
        serde_json::Value::Object(map) => collect_object(map, out),
        serde_json::Value::Array(items) => items.iter().for_each(|v| collect_value(v, out)),
        _ => {}
    }
}

fn has_placeholder(s: &str) -> bool {
    s.contains("{{#")
}

/// `["node","var",...]` or `"node.var"` (no whitespace).
fn as_selector(value: &serde_json::Value) -> Option<Selector> {
    match value {
        serde_json::Value::Array(items) => {
            let parts: Option<Vec<&str>> = items.iter().map(serde_json::Value::as_str).collect();
            parts.filter(|p| p.len() >= 2).and_then(|p| Selector::new(p).ok())
        }
        serde_json::Value::String(s) if !s.chars().any(char::is_whitespace) && s.contains('.') => {
            Selector::parse(s).ok()
        }
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
        p.insert("env", "KEY", Value::Secret(Arc::from("s3cret")));
        p
    }

    fn cfg(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn renders_templates_and_selectors() {
        let c = cfg(serde_json::json!({
            "greeting": "hi {{#start.name#}}",
            "count": "{{#start.n#}}",
            "plain": "no vars",
            "value_selector": ["start", "name"],
            "nested": [{"query_selector": "start.n"}],
            "missing_selector": ["start", "nope"]
        }));
        let out = render_config(&c, &pool(), false);
        assert_eq!(out["greeting"], "hi Ada");
        assert_eq!(out["greeting_raw"], "hi {{#start.name#}}");
        assert_eq!(out["count"], 3);
        assert_eq!(out["plain"], "no vars");
        assert!(out.get("plain_raw").is_none());
        assert_eq!(out["value_selector_value"], "Ada");
        assert_eq!(out["nested"][0]["query_selector_value"], 3);
        assert_eq!(out["missing_selector_value"], serde_json::Value::Null);
    }

    #[test]
    fn masks_secrets_unless_exposed() {
        let c = cfg(serde_json::json!({"key": "{{#env.KEY#}}", "auth": "Bearer {{#env.KEY#}}"}));
        let masked = render_config(&c, &pool(), false);
        assert_eq!(masked["key"], "******");
        assert!(!masked.to_string().contains("s3cret"));
        let exposed = render_config(&c, &pool(), true);
        assert_eq!(exposed["auth"], "Bearer s3cret");
    }

    #[test]
    fn collects_references() {
        let c = cfg(serde_json::json!({
            "a": "{{#start.name#}} and {{#sys.query#}}",
            "variable_selector": ["start", "n"],
            "list": [{"x": "{{#llm.text#}}"}],
            "title": "not.a.selector"
        }));
        let sels: Vec<String> = collect_selectors(&c).iter().map(ToString::to_string).collect();
        assert_eq!(sels, ["start.name", "sys.query", "start.n", "llm.text"]);
    }

    #[test]
    fn user_keys_win_over_siblings() {
        let c = cfg(serde_json::json!({"value_selector": ["start", "name"], "value_selector_value": "mine"}));
        assert_eq!(render_config(&c, &pool(), false)["value_selector_value"], "mine");
    }
}
