//! Catalog descriptors for the model-backed nodes.

use graphrt_core::ValueType;
use graphrt_core::catalog::schema::{model, nested, object, selector, template};
use graphrt_core::catalog::{Handles, NodeCategory, NodeDescriptor, OutputDescriptor};
use serde_json::{Value as Json, json};

const SOURCE: &str = "graphrt-llm";

fn base(kind: &str, title: &str, icon: &str) -> NodeDescriptor {
    NodeDescriptor::new(kind, title, NodeCategory::Model).icon(icon).source(SOURCE)
}

fn prompt_message() -> Json {
    nested(
        json!({
            "role": { "type": "string", "enum": ["system", "user", "assistant"], "default": "user" },
            "text": template("Message template; `{{#context#}}` inserts the retrieved context")
        }),
        &[],
    )
}

fn memory() -> Json {
    nested(
        json!({
            "window": nested(json!({ "enabled": { "type": "boolean" }, "size": { "type": "integer", "minimum": 0 } }), &[]),
            "variable_selector": selector(),
            "query_prompt_template": template("Template for the current user turn")
        }),
        &[],
    )
}

fn vision() -> Json {
    let detail = json!({ "type": "string", "enum": ["low", "high", "auto"] });
    nested(
        json!({
            "enabled": { "type": "boolean", "default": false },
            "variable_selector": selector(),
            "detail": detail,
            "configs": nested(json!({ "variable_selector": selector(), "detail": detail }), &[])
        }),
        &[],
    )
}

fn usage_output() -> OutputDescriptor {
    OutputDescriptor::new("usage", ValueType::Object).describe("{prompt_tokens, completion_tokens, total_tokens}")
}

pub(crate) fn llm() -> NodeDescriptor {
    base("llm", "LLM", "brain")
        .describe(
            "Calls a chat model with a prompt template, optional retrieved context, image inputs, \
             conversation memory and structured (JSON Schema) output. Streams `text` and `reasoning`.",
        )
        .schema(object(
            json!({
                "model": model(),
                "prompt_template": {
                    "oneOf": [ { "type": "array", "items": prompt_message() }, prompt_message() ]
                },
                "context": nested(json!({ "enabled": { "type": "boolean" }, "variable_selector": selector() }), &[]),
                "vision": vision(),
                "memory": memory(),
                "structured_output": nested(json!({ "enabled": { "type": "boolean" }, "schema": { "type": "object" } }), &[]),
                "structured_output_enabled": { "type": "boolean" },
                "structured_output_switch_on": { "type": "boolean" },
                "reasoning_format": { "type": "string", "enum": ["tagged", "separated"], "default": "tagged" }
            }),
            &["model"],
        ))
        .output(OutputDescriptor::new("text", ValueType::String).streaming())
        .output(OutputDescriptor::new("reasoning", ValueType::String).streaming().describe("Reasoning, when the model emits it"))
        .output(usage_output())
        .output(OutputDescriptor::new("finish_reason", ValueType::String))
        .output(OutputDescriptor::new("structured_output", ValueType::Object).describe("Parsed JSON when structured output is enabled"))
}

pub(crate) fn question_classifier() -> NodeDescriptor {
    let class = nested(
        json!({ "id": { "type": "string", "minLength": 1 }, "name": { "type": "string", "minLength": 1 }, "label": { "type": "string" } }),
        &["id", "name"],
    );
    base("question-classifier", "Question Classifier", "tags")
        .describe(
            "Asks a model to pick one of the configured classes and takes the edge whose handle is the \
             class id. An unrecognized answer fails the node unless `fallback_class` is set.",
        )
        .schema(object(
            json!({
                "model": model(),
                "query_variable_selector": selector(),
                "query": selector(),
                "classes": { "type": "array", "items": class, "minItems": 1 },
                "instruction": template("Extra classification instructions"),
                "memory": memory(),
                "vision": vision(),
                "fallback_class": { "type": "string" }
            }),
            &["model", "classes"],
        ))
        .output(OutputDescriptor::new("class_id", ValueType::String))
        .output(OutputDescriptor::new("class_name", ValueType::String))
        .output(OutputDescriptor::new("class_label", ValueType::String))
        .output(usage_output())
        .handles(Handles::Dynamic { config_path: Some("classes[].id".into()) })
}

pub(crate) fn parameter_extractor() -> NodeDescriptor {
    let parameter = nested(
        json!({
            "name": { "type": "string", "minLength": 1 },
            "type": {
                "type": "string",
                "enum": ["string", "number", "boolean", "bool", "object", "select", "array[string]", "array[number]", "array[boolean]", "array[object]"]
            },
            "description": { "type": "string" },
            "required": { "type": "boolean", "default": false },
            "options": { "type": "array", "items": { "type": "string" } }
        }),
        &["name", "type"],
    );
    base("parameter-extractor", "Parameter Extractor", "filter")
        .describe(
            "Extracts typed parameters from text using function calling or a JSON prompt. Outputs each \
             parameter plus `__is_success` / `__reason`.",
        )
        .schema(object(
            json!({
                "model": model(),
                "query": selector(),
                "parameters": { "type": "array", "items": parameter, "minItems": 1 },
                "instruction": template("Extra extraction instructions"),
                "reasoning_mode": { "type": "string", "enum": ["function_call", "prompt"], "default": "function_call" },
                "memory": memory(),
                "vision": vision()
            }),
            &["model", "query", "parameters"],
        ))
        .output(
            OutputDescriptor::new("__is_success", ValueType::Integer)
                .describe("1 when every required parameter was extracted"),
        )
        .output(OutputDescriptor::new("__reason", ValueType::String))
        .output(OutputDescriptor::new("__usage", ValueType::Object))
        .output(usage_output())
        .dynamic_outputs()
}

pub(crate) fn agent() -> NodeDescriptor {
    let tool = json!({
        "oneOf": [
            { "type": "string", "description": "`provider/tool` or `provider/*`" },
            nested(json!({ "provider": { "type": "string" }, "name": { "type": "string" } }), &["provider", "name"])
        ]
    });
    base("agent", "Agent", "bot")
        .describe(
            "Tool-calling agent loop: the model calls tools (native, MCP or plugin) until it answers or \
             `max_iterations` is reached. Tool calls can run in parallel and selected tools can require \
             human approval (the run pauses). Streams the final answer as `text`.",
        )
        .schema(object(
            json!({
                "model": model(),
                "instruction": template("System prompt"),
                "query": template("User turn (default `{{#sys.query#}}`)"),
                "tools": { "type": "array", "items": tool },
                "max_iterations": { "type": "integer", "minimum": 1, "maximum": 100, "default": 10 },
                "max_tool_calls_per_turn": { "type": "integer", "minimum": 1 },
                "parallel_tool_calls": { "type": "boolean", "default": true },
                "memory": memory(),
                "vision": vision(),
                "require_approval": { "type": "array", "items": { "type": "string" } },
                "max_tool_output_chars": { "type": "integer", "minimum": 1, "default": 32000 }
            }),
            &["model"],
        ))
        .output(OutputDescriptor::new("text", ValueType::String).streaming().describe("Final answer"))
        .output(
            OutputDescriptor::new("tool_calls", ValueType::ArrayObject)
                .describe("[{name, provider, tool, arguments, result, is_error}]"),
        )
        .output(OutputDescriptor::new("iterations", ValueType::Integer))
        .output(
            OutputDescriptor::new("stopped_reason", ValueType::String).describe("completed | max_iterations | length"),
        )
        .output(usage_output())
}
