//! Condition evaluation shared by `if-else`, `loop` break conditions and
//! `list-operator` filters.
//!
//! Semantics:
//! - A missing variable is treated as [`Value::Null`]. `exists`, `null` and
//!   `empty` therefore work on absent variables, and nothing errors on null.
//! - Positive operators on `Null` are `false`; negated operators (`not contains`,
//!   `is not`, `≠`, `not in`, `not empty`...) are the exact negation of their
//!   positive counterpart.
//! - Numeric comparisons use `f64` without truncation (`3 = "3.9"` is false).
//! - Expected values that are strings may contain `{{#ns.name#}}` placeholders;
//!   they are resolved against the pool before comparison.
//! - For `array[file]` with a sub-condition group, the outer operator decides how
//!   per-file results combine: `contains` = any file matches, `not contains` =
//!   no file matches, `all of` = every file matches (and the list is non-empty).

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::pool::{Selector, VariablePool};
use crate::template::Template;
use crate::value::Value;

/// Comparison operator. Wire names follow the established workflow format; ASCII
/// aliases are accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Operator {
    #[serde(rename = "contains")]
    Contains,
    #[serde(rename = "not contains")]
    NotContains,
    #[serde(rename = "start with", alias = "starts with", alias = "start_with", alias = "starts_with")]
    StartWith,
    #[serde(rename = "end with", alias = "ends with", alias = "end_with", alias = "ends_with")]
    EndWith,
    #[serde(rename = "is")]
    Is,
    #[serde(rename = "is not", alias = "is_not")]
    IsNot,
    #[serde(rename = "empty")]
    Empty,
    #[serde(rename = "not empty", alias = "not_empty")]
    NotEmpty,
    #[serde(rename = "in")]
    In,
    #[serde(rename = "not in", alias = "not_in")]
    NotIn,
    #[serde(rename = "all of", alias = "all_of")]
    AllOf,
    #[serde(rename = "=", alias = "==", alias = "eq")]
    Eq,
    #[serde(rename = "≠", alias = "!=", alias = "ne")]
    Ne,
    #[serde(rename = ">", alias = "gt")]
    Gt,
    #[serde(rename = "<", alias = "lt")]
    Lt,
    #[serde(rename = "≥", alias = ">=", alias = "ge")]
    Ge,
    #[serde(rename = "≤", alias = "<=", alias = "le")]
    Le,
    #[serde(rename = "null", alias = "is null")]
    Null,
    #[serde(rename = "not null", alias = "is not null", alias = "not_null")]
    NotNull,
    #[serde(rename = "exists")]
    Exists,
    #[serde(rename = "not exists", alias = "not_exists")]
    NotExists,
}

impl Operator {
    /// Wire name of the operator.
    pub fn as_str(self) -> &'static str {
        match self {
            Operator::Contains => "contains",
            Operator::NotContains => "not contains",
            Operator::StartWith => "start with",
            Operator::EndWith => "end with",
            Operator::Is => "is",
            Operator::IsNot => "is not",
            Operator::Empty => "empty",
            Operator::NotEmpty => "not empty",
            Operator::In => "in",
            Operator::NotIn => "not in",
            Operator::AllOf => "all of",
            Operator::Eq => "=",
            Operator::Ne => "≠",
            Operator::Gt => ">",
            Operator::Lt => "<",
            Operator::Ge => "≥",
            Operator::Le => "≤",
            Operator::Null => "null",
            Operator::NotNull => "not null",
            Operator::Exists => "exists",
            Operator::NotExists => "not exists",
        }
    }

    /// Whether the operator ignores the expected value.
    pub fn is_unary(self) -> bool {
        matches!(
            self,
            Operator::Empty
                | Operator::NotEmpty
                | Operator::Null
                | Operator::NotNull
                | Operator::Exists
                | Operator::NotExists
        )
    }
}

impl fmt::Display for Operator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How conditions in a group combine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogicalOperator {
    #[default]
    And,
    Or,
}

/// Error evaluating a condition (operator not applicable to the value types).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("operator `{operator}` cannot be applied to {actual} (expected value: {expected})")]
pub struct ConditionError {
    pub operator: &'static str,
    pub actual: &'static str,
    pub expected: String,
}

/// A condition on one variable.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    pub variable_selector: Selector,
    pub comparison_operator: Operator,
    /// Expected value: literal JSON, or a string that may contain `{{#..#}}` placeholders.
    #[serde(default)]
    pub value: serde_json::Value,
    /// Per-file attribute conditions for file / `array[file]` variables.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_variable_condition: Option<SubConditionGroup>,
}

