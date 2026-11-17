//! Variable templates: `Hello {{#start.name#}}, you said {{#sys.query#}}`.
//!
//! Templates are parsed once into [`Template`] parts at load time. Rendering
//! never re-parses resolved values, so a variable whose content looks like a
//! placeholder cannot inject further lookups.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::pool::{Selector, VariablePool};
use crate::value::Value;

/// One piece of a parsed template.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part {
    Text(String),
    Var(Selector),
}

/// A parsed `{{#ns.name.path#}}` template.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Template {
    source: String,
    parts: Vec<Part>,
}

impl Template {
    /// Parse a template. Anything that is not a well-formed placeholder stays literal text.
    pub fn parse(source: &str) -> Self {
        let mut parts = Vec::new();
        let mut text = String::new();
        let mut rest = source;
        while let Some(start) = rest.find("{{#") {
            let (before, after_open) = rest.split_at(start);
            text.push_str(before);
            let body_start = &after_open[3..];
            match body_start.find("#}}") {
                Some(end) => {
                    let body = &body_start[..end];
                    match parse_placeholder(body) {
                        Some(selector) => {
                            if !text.is_empty() {
                                parts.push(Part::Text(std::mem::take(&mut text)));
                            }
                            parts.push(Part::Var(selector));
                            rest = &body_start[end + 3..];
                        }
                        None => {
                            text.push_str("{{#");
                            rest = body_start;
                        }
                    }
                }
                None => {
                    text.push_str(after_open);
                    rest = "";
                }
            }
        }
        text.push_str(rest);
        if !text.is_empty() {
            parts.push(Part::Text(text));
        }
        Self { source: source.to_owned(), parts }
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn parts(&self) -> &[Part] {
        &self.parts
    }

    /// Selectors referenced by this template, in order of appearance.
    pub fn selectors(&self) -> impl Iterator<Item = &Selector> {
        self.parts.iter().filter_map(|p| match p {
            Part::Var(s) => Some(s),
            Part::Text(_) => None,
        })
    }

    /// Whether the template has no placeholders.
    pub fn is_static(&self) -> bool {
        self.selectors().next().is_none()
    }

    /// Render to text. Missing variables render as empty text; secrets are masked.
    pub fn render(&self, pool: &VariablePool) -> String {
        self.render_with(pool, Value::to_text)
    }

    /// Render revealing secrets. Only for nodes that must transmit plaintext
    /// (e.g. HTTP auth headers).
    pub fn render_exposed(&self, pool: &VariablePool) -> String {
        self.render_with(pool, Value::to_text_exposed)
    }

    fn render_with(&self, pool: &VariablePool, f: fn(&Value) -> String) -> String {
        let mut out = String::with_capacity(self.source.len());
        for part in &self.parts {
            match part {
                Part::Text(t) => out.push_str(t),
                Part::Var(sel) => {
                    if let Some(v) = pool.get(sel) {
                        out.push_str(&f(&v));
                    }
                }
            }
        }
        out
    }

    /// If the template is exactly one placeholder, resolve it to the typed value
    /// (so `"{{#llm.json#}}"` yields an object rather than its text). Otherwise render text.
    pub fn resolve(&self, pool: &VariablePool) -> Value {
        match self.parts.as_slice() {
            [Part::Var(sel)] => pool.get_or_null(sel),
            _ => Value::from(self.render(pool)),
        }
    }
}

fn parse_placeholder(body: &str) -> Option<Selector> {
    if body.is_empty() || body.len() > 512 {
        return None;
    }
    let valid = body
        .split('.')
        .all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-'));
    if !valid {
        return None;
    }
    Selector::parse(body).ok()
}

impl fmt::Debug for Template {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Template({:?})", self.source)
    }
}

impl From<&str> for Template {
    fn from(s: &str) -> Self {
        Template::parse(s)
    }
}

impl Serialize for Template {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.source)
    }
}

impl<'de> Deserialize<'de> for Template {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Template::parse(&s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> VariablePool {
        let mut p = VariablePool::new();
        p.insert("sys", "query", Value::from("hi"));
        p.insert("n", "obj", Value::from_json(serde_json::json!({"list": ["a", "b"]})));
        p.insert("n", "evil", Value::from("{{#sys.query#}}"));
        p
    }

    #[test]
    fn renders_placeholders() {
        let t = Template::parse("say {{#sys.query#}}! {{#n.obj.list.1#}}");
        assert_eq!(t.render(&pool()), "say hi! b");
    }

    #[test]
    fn missing_renders_empty_and_bare_text_is_literal() {
        let t = Template::parse("sys.query {{#sys.nope#}}|");
        assert_eq!(t.render(&pool()), "sys.query |");
    }

    #[test]
    fn no_reinjection() {
        let t = Template::parse("{{#n.evil#}}");
        assert_eq!(t.render(&pool()), "{{#sys.query#}}");
    }

    #[test]
    fn malformed_placeholders_stay_literal() {
        let t = Template::parse("{{#bad#}} {{#a b.c#}} {{#unterminated");
        assert!(t.is_static());
        assert_eq!(t.render(&pool()), "{{#bad#}} {{#a b.c#}} {{#unterminated");
    }

    #[test]
    fn single_placeholder_resolves_typed() {
        let t = Template::parse("{{#n.obj#}}");
        assert!(matches!(t.resolve(&pool()), Value::Object(_)));
    }
}
