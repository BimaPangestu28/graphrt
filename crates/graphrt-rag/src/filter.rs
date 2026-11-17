//! Metadata filtering for retrieval.

use serde::{Deserialize, Serialize};

use crate::types::Metadata;

/// How conditions combine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Logical {
    #[default]
    And,
    Or,
}

/// One metadata condition, e.g. `{name: "lang", comparison_operator: "is", value: "en"}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetadataCondition {
    /// Metadata key; dotted paths descend into nested objects.
    pub name: String,
    #[serde(alias = "operator")]
    pub comparison_operator: String,
    #[serde(default)]
    pub value: serde_json::Value,
}

/// A set of metadata conditions.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MetadataFilter {
    #[serde(default)]
    pub logical_operator: Logical,
    #[serde(default)]
    pub conditions: Vec<MetadataCondition>,
}

/// Error for an unsupported comparison operator.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unsupported metadata operator `{0}`")]
pub struct UnknownOperator(pub String);

impl MetadataFilter {
    pub fn is_empty(&self) -> bool {
        self.conditions.is_empty()
    }

    /// Reject unknown operators up front (at node build / request time).
    pub fn validate(&self) -> Result<(), UnknownOperator> {
        for c in &self.conditions {
            if Op::parse(&c.comparison_operator).is_none() {
                return Err(UnknownOperator(c.comparison_operator.clone()));
            }
        }
        Ok(())
    }

    /// Whether `metadata` satisfies the filter. An empty filter matches everything.
    pub fn matches(&self, metadata: &Metadata) -> bool {
        if self.conditions.is_empty() {
            return true;
        }
        let mut results = self.conditions.iter().map(|c| condition_matches(c, metadata));
        match self.logical_operator {
            Logical::And => results.all(|r| r),
            Logical::Or => results.any(|r| r),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Is,
    IsNot,
    Contains,
    NotContains,
    StartsWith,
    EndsWith,
    Empty,
    NotEmpty,
    Gt,
    Lt,
    Ge,
    Le,
    In,
    NotIn,
}

impl Op {
    fn parse(s: &str) -> Option<Op> {
        Some(match s.trim() {
            "is" | "=" | "==" | "eq" => Op::Is,
            "is not" | "≠" | "!=" | "ne" => Op::IsNot,
            "contains" => Op::Contains,
            "not contains" => Op::NotContains,
            "start with" | "starts with" => Op::StartsWith,
            "end with" | "ends with" => Op::EndsWith,
            "empty" => Op::Empty,
            "not empty" => Op::NotEmpty,
            ">" | "gt" | "after" => Op::Gt,
            "<" | "lt" | "before" => Op::Lt,
            "≥" | ">=" | "ge" => Op::Ge,
            "≤" | "<=" | "le" => Op::Le,
            "in" => Op::In,
            "not in" => Op::NotIn,
            _ => return None,
        })
    }
}

fn lookup<'a>(metadata: &'a Metadata, path: &str) -> Option<&'a serde_json::Value> {
    let mut parts = path.split('.');
    let first = parts.next()?;
    let mut current = metadata.get(first)?;
    for part in parts {
        current = current.as_object()?.get(part)?;
    }
    Some(current)
}

fn as_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn as_number(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn is_empty(v: Option<&serde_json::Value>) -> bool {
    match v {
        None | Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::String(s)) => s.is_empty(),
        Some(serde_json::Value::Array(a)) => a.is_empty(),
        Some(serde_json::Value::Object(o)) => o.is_empty(),
        Some(_) => false,
    }
}

fn equals(actual: &serde_json::Value, expected: &serde_json::Value) -> bool {
    if let (Some(a), Some(b)) = (as_number(actual), as_number(expected)) {
        if matches!(actual, serde_json::Value::Number(_)) || matches!(expected, serde_json::Value::Number(_)) {
            return a == b;
        }
    }
    as_text(actual) == as_text(expected)
}

fn contains(actual: &serde_json::Value, expected: &serde_json::Value) -> bool {
    match actual {
        serde_json::Value::Array(items) => items.iter().any(|i| equals(i, expected)),
        other => as_text(other).contains(&as_text(expected)),
    }
}