impl Condition {
    /// Every selector this condition reads: the variable plus any placeholders in
    /// the expected value(s), including file sub-conditions.
    pub fn selectors(&self) -> Vec<Selector> {
        let mut out = vec![self.variable_selector.clone()];
        expected_selectors(&self.value, &mut out);
        if let Some(group) = &self.sub_variable_condition {
            for c in &group.conditions {
                expected_selectors(&c.value, &mut out);
            }
        }
        out
    }
}

/// Selectors referenced by placeholders inside a configured expected value.
pub(crate) fn expected_selectors(raw: &serde_json::Value, out: &mut Vec<Selector>) {
    match raw {
        serde_json::Value::String(s) if s.contains("{{#") => out.extend(Template::parse(s).selectors().cloned()),
        serde_json::Value::Array(items) => {
            for item in items {
                expected_selectors(item, out);
            }
        }
        _ => {}
    }
}

/// Attribute conditions applied to each file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubConditionGroup {
    #[serde(default)]
    pub logical_operator: LogicalOperator,
    #[serde(default)]
    pub conditions: Vec<KeyCondition>,
}

/// Condition on an attribute (`key`) of an item: a file attribute, an object path
/// (dotted), or empty for the item itself.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeyCondition {
    #[serde(default)]
    pub key: String,
    pub comparison_operator: Operator,
    #[serde(default)]
    pub value: serde_json::Value,
}

/// Evaluation record of one condition, for observability.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ConditionTrace {
    pub selector: String,
    pub operator: Operator,
    pub actual: Value,
    pub expected: Value,
    pub result: bool,
}

impl ConditionTrace {
    /// Trace as a runtime value (for node metadata).
    pub fn to_value(&self) -> Value {
        Value::object([
            ("selector", Value::from(self.selector.as_str())),
            ("operator", Value::from(self.operator.as_str())),
            ("actual", self.actual.clone()),
            ("expected", self.expected.clone()),
            ("result", Value::Bool(self.result)),
        ])
    }
}

/// Resolve an expected value from configuration: strings are parsed as templates
/// (a lone placeholder keeps its typed value), arrays are resolved element-wise.
pub(crate) fn resolve_expected(raw: &serde_json::Value, pool: &VariablePool) -> Value {
    match raw {
        serde_json::Value::String(s) => {
            if s.contains("{{#") {
                Template::parse(s).resolve(pool)
            } else {
                Value::from(s.as_str())
            }
        }
        serde_json::Value::Array(items) => Value::array(items.iter().map(|i| resolve_expected(i, pool))),
        other => Value::from_json(other.clone()),
    }
}

/// Evaluate one condition against the pool.
pub fn evaluate(condition: &Condition, pool: &VariablePool) -> Result<bool, ConditionError> {
    evaluate_traced(condition, pool).map(|t| t.result)
}

/// Evaluate one condition and return its trace.
pub fn evaluate_traced(condition: &Condition, pool: &VariablePool) -> Result<ConditionTrace, ConditionError> {
    let actual = pool.get_or_null(&condition.variable_selector);
    let op = condition.comparison_operator;
    let (expected, result) = match &condition.sub_variable_condition {
        Some(group) if is_file_like(&actual) && !op.is_unary() => {
            (Value::Null, evaluate_file_group(op, &actual, group, pool)?)
        }
        _ => {
            let expected = if op.is_unary() { Value::Null } else { resolve_expected(&condition.value, pool) };
            let result = compare(op, &actual, &expected)?;
            (expected, result)
        }
    };
    Ok(ConditionTrace { selector: condition.variable_selector.to_string(), operator: op, actual, expected, result })
}

/// Evaluate a group of conditions with short-circuiting. An empty group is `false`
/// for `and`/`or` alike (no condition means "no match").
pub fn evaluate_group(
    conditions: &[Condition],
    logical: LogicalOperator,
    pool: &VariablePool,
) -> Result<(bool, Vec<ConditionTrace>), ConditionError> {
    if conditions.is_empty() {
        return Ok((false, Vec::new()));
    }
    let mut traces = Vec::with_capacity(conditions.len());
    for condition in conditions {
        let trace = evaluate_traced(condition, pool)?;
        let result = trace.result;
        traces.push(trace);
        match logical {
            LogicalOperator::And if !result => return Ok((false, traces)),
            LogicalOperator::Or if result => return Ok((true, traces)),
            _ => {}
        }
    }
    Ok((logical == LogicalOperator::And, traces))
}

