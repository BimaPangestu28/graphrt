//! `parameter-extractor` node: extracts typed parameters from text with a model.
//!
//! Each declared parameter becomes an output. Values are validated and coerced
//! to their declared type. Validation problems do not fail the node: they set
//! `__is_success` to 0 and explain the problem in `__reason`, so a graph can
//! branch on them. This includes malformed function-call arguments, which are
//! never treated as an empty (successful) result. Model invocation errors fail
//! the node.

use std::sync::Arc;

use futures::future::BoxFuture;
use graphrt_core::dsl::NodeDoc;
use graphrt_core::node::{ConfigError, NodeResult, parse_config};
use graphrt_core::template::Template;
use graphrt_core::tool::ToolSpec;
use graphrt_core::value::Value;
use graphrt_core::{Node, NodeContext, NodeError, NodeOutput, Selector};
use serde::Deserialize;
use serde_json::{Map, Value as Json, json};

use super::common::{
    MemoryConfig, ModelConfig, StreamMode, VisionConfig, extract_json_where, invoke, model_for, usage_value,
};
use crate::types::{Message, ToolChoice};

/// Name of the synthetic function used in function-call mode.
pub const EXTRACT_TOOL: &str = "extract_parameters";

/// Declared parameter type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub enum ParamType {
    #[serde(rename = "string")]
    String,
    #[serde(rename = "number")]
    Number,
    #[serde(rename = "boolean", alias = "bool")]
    Boolean,
    #[serde(rename = "object")]
    Object,
    #[serde(rename = "select")]
    Select,
    #[serde(rename = "array[string]")]
    ArrayString,
    #[serde(rename = "array[number]")]
    ArrayNumber,
    #[serde(rename = "array[boolean]")]
    ArrayBoolean,
    #[serde(rename = "array[object]")]
    ArrayObject,
}

impl ParamType {
    fn element(self) -> Option<ParamType> {
        match self {
            ParamType::ArrayString => Some(ParamType::String),
            ParamType::ArrayNumber => Some(ParamType::Number),
            ParamType::ArrayBoolean => Some(ParamType::Boolean),
            ParamType::ArrayObject => Some(ParamType::Object),
            _ => None,
        }
    }

    fn json_type(self) -> &'static str {
        match self {
            ParamType::String | ParamType::Select => "string",
            ParamType::Number => "number",
            ParamType::Boolean => "boolean",
            ParamType::Object => "object",
            _ => "array",
        }
    }
}

/// One parameter to extract.
#[derive(Clone, Debug, Deserialize)]
pub struct ParamConfig {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: ParamType,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub options: Vec<String>,
}

/// How the model is asked to answer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningMode {
    /// A forced function call whose schema is the parameter list.
    #[default]
    FunctionCall,
    /// A JSON answer requested in the prompt.
    Prompt,
}

/// Configuration of the `parameter-extractor` node.
#[derive(Clone, Debug, Deserialize)]
pub struct ExtractorConfig {
    pub model: ModelConfig,
    pub query: Selector,
    pub parameters: Vec<ParamConfig>,
    #[serde(default)]
    pub instruction: Option<Template>,
    #[serde(default)]
    pub reasoning_mode: ReasoningMode,
    #[serde(default)]
    pub memory: Option<MemoryConfig>,
    #[serde(default)]
    pub vision: VisionConfig,
}

/// The `parameter-extractor` node.
#[derive(Debug)]
pub struct ExtractorNode {
    config: ExtractorConfig,
    schema: Json,
}

const RESERVED: &[&str] = &["__is_success", "__reason", "__usage", "usage"];

impl ExtractorNode {
    pub fn from_doc(doc: &NodeDoc) -> Result<Self, ConfigError> {
        let config: ExtractorConfig = parse_config(doc)?;
        if config.parameters.is_empty() {
            return Err(ConfigError::new(doc, "parameters must not be empty"));
        }
        let mut seen = std::collections::HashSet::new();
        for p in &config.parameters {
            if p.name.is_empty() || RESERVED.contains(&p.name.as_str()) || !seen.insert(p.name.as_str()) {
                return Err(ConfigError::new(doc, format!("invalid or duplicate parameter name `{}`", p.name)));
            }
        }
        let schema = parameters_schema(&config.parameters);
        Ok(Self { config, schema })
    }

