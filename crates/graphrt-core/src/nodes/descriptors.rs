//! Catalog descriptors for the built-in core nodes.

use serde_json::json;

use crate::catalog::schema::{
    conditions, deprecated_alias, key_condition, logical_operator, nested, object, selector, template, value_type,
};
use crate::catalog::{Handles, NodeCategory, NodeDescriptor, OutputDescriptor};
use crate::value::ValueType;

const SOURCE: &str = "builtin";

fn base(kind: &str, title: &str, category: NodeCategory, icon: &str) -> NodeDescriptor {
    NodeDescriptor::new(kind, title, category).icon(icon).source(SOURCE)
}

pub(crate) fn start() -> NodeDescriptor {
    let variable = nested(
        json!({
            "variable": { "type": "string", "minLength": 1 },
            "label": { "type": "string" },
            "type": value_type(),
            "required": { "type": "boolean", "default": false },
            "default": {},
            "options": { "type": "array", "items": { "type": "string" } },
            "max_length": { "type": "integer", "minimum": 0 }
        }),
        &["variable"],
    );
    base("start", "Start", NodeCategory::Io, "play")
        .describe("Entry point. Validates and coerces run inputs and exposes them as outputs.")
        .schema(object(json!({ "variables": { "type": "array", "items": variable } }), &[]))
        .dynamic_outputs()
}

pub(crate) fn end() -> NodeDescriptor {
    let output = nested(
        json!({ "variable": { "type": "string", "minLength": 1 }, "value_selector": selector(), "value_type": value_type() }),
        &["variable", "value_selector"],
    );
    base("end", "End", NodeCategory::Io, "flag")
        .describe("Collects selected values as the run outputs.")
        .schema(object(json!({ "outputs": { "type": "array", "items": output } }), &[]))
        .dynamic_outputs()
}

pub(crate) fn answer() -> NodeDescriptor {
    base("answer", "Answer", NodeCategory::Io, "message")
        .describe("Renders a reply template. Referenced streaming outputs are relayed to the user in order.")
        .schema(object(json!({ "answer": template("Reply template with {{#node.var#}} placeholders") }), &["answer"]))
        .output(OutputDescriptor::new("answer", ValueType::String).describe("Rendered reply text"))
        .output(OutputDescriptor::new("files", ValueType::ArrayFile).describe("Files referenced by the template"))
}

pub(crate) fn if_else() -> NodeDescriptor {
    let case = nested(
        json!({
            "case_id": { "type": "string", "minLength": 1 },
            "logical_operator": logical_operator(),
            "conditions": conditions()
        }),
        &["case_id"],
    );
    base("if-else", "If / Else", NodeCategory::Logic, "split")
        .describe(
            "Takes the edge of the first case whose conditions hold, or the `false` edge. \
             Legacy single-case configs (top-level `conditions`) use case id `true`.",
        )
        .schema(object(
            json!({
                "cases": { "type": "array", "items": case },
                "conditions": conditions(),
                "logical_operator": logical_operator()
            }),
            &[],
        ))
        .output(OutputDescriptor::new("result", ValueType::Boolean).describe("Whether any case matched"))
        .output(OutputDescriptor::new("selected_case_id", ValueType::String).describe("Matched case id, or null"))
        .handles(Handles::Dynamic { config_path: Some("cases[].case_id + \"false\"".into()) })
}

#[cfg(feature = "jinja")]
pub(crate) fn template_transform() -> NodeDescriptor {
    base("template-transform", "Template", NodeCategory::Transform, "braces")
        .describe("Renders a sandboxed Jinja2 template (fuel and output-size limited) from bound variables.")
        .schema(object(
            json!({
                "template": { "type": "string", "description": "Jinja2 template" },
                "variables": crate::catalog::schema::variable_bindings()
            }),
            &["template"],
        ))
        .output(OutputDescriptor::new("output", ValueType::String))
}