fn is_file_like(v: &Value) -> bool {
    match v {
        Value::File(_) => true,
        Value::Array(items) => !items.is_empty() && items.iter().all(|i| matches!(i, Value::File(_))),
        _ => false,
    }
}

fn evaluate_file_group(
    op: Operator,
    actual: &Value,
    group: &SubConditionGroup,
    pool: &VariablePool,
) -> Result<bool, ConditionError> {
    let files: Vec<&Value> = match actual {
        Value::Array(items) => items.iter().collect(),
        single => vec![single],
    };
    let mut matches = Vec::with_capacity(files.len());
    for file in files {
        matches.push(key_group_matches(file, &group.conditions, group.logical_operator, pool)?);
    }
    Ok(match op {
        Operator::NotContains | Operator::NotIn | Operator::IsNot | Operator::Ne => !matches.iter().any(|m| *m),
        Operator::AllOf => !matches.is_empty() && matches.iter().all(|m| *m),
        _ => matches.iter().any(|m| *m),
    })
}

/// Evaluate key conditions against one item, combining with `logical`. Empty = `true`.
pub(crate) fn key_group_matches(
    item: &Value,
    conditions: &[KeyCondition],
    logical: LogicalOperator,
    pool: &VariablePool,
) -> Result<bool, ConditionError> {
    if conditions.is_empty() {
        return Ok(true);
    }
    for c in conditions {
        let r = key_condition_matches(item, c, pool)?;
        match logical {
            LogicalOperator::And if !r => return Ok(false),
            LogicalOperator::Or if r => return Ok(true),
            _ => {}
        }
    }
    Ok(logical == LogicalOperator::And)
}

/// Evaluate one key condition against an item.
pub(crate) fn key_condition_matches(
    item: &Value,
    c: &KeyCondition,
    pool: &VariablePool,
) -> Result<bool, ConditionError> {
    let actual = item_attribute(item, &c.key);
    let expected = if c.comparison_operator.is_unary() { Value::Null } else { resolve_expected(&c.value, pool) };
    compare(c.comparison_operator, &actual, &expected)
}

/// Look up `key` in an item: empty key = the item, otherwise a file attribute or
/// a dotted object path.
pub(crate) fn item_attribute(item: &Value, key: &str) -> Value {
    if key.is_empty() {
        return item.clone();
    }
    match item {
        Value::File(f) => f.attribute(key).unwrap_or(Value::Null),
        other => {
            let parts: Vec<&str> = key.split('.').collect();
            other.get_path(&parts).unwrap_or(Value::Null)
        }
    }
}

/// Apply an operator to an actual and an (already resolved) expected value.
pub(crate) fn compare(op: Operator, actual: &Value, expected: &Value) -> Result<bool, ConditionError> {
    let err = || ConditionError { operator: op.as_str(), actual: actual.type_name(), expected: expected.to_text() };
    Ok(match op {
        Operator::Null | Operator::NotExists => actual.is_null(),
        Operator::NotNull | Operator::Exists => !actual.is_null(),
        Operator::Empty => actual.is_empty(),
        Operator::NotEmpty => !actual.is_empty(),
        Operator::Contains => contains(actual, expected).ok_or_else(err)?,
        Operator::NotContains => !contains(actual, expected).ok_or_else(err)?,
        Operator::StartWith => match text_of(actual) {
            None if actual.is_null() => false,
            None => return Err(err()),
            Some(s) => s.starts_with(&expected.to_text_exposed()),
        },
        Operator::EndWith => match text_of(actual) {
            None if actual.is_null() => false,
            None => return Err(err()),
            Some(s) => s.ends_with(&expected.to_text_exposed()),
        },
        Operator::Is | Operator::Eq => !actual.is_null() && loose_eq(actual, expected),
        Operator::IsNot | Operator::Ne => actual.is_null() || !loose_eq(actual, expected),
        Operator::Gt | Operator::Lt | Operator::Ge | Operator::Le => {
            if actual.is_null() {
                return Ok(false);
            }
            let a = number_of(actual).ok_or_else(err)?;
            let Some(e) = number_of(expected) else {
                return Err(err());
            };
            match op {
                Operator::Gt => a > e,
                Operator::Lt => a < e,
                Operator::Ge => a >= e,
                _ => a <= e,
            }
        }
        Operator::In => is_in(actual, expected),
        Operator::NotIn => !is_in(actual, expected),
        Operator::AllOf => {
            if actual.is_null() {
                return Ok(false);
            }
            let wanted = as_list(expected);
            let mut all = true;
            for w in &wanted {
                if !contains(actual, w).ok_or_else(err)? {
                    all = false;
                    break;
                }
            }
            all
        }
    })
}