    async fn execute(&self, ctx: NodeContext) -> NodeResult {
        let cfg = &self.config;
        let pool = &ctx.pool;
        let query = pool
            .get(&cfg.query)
            .map(|v| v.to_text())
            .ok_or_else(|| NodeError::invalid_input(format!("query variable `{}` is missing", cfg.query)))?;
        let instruction = cfg.instruction.as_ref().map(|t| t.render(pool)).unwrap_or_default();

        let mut system = match cfg.reasoning_mode {
            ReasoningMode::FunctionCall => {
                format!(
                    "Extract the requested parameters from the user's input by calling the `{EXTRACT_TOOL}` function."
                )
            }
            ReasoningMode::Prompt => format!(
                "Extract the requested parameters from the user's input. Respond only with a JSON object matching this JSON Schema:\n{}",
                self.schema
            ),
        };
        system.push_str(" Omit parameters that are not present in the input; never invent values.");
        if !instruction.trim().is_empty() {
            system.push_str("\n\nAdditional instructions:\n");
            system.push_str(instruction.trim());
        }
        let mut messages = vec![Message::system(system)];
        if let Some(memory) = &cfg.memory {
            messages.extend(memory.history(pool)?);
        }
        let mut user = Message::user(query);
        #[cfg(feature = "vision-preparer")]
        user.content.extend(cfg.vision.prepared_image_parts(&ctx).await?);
        #[cfg(not(feature = "vision-preparer"))]
        user.content.extend(cfg.vision.image_parts(pool)?);
        messages.push(user);

        let mut request = cfg.model.request(messages, pool)?;
        if cfg.reasoning_mode == ReasoningMode::FunctionCall {
            request.tools = vec![ToolSpec {
                name: EXTRACT_TOOL.to_owned(),
                description: "Record the parameters extracted from the user's input.".to_owned(),
                parameters: self.schema.clone(),
            }];
            request.tool_choice = Some(ToolChoice::Tool(EXTRACT_TOOL.to_owned()));
        }

        let model = model_for(&ctx, &cfg.model)?;
        let response = invoke(&ctx, model.as_ref(), request, StreamMode::Off).await?;
        crate::nodes::common::charge_model(&ctx, &cfg.model, &response.usage)?;
        let raw: Result<Map<String, Json>, String> = match response.tool_calls.iter().find(|c| c.name == EXTRACT_TOOL) {
            Some(call) => call.arguments_json().map_err(|e| format!("model returned invalid function arguments: {e}")),
            None => match extract_json_where(&response.text, Json::is_object) {
                Some(Json::Object(values)) => Ok(values),
                _ => Err("model did not return a JSON object of parameters".to_owned()),
            },
        };

        let mut output = NodeOutput::new();
        let mut problems: Vec<String> = Vec::new();
        match raw {
            Ok(values) => {
                for p in &cfg.parameters {
                    let value = match values.get(&p.name).filter(|v| !v.is_null()) {
                        None if p.required => {
                            problems.push(format!("missing required parameter `{}`", p.name));
                            Value::Null
                        }
                        None => Value::Null,
                        Some(v) => match coerce(p, v) {
                            Ok(v) => v,
                            Err(e) => {
                                problems.push(format!("parameter `{}`: {e}", p.name));
                                Value::Null
                            }
                        },
                    };
                    output.outputs.insert(p.name.clone(), value);
                }
            }
            Err(problem) => {
                problems.push(problem);
                for p in &cfg.parameters {
                    output.outputs.insert(p.name.clone(), Value::Null);
                }
            }
        }
        let usage = usage_value(&response.usage);
        Ok(output
            .with("__is_success", i64::from(problems.is_empty()))
            .with("__reason", problems.join("; "))
            .with("__usage", usage.clone())
            .with("usage", usage)
            .usage(response.usage)
            .into())
    }
}

