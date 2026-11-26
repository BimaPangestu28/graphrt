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
            let Some((selector, end)) = placeholder_at(body_start) else {
                // Not a placeholder: keep the opener as text and scan on after it.
                text.push_str("{{#");
                rest = body_start;
                continue;
            };
            if !text.is_empty() {
                parts.push(Part::Text(std::mem::take(&mut text)));
            }
            parts.push(Part::Var(selector));
            rest = &body_start[end + 3..];
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

/// Longest placeholder body that can be a selector.
const MAX_PLACEHOLDER_BYTES: usize = 512;

/// Offset of the `#}}` closing a placeholder body, looked for only as far as
/// the longest possible body. Searching the rest of the text made a source
/// with many `{{#` and one late `#}}` quadratic to parse.
fn find_placeholder_end(body_start: &str) -> Option<usize> {
    let window = body_start.as_bytes();
    let window = window.get(..window.len().min(MAX_PLACEHOLDER_BYTES + 3)).unwrap_or(window);
    // `#}}` is ASCII, so its offset is a char boundary.
    window.windows(3).position(|w| w == b"#}}")
}

/// The selector of a well-formed placeholder whose body starts at `body_start`
/// (just after `{{#`), with the offset of its closing `#}}`.
fn placeholder_at(body_start: &str) -> Option<(Selector, usize)> {
    let end = find_placeholder_end(body_start)?;
    let selector = parse_placeholder(body_start.get(..end)?)?;
    Some((selector, end))
}

fn parse_placeholder(body: &str) -> Option<Selector> {
    if body.is_empty() || body.len() > MAX_PLACEHOLDER_BYTES {
        return None;
    }
    let valid =
        body.split('.').all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-'));
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

    /// `repeats` unclosed `{{#` followed by one placeholder.
    fn unclosed_source(repeats: usize) -> String {
        format!("{}{{{{#sys.query#}}}}", "{{#".repeat(repeats))
    }

    /// Fastest of three parses of `source`. A busy machine only ever adds time,
    /// so the fastest run is the one closest to the parser's own cost.
    fn fastest_parse(source: &str) -> std::time::Duration {
        (0..3)
            .map(|_| {
                let started = std::time::Instant::now();
                std::hint::black_box(Template::parse(std::hint::black_box(source)));
                started.elapsed()
            })
            .min()
            .unwrap_or_default()
    }

    #[test]
    fn unclosed_placeholders_parse_in_linear_time() {
        // Each `{{#` used to scan to the one `#}}` at the very end: quadratic.
        // Compared with itself rather than with a fixed time, so neither a debug
        // build nor a loaded machine makes it fail: 8 times the input takes about
        // 8 times as long when linear and about 64 times as long when quadratic.
        let (small, large) = (5_000, 40_000);
        let small_time = fastest_parse(&unclosed_source(small));
        let large_time = fastest_parse(&unclosed_source(large));
        assert!(
            large_time < small_time * 24,
            "{large} took {large_time:?}, {small} took {small_time:?}: more than linear"
        );
        let template = Template::parse(&unclosed_source(small));
        assert_eq!(template.render(&pool()), format!("{}hi", "{{#".repeat(small)));
        // A closing marker beyond the longest possible body stays literal text.
        let far = format!("{{{{#{}#}}}}", "a".repeat(600));
        assert!(Template::parse(&far).is_static());
        let edge = format!("{{{{#sys.{}#}}}}", "q".repeat(MAX_PLACEHOLDER_BYTES - 4));
        assert!(!Template::parse(&edge).is_static());
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