/// `Some(true/false)` when the containment check applies, `None` when the actual type
/// does not support containment. Null contains nothing.
fn contains(actual: &Value, expected: &Value) -> Option<bool> {
    match actual {
        Value::Null => Some(false),
        Value::String(s) | Value::Secret(s) => Some(s.contains(expected.to_text_exposed().as_str())),
        Value::Array(items) => Some(items.iter().any(|i| loose_eq(i, expected))),
        Value::Object(map) => Some(map.contains_key(expected.to_text_exposed().as_str())),
        _ => None,
    }
}

fn is_in(actual: &Value, expected: &Value) -> bool {
    if actual.is_null() {
        return false;
    }
    as_list(expected).iter().any(|e| loose_eq(actual, e))
}

/// Interpret an expected value as a list: arrays as-is, JSON-array strings parsed,
/// other strings split on commas, scalars as a single-element list.
fn as_list(expected: &Value) -> Vec<Value> {
    match expected {
        Value::Array(items) => items.iter().cloned().collect(),
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.starts_with('[')
                && let Ok(serde_json::Value::Array(items)) =
                    serde_json::from_slice::<serde_json::Value>(trimmed.as_bytes())
            {
                return items.into_iter().map(Value::from_json).collect();
            }
            trimmed.split(',').map(|p| Value::from(p.trim())).filter(|v| !v.is_empty()).collect()
        }
        Value::Null => Vec::new(),
        other => vec![other.clone()],
    }
}

fn text_of(v: &Value) -> Option<Arc<str>> {
    match v {
        Value::String(s) | Value::Secret(s) => Some(s.clone()),
        _ => None,
    }
}

/// Numeric view of a value: numbers, and strings that parse as numbers.
pub(crate) fn number_of(v: &Value) -> Option<f64> {
    match v {
        Value::Int(_) | Value::Float(_) => v.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn bool_of(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Some(true),
            "false" | "0" | "no" => Some(false),
            _ => None,
        },
        Value::Int(1) => Some(true),
        Value::Int(0) => Some(false),
        _ => None,
    }
}