/// JSON Schema object describing all parameters.
fn parameters_schema(params: &[ParamConfig]) -> Json {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for p in params {
        let mut prop = Map::new();
        prop.insert("type".into(), json!(p.ty.json_type()));
        if let Some(elem) = p.ty.element() {
            prop.insert("items".into(), json!({"type": elem.json_type()}));
        }
        if p.ty == ParamType::Select && !p.options.is_empty() {
            prop.insert("enum".into(), json!(p.options));
        }
        if !p.description.is_empty() {
            prop.insert("description".into(), json!(p.description));
        }
        properties.insert(p.name.clone(), Json::Object(prop));
        if p.required {
            required.push(json!(p.name));
        }
    }
    json!({"type": "object", "properties": properties, "required": required})
}

fn coerce_scalar(ty: ParamType, options: &[String], v: &Json) -> Result<Value, String> {
    match (ty, v) {
        (ParamType::String, Json::String(_)) => Ok(Value::from_json(v.clone())),
        (ParamType::String, Json::Number(n)) => Ok(Value::from(n.to_string())),
        (ParamType::String, Json::Bool(b)) => Ok(Value::from(b.to_string())),
        (ParamType::Select, Json::String(s)) => {
            if options.is_empty() || options.iter().any(|o| o == s) {
                Ok(Value::from(s.as_str()))
            } else {
                Err(format!("`{s}` is not one of {options:?}"))
            }
        }
        (ParamType::Number, Json::Number(_)) => Ok(Value::from_json(v.clone())),
        (ParamType::Number, Json::String(s)) => {
            let t = s.trim();
            t.parse::<i64>()
                .map(Value::Int)
                .or_else(|_| t.parse::<f64>().map(Value::Float))
                .map_err(|_| format!("`{s}` is not a number"))
        }
        (ParamType::Boolean, Json::Bool(b)) => Ok(Value::Bool(*b)),
        (ParamType::Boolean, Json::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" => Ok(Value::Bool(true)),
            "false" | "no" => Ok(Value::Bool(false)),
            _ => Err(format!("`{s}` is not a boolean")),
        },
        (ParamType::Object, Json::Object(_)) => Ok(Value::from_json(v.clone())),
        (t, other) => Err(format!("expected {}, got {}", t.json_type(), json_kind(other))),
    }
}

fn json_kind(v: &Json) -> &'static str {
    match v {
        Json::Null => "null",
        Json::Bool(_) => "boolean",
        Json::Number(_) => "number",
        Json::String(_) => "string",
        Json::Array(_) => "array",
        Json::Object(_) => "object",
    }
}

fn coerce(p: &ParamConfig, v: &Json) -> Result<Value, String> {
    match p.ty.element() {
        None => coerce_scalar(p.ty, &p.options, v),
        Some(elem) => match v {
            Json::Array(items) => {
                items.iter().map(|i| coerce_scalar(elem, &[], i)).collect::<Result<Vec<_>, _>>().map(Value::from)
            }
            other => Err(format!("expected array, got {}", json_kind(other))),
        },
    }
}

impl Node for ExtractorNode {
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(self.execute(ctx))
    }

    /// Each model response is charged with its cost as it arrives.
    fn tracks_turns(&self) -> bool {
        true
    }

    fn inputs(&self) -> Vec<Selector> {
        let cfg = &self.config;
        let mut out = vec![cfg.query.clone()];
        if let Some(instruction) = &cfg.instruction {
            out.extend(instruction.selectors().cloned());
        }
        if let Some(memory) = &cfg.memory {
            out.extend(memory.selectors());
        }
        out.extend(cfg.vision.selectors());
        out.extend(cfg.model.selectors());
        out
    }
}