pub(crate) fn aggregator() -> NodeDescriptor {
    let group = nested(
        json!({ "group_name": { "type": "string", "minLength": 1 }, "variables": { "type": "array", "items": selector() } }),
        &["group_name"],
    );
    base("variable-aggregator", "Variable Aggregator", NodeCategory::Transform, "merge")
        .describe(
            "Outputs the first non-null value among the listed selectors (typically one per branch). \
             With groups enabled, outputs one `{output}` object per group.",
        )
        .schema(object(
            json!({
                "variables": { "type": "array", "items": selector() },
                "advanced_settings": nested(
                    json!({ "group_enabled": { "type": "boolean", "default": false }, "groups": { "type": "array", "items": group } }),
                    &[]
                )
            }),
            &[],
        ))
        .output(OutputDescriptor::new("output", ValueType::Any).describe("First non-null value (ungrouped mode)"))
        .dynamic_outputs()
}

fn assign_operation() -> serde_json::Value {
    json!({
        "type": "string",
        "enum": ["over-write", "overwrite", "clear", "set", "append", "extend", "remove-first", "remove-last", "+=", "-=", "*=", "/="]
    })
}

fn assigner_schema() -> serde_json::Value {
    let item = nested(
        json!({
            "variable_selector": selector(),
            "input_type": { "type": "string", "enum": ["variable", "constant"], "default": "constant" },
            "operation": assign_operation(),
            "value": { "description": "Constant value, or a selector when input_type is `variable`" }
        }),
        &["variable_selector", "operation"],
    );
    object(
        json!({
            "items": { "type": "array", "items": item },
            "assigned_variable_selector": selector(),
            "input_variable_selector": selector(),
            "write_mode": assign_operation()
        }),
        &[],
    )
}

pub(crate) fn assigner() -> NodeDescriptor {
    base("assigner", "Variable Assigner", NodeCategory::Transform, "pencil")
        .describe(
            "Writes conversation, loop or other variables. All items are validated before any write is \
             committed; writes to `sys` and `env` are rejected.",
        )
        .schema(assigner_schema())
}

/// `variable-assigner` accepts both assigner and aggregator configs (detected by shape).
pub(crate) fn variable_assigner() -> NodeDescriptor {
    let mut schema = assigner_schema();
    let aggregator_props = aggregator().config_schema.get("properties").cloned();
    if let (Some(props), Some(serde_json::Value::Object(extra))) =
        (schema.get_mut("properties").and_then(serde_json::Value::as_object_mut), aggregator_props)
    {
        props.extend(extra);
    }
    base("variable-assigner", "Variable Assigner", NodeCategory::Transform, "pencil")
        .describe(
            "Assigner for configs with `items` / `assigned_variable_selector`; variable aggregator for \
             configs with `variables`.",
        )
        .schema(schema)
        .dynamic_outputs()
}

pub(crate) fn list_operator() -> NodeDescriptor {
    base("list-operator", "List Operator", NodeCategory::Transform, "list")
        .describe("Filters, sorts, extracts and limits the items of an array.")
        .schema(object(
            json!({
                "variable": selector(),
                "filter_by": nested(json!({ "enabled": { "type": "boolean" }, "conditions": { "type": "array", "items": key_condition() } }), &[]),
                "extract_by": nested(json!({ "enabled": { "type": "boolean" }, "serial": { "description": "1-based index (number, string or template)" } }), &[]),
                "order_by": nested(json!({ "enabled": { "type": "boolean" }, "key": { "type": "string" }, "value": { "type": "string", "enum": ["asc", "desc"] } }), &[]),
                "limit": nested(json!({ "enabled": { "type": "boolean" }, "size": { "type": "integer" } }), &[])
            }),
            &["variable"],
        ))
        .output(OutputDescriptor::new("result", ValueType::ArrayAny))
        .output(OutputDescriptor::new("first_record", ValueType::Any))
        .output(OutputDescriptor::new("last_record", ValueType::Any))
}

pub(crate) fn iteration() -> NodeDescriptor {
    base("iteration", "Iteration", NodeCategory::Logic, "repeat")
        .describe(
            "Runs its body once per array item (`<id>.item`, `<id>.index`), sequentially or with bounded \
             parallelism. Output order always matches input order.",
        )
        .schema(object(
            json!({
                "iterator_selector": selector(),
                "output_selector": selector(),
                "is_parallel": { "type": "boolean", "default": false },
                "parallel_nums": { "type": "integer", "minimum": 1, "maximum": 100, "default": 10 },
                "error_handle_mode": { "type": "string", "enum": ["terminated", "continue-on-error", "remove-abnormal-output"], "default": "terminated" },
                "flatten_output": { "type": "boolean", "default": true },
                "start_node_id": { "type": "string", "description": "Ignored; the body entry is detected" }
            }),
            &["iterator_selector"],
        ))
        .output(OutputDescriptor::new("output", ValueType::ArrayAny).describe("Per-item results in input order"))
        .output(OutputDescriptor::new("errors", ValueType::ArrayObject).describe("[{index, error}] in continue-on-error mode"))
        .container()
}

