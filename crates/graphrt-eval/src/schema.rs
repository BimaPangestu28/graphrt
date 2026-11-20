//! JSON Schema of eval files, published as `schema/eval-v1.schema.json`.
//!
//! Regenerate the checked-in file with
//! `cargo run -q -p graphrt-eval --example eval-schema > schema/eval-v1.schema.json`.

use serde_json::{Value as Json, json};

/// Where the schema is published.
pub const EVAL_SCHEMA_URL: &str =
    "https://raw.githubusercontent.com/BimaPangestu28/graphrt/main/schema/eval-v1.schema.json";

/// Assertion types built into graphrt-eval, with a one-line description each.
pub const BUILTIN_ASSERTIONS: &[(&str, &str)] = &[
    ("equals", "The output (or `transform` path) equals `value`; non-string values compare as JSON."),
    ("contains", "The output contains the `value` text."),
    ("icontains", "Case-insensitive `contains`."),
    ("contains-any", "The output contains at least one of the `value` texts."),
    ("icontains-any", "Case-insensitive `contains-any`."),
    ("contains-all", "The output contains every `value` text (score: fraction found)."),
    ("icontains-all", "Case-insensitive `contains-all`."),
    ("starts-with", "The output starts with the `value` text."),
    ("regex", "The output matches the `value` regular expression (Rust syntax; `(?i)` for case-insensitive)."),
    ("is-json", "The output is JSON; when `value` is a JSON Schema, it must also match it."),
    ("json-schema", "The output is JSON matching the `value` JSON Schema."),
    ("latency", "The run took at most `threshold` milliseconds."),
    ("tokens", "The run used at most `threshold` tokens (`value`: total, prompt or completion)."),
    ("trajectory:tool-used", "Agents called the `value` tool(s); `{name, min, max}` bounds the count."),
    ("trajectory:tool-sequence", "Agents called the `value` tools in order (`{steps, mode: in_order|exact}`)."),
    ("trajectory:tool-args-match", "A call of `value.name` had `value.args` (`mode: partial|exact`)."),
    ("trajectory:step-count", "The number of tool calls, agent steps or node runs is within `{min, max}`."),
    ("retrieval:recall-at-k", "Fraction of the `value` document ids in the top `k` knowledge-retrieval hits."),
    ("retrieval:precision-at-k", "Fraction of the top `k` hits that are relevant (`value` ids)."),
    ("retrieval:mrr", "Reciprocal rank of the first relevant hit in the top `k`."),
    ("verdict", "The run is a judge whose verdict (or `score` >= `threshold`) equals the `value` label."),
];

/// Assertion types a host registers (they need to compile workflows): the
/// `graphrt eval` command provides them.
pub const HOST_ASSERTIONS: &[(&str, &str)] =
    &[("judge", "Run the `workflow` judge on the output and read its verdict or score.")];

fn assertion_types() -> Vec<Json> {
    BUILTIN_ASSERTIONS
        .iter()
        .chain(HOST_ASSERTIONS)
        .flat_map(|(kind, _)| [Json::String((*kind).to_owned()), Json::String(format!("not-{kind}"))])
        .collect()
}