/// Factory for the node registry.
pub fn factory(doc: &NodeDoc) -> Result<Arc<dyn Node>, ConfigError> {
    Ok(Arc::new(ExtractorNode::from_doc(doc)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockModel;
    use crate::registry::ModelRegistry;
    use crate::types::ChatDelta;
    use graphrt_core::{NodeOutcome, Services, VariablePool};

    fn node(mode: &str) -> ExtractorNode {
        ExtractorNode::from_doc(&NodeDoc::new(
            "pe",
            "parameter-extractor",
            json!({
                "model": {"provider": "openai", "name": "gpt"},
                "query": ["sys", "query"],
                "reasoning_mode": mode,
                "parameters": [
                    {"name": "city", "type": "string", "required": true},
                    {"name": "days", "type": "number", "required": true},
                    {"name": "unit", "type": "select", "options": ["c", "f"]},
                    {"name": "tags", "type": "array[string]"},
                    {"name": "note", "type": "string"}
                ]
            }),
        ))
        .unwrap()
    }

    async fn run(node: &ExtractorNode, response: Vec<ChatDelta>) -> (NodeOutput, Arc<MockModel>) {
        let model = Arc::new(MockModel::new(vec![response]));
        let mut pool = VariablePool::new();
        pool.insert("sys", "query", Value::from("weather in Paris for 3 days"));
        let ctx = NodeContext::for_test("pe", pool).with_services(Arc::new(
            Services::new().with(ModelRegistry::new().with("openai", Arc::clone(&model) as Arc<dyn crate::ChatModel>)),
        ));
        match node.run(ctx).await.unwrap() {
            NodeOutcome::Completed(o) => (o, model),
            NodeOutcome::Paused(_) => panic!("paused"),
        }
    }

    #[tokio::test]
    async fn function_call_mode_coerces_types() {
        let n = node("function_call");
        let (out, model) = run(
            &n,
            MockModel::tool_calls(&[("c", EXTRACT_TOOL, r#"{"city":"Paris","days":"3","unit":"c","tags":["a"]}"#)]),
        )
        .await;
        assert_eq!(out.outputs["__is_success"], Value::Int(1));
        assert_eq!(out.outputs["days"], Value::Int(3));
        assert_eq!(out.outputs["note"], Value::Null, "missing optional parameters are fine");
        let req = &model.requests()[0];
        assert_eq!(req.tool_choice, Some(ToolChoice::Tool(EXTRACT_TOOL.into())));
        assert_eq!(req.tools[0].parameters["properties"]["unit"]["enum"][1], "f");
        assert_eq!(req.tools[0].parameters["required"][0], "city");
    }

    #[tokio::test]
    async fn prompt_mode_reports_validation_problems() {
        let n = node("prompt");
        let (out, model) = run(&n, MockModel::text("Here: {\"city\": \"Paris\", \"unit\": \"k\"}")).await;
        assert_eq!(out.outputs["__is_success"], Value::Int(0));
        let reason = out.outputs["__reason"].to_text();
        assert!(reason.contains("days"));
        assert!(reason.contains("unit"));
        assert_eq!(out.outputs["city"], Value::from("Paris"));
        assert!(model.requests()[0].tools.is_empty());
    }

    #[tokio::test]
    async fn malformed_function_arguments_are_not_a_success() {
        let n = node("function_call");
        let (out, _) = run(&n, MockModel::tool_calls(&[("c", EXTRACT_TOOL, r#"{"city":"Par"#)])).await;
        assert_eq!(out.outputs["__is_success"], Value::Int(0));
        assert!(out.outputs["__reason"].to_text().contains("invalid function arguments"));
        assert_eq!(out.outputs["city"], Value::Null);
    }

    #[tokio::test]
    async fn non_json_answer_is_soft_failure() {
        let (out, _) = run(&node("prompt"), MockModel::text("sorry")).await;
        assert_eq!(out.outputs["__is_success"], Value::Int(0));
    }

    #[test]
    fn rejects_reserved_names() {
        let doc = NodeDoc::new(
            "pe",
            "parameter-extractor",
            json!({"model": {"provider": "p", "name": "m"}, "query": "sys.query",
                   "parameters": [{"name": "__reason", "type": "string"}]}),
        );
        assert!(ExtractorNode::from_doc(&doc).is_err());
    }
}
