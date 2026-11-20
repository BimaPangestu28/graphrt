//! Dotted JSON paths (`output.items[0].name`) and `{{ var }}` substitution in
//! assertion values. No JavaScript: a path is identifiers, `.` and `[index]`.

use serde_json::Value as Json;

use crate::error::EvalError;

/// One step of a path.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Step {
    Key(String),
    Index(usize),
}

/// A parsed path such as `output.items[0].name` or `outputs["odd key"]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonPath {
    text: String,
    steps: Vec<Step>,
}

impl JsonPath {
    /// Parse a path. An optional `json.` or `$.` prefix is accepted for
    /// compatibility with Promptfoo `transform` expressions.
    pub fn parse(text: &str) -> Result<Self, EvalError> {
        let bad = |why: &str| {
            EvalError::invalid(format!(
                "`{text}` is not a supported path ({why}); use identifiers, `.` and `[n]`, e.g. `output.items[0].name` \
                 (JavaScript transforms are not supported)"
            ))
        };
        let trimmed = text.trim();
        let body = trimmed.strip_prefix("$.").unwrap_or(trimmed);
        let mut steps = Vec::new();
        let mut chars = body.chars().peekable();
        let mut key = String::new();
        let flush = |key: &mut String, steps: &mut Vec<Step>| {
            if !key.is_empty() {
                steps.push(Step::Key(std::mem::take(key)));
            }
        };
        while let Some(c) = chars.next() {
            match c {
                '.' => {
                    if key.is_empty() && !matches!(steps.last(), Some(Step::Index(_))) {
                        return Err(bad("empty segment"));
                    }
                    flush(&mut key, &mut steps);
                }
                '[' => {
                    flush(&mut key, &mut steps);
                    let mut inner = String::new();
                    let mut closed = false;
                    for c in chars.by_ref() {
                        if c == ']' {
                            closed = true;
                            break;
                        }
                        inner.push(c);
                    }
                    if !closed {
                        return Err(bad("unclosed `[`"));
                    }
                    let inner = inner.trim();
                    let quoted = inner
                        .strip_prefix('"')
                        .and_then(|s| s.strip_suffix('"'))
                        .or_else(|| inner.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')));
                    match (quoted, inner.parse::<usize>()) {
                        (Some(name), _) => steps.push(Step::Key(name.to_owned())),
                        (None, Ok(index)) => steps.push(Step::Index(index)),
                        (None, Err(_)) => return Err(bad("`[...]` needs an index or a quoted key")),
                    }
                }
                c if c.is_alphanumeric() || c == '_' || c == '-' => key.push(c),
                _ => return Err(bad(&format!("unexpected `{c}`"))),
            }
        }
        if body.ends_with('.') {
            return Err(bad("trailing `.`"));
        }
        flush(&mut key, &mut steps);
        if steps.is_empty() {
            return Err(bad("empty path"));
        }
        if matches!(steps.first(), Some(Step::Key(k)) if k == "json") && steps.len() > 1 {
            steps.remove(0);
        }
        Ok(Self { text: trimmed.to_owned(), steps })
    }

    /// The path as written.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Look the path up in `root`. A string holding JSON is parsed on the way,
    /// so `output.field` works when the output is a JSON text.
    pub fn get(&self, root: &Json) -> Option<Json> {
        let mut current = root.clone();
        for step in &self.steps {
            if let Json::String(text) = &current
                && let Ok(parsed) = serde_json::from_str::<Json>(text)
                && (parsed.is_object() || parsed.is_array())
            {
                current = parsed;
            }
            current = match (step, current) {
                (Step::Key(k), Json::Object(mut map)) => map.remove(k)?,
                (Step::Index(i), Json::Array(items)) => items.into_iter().nth(*i)?,
                _ => return None,
            };
        }
        Some(current)
    }
}

/// Replace `{{ path }}` references in every string of `value`, looking paths up
/// in `context`. A string that is exactly one reference becomes the referenced
/// value itself (so `value: "{{expected_docs}}"` can be a list); otherwise the
/// referenced values are inserted as text. Unknown references are errors.
pub fn render(value: &Json, context: &Json) -> Result<Json, EvalError> {
    Ok(match value {
        Json::String(text) => render_text(text, context)?,
        Json::Array(items) => Json::Array(items.iter().map(|v| render(v, context)).collect::<Result<_, _>>()?),
        Json::Object(map) => Json::Object(
            map.iter().map(|(k, v)| Ok((k.clone(), render(v, context)?))).collect::<Result<_, EvalError>>()?,
        ),
        other => other.clone(),
    })
}

fn render_text(text: &str, context: &Json) -> Result<Json, EvalError> {
    if !text.contains("{{") {
        return Ok(Json::String(text.to_owned()));
    }
    let lookup = |expr: &str| -> Result<Json, EvalError> {
        JsonPath::parse(expr)?.get(context).ok_or_else(|| {
            EvalError::invalid(format!("`{{{{{}}}}}` does not name a variable of this test", expr.trim()))
        })
    };
    let trimmed = text.trim();
    if let Some(inner) = trimmed.strip_prefix("{{").and_then(|s| s.strip_suffix("}}"))
        && !inner.contains("{{")
        && !inner.contains("}}")
    {
        return lookup(inner);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let (before, after) = rest.split_at(start);
        out.push_str(before);
        let after = after.get(2..).unwrap_or_default();
        let Some(end) = after.find("}}") else {
            return Err(EvalError::invalid(format!("unterminated `{{{{` in `{text}`")));
        };
        let value = lookup(after.get(..end).unwrap_or_default())?;
        match value {
            Json::String(s) => out.push_str(&s),
            other => out.push_str(&other.to_string()),
        }
        rest = after.get(end + 2..).unwrap_or_default();
    }
    out.push_str(rest);
    Ok(Json::String(out))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use serde_json::json;

    #[test]
    fn paths() {
        let root = json!({"output": {"items": [{"name": "a"}, {"name": "b"}], "odd key": 1}, "text": "{\"n\": 2}"});
        let get = |p: &str| JsonPath::parse(p).unwrap().get(&root);
        assert_eq!(get("output.items[1].name"), Some(json!("b")));
        assert_eq!(get("output[\"odd key\"]"), Some(json!(1)));
        assert_eq!(get("$.output.items[0]"), Some(json!({"name": "a"})));
        assert_eq!(get("text.n"), Some(json!(2)), "JSON strings are parsed");
        assert_eq!(get("output.missing"), None);
        for bad in ["output.map(x => x)", "a..b", "a[", "a[x]", "", "a."] {
            assert!(JsonPath::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn rendering() {
        let ctx = json!({"name": "Ada", "docs": ["d1", "d2"], "vars": {"n": 3}});
        assert_eq!(render(&json!("{{docs}}"), &ctx).unwrap(), json!(["d1", "d2"]));
        assert_eq!(render(&json!("Hello, {{ name }} ({{vars.n}})"), &ctx).unwrap(), json!("Hello, Ada (3)"));
        assert_eq!(render(&json!({"a": ["{{ name }}"]}), &ctx).unwrap(), json!({"a": ["Ada"]}));
        assert!(render(&json!("{{ nope }}"), &ctx).is_err());
        assert!(render(&json!("{{ name"), &ctx).is_err());
    }
}