fn member_of(actual: &serde_json::Value, expected: &serde_json::Value) -> bool {
    match expected {
        serde_json::Value::Array(options) => options.iter().any(|o| equals(actual, o)),
        serde_json::Value::String(s) => s.split(',').map(str::trim).any(|o| as_text(actual) == o),
        other => equals(actual, other),
    }
}

fn condition_matches(c: &MetadataCondition, metadata: &Metadata) -> bool {
    let Some(op) = Op::parse(&c.comparison_operator) else {
        return false;
    };
    let actual = lookup(metadata, &c.name);
    match op {
        Op::Empty => return is_empty(actual),
        Op::NotEmpty => return !is_empty(actual),
        _ => {}
    }
    let Some(actual) = actual else {
        // A missing key only satisfies the negative operators.
        return matches!(op, Op::IsNot | Op::NotContains | Op::NotIn);
    };
    let expected = &c.value;
    match op {
        Op::Is => equals(actual, expected),
        Op::IsNot => !equals(actual, expected),
        Op::Contains => contains(actual, expected),
        Op::NotContains => !contains(actual, expected),
        Op::StartsWith => as_text(actual).starts_with(&as_text(expected)),
        Op::EndsWith => as_text(actual).ends_with(&as_text(expected)),
        Op::In => member_of(actual, expected),
        Op::NotIn => !member_of(actual, expected),
        Op::Gt | Op::Lt | Op::Ge | Op::Le => {
            let ordering = match (as_number(actual), as_number(expected)) {
                (Some(a), Some(b)) => a.partial_cmp(&b),
                // Fall back to lexical comparison (works for ISO dates).
                _ => Some(as_text(actual).cmp(&as_text(expected))),
            };
            match (op, ordering) {
                (Op::Gt, Some(o)) => o.is_gt(),
                (Op::Lt, Some(o)) => o.is_lt(),
                (Op::Ge, Some(o)) => o.is_ge(),
                (Op::Le, Some(o)) => o.is_le(),
                _ => false,
            }
        }
        Op::Empty | Op::NotEmpty => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn meta(v: serde_json::Value) -> Metadata {
        v.as_object().cloned().unwrap_or_default()
    }

    fn filter(logical: Logical, conds: &[(&str, &str, serde_json::Value)]) -> MetadataFilter {
        MetadataFilter {
            logical_operator: logical,
            conditions: conds
                .iter()
                .map(|(n, o, v)| MetadataCondition {
                    name: (*n).into(),
                    comparison_operator: (*o).into(),
                    value: v.clone(),
                })
                .collect(),
        }
    }

    #[test]
    fn operators() {
        let m = meta(json!({"lang": "en", "year": 2024, "tags": ["a", "b"], "doc": {"kind": "faq"}}));
        assert!(filter(Logical::And, &[("lang", "is", json!("en"))]).matches(&m));
        assert!(filter(Logical::And, &[("year", ">", json!("2023"))]).matches(&m));
        assert!(filter(Logical::And, &[("year", "≤", json!(2024))]).matches(&m));
        assert!(filter(Logical::And, &[("tags", "contains", json!("b"))]).matches(&m));
        assert!(filter(Logical::And, &[("doc.kind", "start with", json!("fa"))]).matches(&m));
        assert!(filter(Logical::And, &[("lang", "in", json!(["de", "en"]))]).matches(&m));
        assert!(filter(Logical::And, &[("missing", "empty", json!(null))]).matches(&m));
        assert!(filter(Logical::And, &[("missing", "is not", json!("x"))]).matches(&m));
        assert!(!filter(Logical::And, &[("missing", "is", json!("x"))]).matches(&m));
        assert!(!filter(Logical::And, &[("lang", "is", json!("en")), ("year", "<", json!(2000))]).matches(&m));
        assert!(filter(Logical::Or, &[("lang", "is", json!("en")), ("year", "<", json!(2000))]).matches(&m));
    }

    #[test]
    fn validate_rejects_unknown() {
        assert!(filter(Logical::And, &[("a", "resembles", json!(1))]).validate().is_err());
        assert!(filter(Logical::And, &[("a", "contains", json!(1))]).validate().is_ok());
    }
}