pub(crate) fn loop_node() -> NodeDescriptor {
    let input_type = json!({ "type": "string", "enum": ["constant", "variable"], "default": "constant" });
    let var = nested(
        json!({
            "name": { "type": "string", "minLength": 1 },
            "type": value_type(),
            "input_type": input_type,
            "value": { "description": "A literal (input_type `constant`) or a selector (`variable`)" },
            "label": deprecated_alias(json!({ "type": "string", "minLength": 1 }), "name"),
            "var_type": deprecated_alias(value_type(), "type"),
            "value_type": deprecated_alias(input_type.clone(), "input_type")
        }),
        &[],
    );
    base("loop", "Loop", NodeCategory::Logic, "loop")
        .describe(
            "Runs its body repeatedly until a break condition holds, a `loop-end` node runs, or `loop_count` \
             rounds complete. Outputs the loop variables and `loop_round`.",
        )
        .schema(object(
            json!({
                "loop_count": { "type": "integer", "minimum": 1, "maximum": 10000, "default": 1 },
                "break_conditions": conditions(),
                "logical_operator": logical_operator(),
                "loop_variables": { "type": "array", "items": var }
            }),
            &[],
        ))
        .output(OutputDescriptor::new("loop_round", ValueType::Integer))
        .dynamic_outputs()
        .container()
}

pub(crate) fn iteration_start() -> NodeDescriptor {
    base("iteration-start", "Iteration Start", NodeCategory::Logic, "play")
        .describe("Entry marker of an iteration body.")
        .schema(object(json!({}), &[]))
}

pub(crate) fn loop_start() -> NodeDescriptor {
    base("loop-start", "Loop Start", NodeCategory::Logic, "play")
        .describe("Entry marker of a loop body.")
        .schema(object(json!({}), &[]))
}

pub(crate) fn loop_end() -> NodeDescriptor {
    base("loop-end", "Loop Break", NodeCategory::Logic, "stop")
        .describe("Breaks the enclosing loop after the current round.")
        .schema(object(json!({}), &[]))
}

pub(crate) fn human_input() -> NodeDescriptor {
    let field = nested(
        json!({
            "name": { "type": "string", "minLength": 1 },
            "variable": { "type": "string", "minLength": 1 },
            "label": { "type": "string" },
            "type": { "type": "string", "default": "text" },
            "required": { "type": "boolean", "default": false },
            "default": {}
        }),
        &[],
    );
    let action = nested(json!({ "id": { "type": "string", "minLength": 1 }, "title": { "type": "string" } }), &["id"]);
    base("human-input", "Human Input", NodeCategory::Human, "user")
        .describe(
            "Pauses the run with a rendered form. Resume with `{action, values}`; the action id selects the \
             outgoing edge and the values become outputs.",
        )
        .schema(object(
            json!({
                "prompt": template("Message shown to the reviewer"),
                "content": template("Alias of `prompt`"),
                "form": nested(json!({ "fields": { "type": "array", "items": field } }), &[]),
                "inputs": { "type": "array", "items": field },
                "actions": { "type": "array", "items": action, "description": "Defaults to a single `submit` action" },
                "expire_after_ms": { "type": "integer", "minimum": 0, "description": "How long the host should wait for a response (reported as `timeout_ms` in the pause data)" },
                "on_expire": {
                    "description": "What the host does when the request expires: `stop` (default) or answer with one of the actions (reported as `on_expire` in the pause data; graphrt does not act on it)",
                    "oneOf": [
                        { "const": "stop" },
                        nested(json!({ "action": { "type": "string", "minLength": 1 } }), &["action"])
                    ]
                }
            }),
            &[],
        ))
        .output(OutputDescriptor::new("action", ValueType::String).describe("Selected action id"))
        .dynamic_outputs()
        .handles(Handles::Dynamic { config_path: Some("actions[].id (default: submit)".into()) })
}
