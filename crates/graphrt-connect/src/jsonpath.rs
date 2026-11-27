//! A small JSONPath subset for API sources: `$`, `.name`, `['name']`, `[0]`,
//! `[-1]`, `[*]` / `.*` and recursive `..name`. Enough to point at the list of
//! records in a response and at the fields to index, without a query engine.

use serde_json::Value;

use crate::error::{ConnectError, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Step {
    Key(String),
    Index(i64),
    Wildcard,
    Descend(String),
}

/// A parsed path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonPath {
    text: String,
    steps: Vec<Step>,
}

impl JsonPath {
    /// Parse `$.data.items[*]`, `data.items` (the `$` is optional) or `$`.
    ///
    /// @throws `ConnectError::Config` naming the problem.
    pub fn parse(text: &str) -> Result<Self> {
        let bad = |why: &str| ConnectError::Config(format!("JSONPath `{text}`: {why}"));
        let trimmed = text.trim();
        let mut rest = trimmed.strip_prefix('$').unwrap_or(trimmed);
        let mut steps = Vec::new();
        if !rest.is_empty() && !rest.starts_with(['.', '[']) {
            // `data.items` without the leading `$.`
            let end = rest.find(['.', '[']).unwrap_or(rest.len());
            let (key, tail) = rest.split_at(end);
            steps.push(Step::Key(key.to_owned()));
            rest = tail;
        }
        while !rest.is_empty() {
            if let Some(tail) = rest.strip_prefix("..") {
                let end = tail.find(['.', '[']).unwrap_or(tail.len());
                let (key, tail) = tail.split_at(end);
                if key.is_empty() {
                    return Err(bad("`..` needs a field name"));
                }
                steps.push(Step::Descend(key.to_owned()));
                rest = tail;
            } else if let Some(tail) = rest.strip_prefix('.') {
                let end = tail.find(['.', '[']).unwrap_or(tail.len());
                let (key, tail) = tail.split_at(end);
                match key {
                    "" => return Err(bad("empty field name")),
                    "*" => steps.push(Step::Wildcard),
                    _ => steps.push(Step::Key(key.to_owned())),
                }
                rest = tail;
            } else if let Some(tail) = rest.strip_prefix('[') {
                let end = tail.find(']').ok_or_else(|| bad("missing `]`"))?;
                let (inner, tail) = tail.split_at(end);
                let inner = inner.trim();
                rest = tail.get(1..).unwrap_or_default();
                if inner == "*" {
                    steps.push(Step::Wildcard);
                } else if let Some(quoted) = inner
                    .strip_prefix('\'')
                    .and_then(|s| s.strip_suffix('\''))
                    .or_else(|| inner.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
                {
                    steps.push(Step::Key(quoted.to_owned()));
                } else {
                    let index =
                        inner.parse::<i64>().map_err(|_| bad("brackets take `*`, a number or a quoted name"))?;
                    steps.push(Step::Index(index));
                }
            } else {
                return Err(bad("expected `.` or `[`"));
            }
        }
        Ok(Self { text: trimmed.to_owned(), steps })
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Every value the path selects, in document order.
    pub fn select<'a>(&self, root: &'a Value) -> Vec<&'a Value> {
        let mut current = vec![root];
        for step in &self.steps {
            let mut next = Vec::new();
            for value in current {
                match step {
                    Step::Key(key) => next.extend(value.get(key)),
                    Step::Index(index) => {
                        if let Some(items) = value.as_array() {
                            let resolved = if *index < 0 {
                                i64::try_from(items.len()).ok().map(|len| len + index)
                            } else {
                                Some(*index)
                            };
                            next.extend(resolved.and_then(|i| usize::try_from(i).ok()).and_then(|i| items.get(i)));
                        }
                    }
                    Step::Wildcard => match value {
                        Value::Array(items) => next.extend(items.iter()),
                        Value::Object(map) => next.extend(map.values()),
                        _ => {}
                    },
                    Step::Descend(key) => descend(value, key, &mut next),
                }
            }
            current = next;
        }
        current
    }

    /// The first selected value.
    pub fn first<'a>(&self, root: &'a Value) -> Option<&'a Value> {
        self.select(root).into_iter().next()
    }
}

fn descend<'a>(value: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match value {
        Value::Object(map) => {
            if let Some(found) = map.get(key) {
                out.push(found);
            }
            for child in map.values() {
                descend(child, key, out);
            }
        }
        Value::Array(items) => {
            for child in items {
                descend(child, key, out);
            }
        }
        _ => {}
    }
}

/// A value as text to index: strings as they are, numbers and booleans
/// printed, arrays joined by new lines, objects as compact JSON.
pub fn value_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Array(items) => items.iter().map(value_text).filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n"),
        Value::Object(_) => value.to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn selects() {
        let doc = json!({"data": {"items": [{"id": 1, "t": "a"}, {"id": 2, "t": "b"}]}, "next": null});
        let ids: Vec<_> = JsonPath::parse("$.data.items[*].id").unwrap().select(&doc).into_iter().cloned().collect();
        assert_eq!(ids, vec![json!(1), json!(2)]);
        assert_eq!(JsonPath::parse("data.items[-1].t").unwrap().first(&doc), Some(&json!("b")));
        assert_eq!(JsonPath::parse("$['data']['items'][0]['t']").unwrap().first(&doc), Some(&json!("a")));
        assert_eq!(JsonPath::parse("$..t").unwrap().select(&doc).len(), 2);
        assert_eq!(JsonPath::parse("$").unwrap().select(&doc), vec![&doc]);
        assert!(JsonPath::parse("$.a[").is_err());
        assert!(JsonPath::parse("$.a[x]").is_err());
    }

    #[test]
    fn texts() {
        assert_eq!(value_text(&json!(["a", null, 2])), "a\n2");
        assert_eq!(value_text(&json!(true)), "true");
    }
}