/// Equality across representations: numbers numerically (strings parsed),
/// booleans against boolean-like strings, otherwise structural or textual equality.
pub(crate) fn loose_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Null, _) | (_, Value::Null) => false,
        (Value::Bool(x), other) | (other, Value::Bool(x)) => bool_of(other) == Some(*x),
        (Value::Int(_) | Value::Float(_), _) | (_, Value::Int(_) | Value::Float(_)) => {
            match (number_of(a), number_of(b)) {
                (Some(x), Some(y)) => x == y,
                _ => false,
            }
        }
        (Value::String(x) | Value::Secret(x), Value::String(y) | Value::Secret(y)) => x == y,
        (Value::Array(_) | Value::Object(_) | Value::File(_), _)
        | (_, Value::Array(_) | Value::Object(_) | Value::File(_)) => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pool() -> VariablePool {
        let mut p = VariablePool::new();
        p.insert("n", "s", Value::from("hello world"));
        p.insert("n", "i", Value::Int(3));
        p.insert("n", "f", Value::Float(2.5));
        p.insert("n", "b", Value::Bool(true));
        p.insert("n", "arr", Value::from_json(json!(["a", "b", 1])));
        p.insert("n", "empty", Value::from(""));
        p.insert("n", "ref", Value::from("world"));
        p
    }

    fn cond(sel: &str, op: &str, value: serde_json::Value) -> Condition {
        serde_json::from_value(json!({
            "variable_selector": sel.split('.').collect::<Vec<_>>(),
            "comparison_operator": op,
            "value": value,
        }))
        .unwrap()
    }

    fn eval(sel: &str, op: &str, value: serde_json::Value) -> bool {
        evaluate(&cond(sel, op, value), &pool()).unwrap()
    }

    #[test]
    fn string_operators() {
        assert!(eval("n.s", "contains", json!("lo w")));
        assert!(!eval("n.s", "not contains", json!("lo w")));
        assert!(eval("n.s", "start with", json!("hell")));
        assert!(eval("n.s", "starts with", json!("hell")));
        assert!(eval("n.s", "end with", json!("world")));
        assert!(eval("n.s", "is", json!("hello world")));
        assert!(eval("n.s", "is not", json!("x")));
        assert!(eval("n.empty", "empty", json!(null)));
        assert!(eval("n.s", "not empty", json!(null)));
    }

    #[test]
    fn template_in_expected_value() {
        assert!(eval("n.s", "end with", json!("{{#n.ref#}}")));
    }

    #[test]
    fn numeric_no_truncation() {
        assert!(!eval("n.i", "=", json!("3.9")));
        assert!(eval("n.i", "=", json!("3")));
        assert!(eval("n.i", "==", json!(3.0)));
        assert!(eval("n.f", ">", json!(2)));
        assert!(eval("n.f", "≤", json!("2.5")));
        assert!(eval("n.i", ">=", json!(3)));
        assert!(eval("n.i", "≠", json!(4)));
    }

    #[test]
    fn missing_variable_is_null() {
        assert!(eval("n.missing", "not exists", json!(null)));
        assert!(eval("n.missing", "null", json!(null)));
        assert!(eval("n.missing", "empty", json!(null)));
        assert!(!eval("n.missing", "exists", json!(null)));
        assert!(!eval("n.missing", ">", json!(1)));
        assert!(!eval("n.missing", "contains", json!("x")));
        assert!(eval("n.missing", "not contains", json!("x")));
        assert!(!eval("n.missing", "=", json!(1)));
        assert!(eval("n.missing", "≠", json!(1)));
    }

    #[test]
    fn booleans() {
        assert!(eval("n.b", "is", json!("true")));
        assert!(eval("n.b", "=", json!(true)));
        assert!(!eval("n.b", "is", json!("false")));
    }

    #[test]
    fn arrays_and_membership() {
        assert!(eval("n.arr", "contains", json!("a")));
        assert!(eval("n.arr", "contains", json!("1")));
        assert!(eval("n.arr", "all of", json!(["a", "b"])));
        assert!(!eval("n.arr", "all of", json!(["a", "z"])));
        assert!(eval("n.i", "in", json!(["1", "3"])));
        assert!(eval("n.s", "not in", json!(["hello"])));
        // `in` with a plain string is list membership, not substring.
        assert!(!eval("n.ref", "in", json!("hello world")));
        assert!(eval("n.ref", "in", json!("hello, world")));
    }

    #[test]
    fn type_errors_are_reported() {
        assert!(evaluate(&cond("n.i", "start with", json!("3")), &pool()).is_err());
        assert!(evaluate(&cond("n.s", ">", json!(1)), &pool()).is_err());
    }

    #[test]
    fn group_short_circuit_and_trace() {
        let conds = vec![cond("n.i", "=", json!(4)), cond("n.s", "contains", json!("hello"))];
        let (r, t) = evaluate_group(&conds, LogicalOperator::And, &pool()).unwrap();
        assert!(!r);
        assert_eq!(t.len(), 1);
        let (r, t) = evaluate_group(&conds, LogicalOperator::Or, &pool()).unwrap();
        assert!(r);
        assert_eq!(t.len(), 2);
        assert!(!evaluate_group(&[], LogicalOperator::And, &pool()).unwrap().0);
    }

    #[test]
    fn file_sub_conditions_respect_outer_operator() {
        let mut p = VariablePool::new();
        let files = json!([
            {"$file": true, "name": "a.png", "kind": "image", "extension": ".png"},
            {"$file": true, "name": "b.pdf", "kind": "document", "extension": ".pdf"}
        ]);
        p.insert("n", "files", Value::from_json_trusted(files));
        let mk = |op: &str, ext: &str| -> Condition {
            serde_json::from_value(json!({
                "variable_selector": ["n", "files"],
                "comparison_operator": op,
                "sub_variable_condition": {"logical_operator": "and", "conditions": [
                    {"key": "extension", "comparison_operator": "is", "value": ext}
                ]}
            }))
            .unwrap()
        };
        assert!(evaluate(&mk("contains", ".png"), &p).unwrap());
        assert!(!evaluate(&mk("not contains", ".png"), &p).unwrap());
        assert!(evaluate(&mk("not contains", ".doc"), &p).unwrap());
        assert!(!evaluate(&mk("all of", ".png"), &p).unwrap());
    }

    #[test]
    fn operator_wire_names_roundtrip() {
        for op in ["contains", "≠", "≥", "≤", "start with", "not exists", "all of"] {
            let parsed: Operator = serde_json::from_value(json!(op)).unwrap();
            assert_eq!(parsed.as_str(), op);
        }
    }
}