/// The eval file schema (draft 2020-12).
pub fn eval_file_schema() -> Json {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": EVAL_SCHEMA_URL,
        "title": "graphrt eval file",
        "description": "Test cases and assertions for `graphrt eval`. The `tests` / `assert` / `defaultTest` shape follows Promptfoo.",
        "type": "object",
        "required": ["workflow"],
        "properties": {
            "description": { "type": "string" },
            "workflow": { "type": "string", "description": "Workflow under test, relative to this file." },
            "providers": { "type": "string", "description": "Model providers file (e.g. with `kind: replay` entries), relative to this file." },
            "knowledge": { "type": "string", "description": "Knowledge base config for knowledge-retrieval nodes, relative to this file." },
            "mcp": { "type": "string", "description": "MCP servers file, relative to this file." },
            "plugins": { "type": "array", "items": { "type": "string" }, "description": "Plugin directories, relative to this file." },
            "dataset": { "type": "string", "description": "JSONL file: one `{id?, vars, expected?, tags?, metadata?, assert?}` object per line." },
            "defaultTest": { "$ref": "#/$defs/defaultTest" },
            "tests": { "type": "array", "items": { "$ref": "#/$defs/test" } },
            "gate": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "pass_rate": { "type": "number", "minimum": 0, "maximum": 1, "default": 1, "description": "Minimum fraction of passing test runs (every trial counts); below it `graphrt eval` exits with code 5." },
                    "pass_hat_k": { "type": "number", "minimum": 0, "maximum": 1, "description": "Minimum mean pass^k over the test cases (all k trials pass); use with `--trials`." },
                    "k": { "type": "integer", "minimum": 1, "description": "k of pass@k and pass^k (default: the number of trials)." },
                    "baseline": {
                        "type": "object",
                        "additionalProperties": false,
                        "description": "Tolerances against `--baseline`: any regression beyond them fails the gate.",
                        "properties": {
                            "tolerance": { "type": "number", "minimum": 0, "maximum": 1, "default": 0.05, "description": "Largest allowed drop of a metric's mean score." },
                            "test_tolerance": { "type": "number", "minimum": 0, "maximum": 1, "default": 0, "description": "Largest allowed drop of a test's pass rate across its trials." },
                            "metrics": { "type": "object", "additionalProperties": { "type": "number", "minimum": 0, "maximum": 1 }, "description": "Per-metric tolerances." }
                        }
                    }
                }
            },
            "calibration": {
                "type": "object",
                "additionalProperties": false,
                "description": "Judge calibration: the workflow is a judge and each `expected` is a human label (pass / fail). Reports agreement, TPR, TNR and Cohen's kappa.",
                "properties": {
                    "unknown": { "enum": ["exclude", "fail"], "default": "exclude", "description": "`unknown` verdicts: left out of the confusion matrix, or counted as fail." },
                    "threshold": { "type": "number", "minimum": 0, "maximum": 1, "default": 0.5, "description": "Score at which a score-only judge says pass." },
                    "min_kappa": { "type": "number", "minimum": -1, "maximum": 1, "description": "Gate: minimum Cohen's kappa." },
                    "min_tpr": { "type": "number", "minimum": 0, "maximum": 1, "description": "Gate: minimum true positive rate." },
                    "min_tnr": { "type": "number", "minimum": 0, "maximum": 1, "description": "Gate: minimum true negative rate." }
                }
            }
        },
        "patternProperties": { "^x-": {} },
        "additionalProperties": false,
        "$defs": {
            "test": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Unique test id (default: `test-<n>`)." },
                    "description": { "type": "string" },
                    "vars": { "type": "object", "description": "Start inputs of the workflow." },
                    "expected": { "description": "Expected output; a test without assertions checks `equals: expected`. Available as `{{expected}}`." },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "metadata": { "type": "object" },
                    "threshold": { "type": "number", "minimum": 0, "maximum": 1, "description": "Pass when the weighted assertion score reaches this, instead of requiring every assertion." },
                    "assert": { "type": "array", "items": { "$ref": "#/$defs/assertion" } }
                },
                "patternProperties": { "^x-": {} },
                "additionalProperties": false
            },
            "defaultTest": {
                "type": "object",
                "description": "Merged into every test: vars and metadata as defaults, tags and assertions prepended.",
                "properties": {
                    "description": { "type": "string" },
                    "vars": { "type": "object" },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "metadata": { "type": "object" },
                    "threshold": { "type": "number", "minimum": 0, "maximum": 1 },
                    "assert": { "type": "array", "items": { "$ref": "#/$defs/assertion" } }
                },
                "additionalProperties": false
            },
            "assertion": {
                "type": "object",
                "required": ["type"],
                "properties": {
                    "type": {
                        "type": "string",
                        "description": "Assertion type; any type can be negated with a `not-` prefix.",
                        "anyOf": [ { "enum": assertion_types() }, { "type": "string" } ]
                    },
                    "value": { "description": "Expected value; strings may reference test variables as `{{name}}`." },
                    "threshold": { "type": "number" },
                    "weight": { "type": "number", "minimum": 0, "default": 1, "description": "Weight in the test score; 0 reports the result without failing the test." },
                    "metric": { "type": "string", "description": "Name the score is aggregated under in the summary." },
                    "transform": { "type": "string", "description": "Path to the value under test, e.g. `output.field` or `outputs.text` (no JavaScript)." },
                    "k": { "type": "integer", "minimum": 1, "description": "retrieval:*: rank cut-off." },
                    "node": { "type": "string", "description": "retrieval:*: only this knowledge-retrieval node." },
                    "workflow": { "type": "string", "description": "judge: the judge workflow file, relative to this eval file." },
                    "inputs": { "type": "object", "description": "judge: extra start inputs of the judge (values may reference test variables)." },
                    "unknown": { "enum": ["fail", "exclude"], "default": "fail", "description": "judge: an `unknown` verdict fails, or is reported without scoring or gating." }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::assertion::AssertionRegistry;

    #[test]
    fn every_builtin_type_is_listed() {
        let listed: Vec<&str> = BUILTIN_ASSERTIONS.iter().map(|(k, _)| *k).collect();
        let registry = AssertionRegistry::with_builtins();
        let registered: Vec<&str> = registry.kinds().collect();
        assert_eq!(listed, registered);
    }
}
