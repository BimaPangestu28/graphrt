//! `llm` node: a single (streaming) model call.
//!
//! Outputs: `text`, `reasoning` (when present), `usage`, `finish_reason` and,
//! with structured output enabled, `structured_output`. Answer tokens stream
//! as chunks on field `text`, reasoning on field `reasoning`. With
//! `reasoning_format: separated`, `<think>` sections are streamed on
//! `reasoning` too, so the streamed text always equals the `text` output.
//!
//! Structured output is parsed from the answer and checked against the schema
//! (top-level type, required properties, property types); a mismatch is a
//! retryable `structured_output` error.

use std::sync::Arc;

use futures::future::BoxFuture;
use graphrt_core::dsl::NodeDoc;
use graphrt_core::node::{ConfigError, NodeResult, parse_config};
use graphrt_core::value::Value;
use graphrt_core::{Node, NodeContext, NodeError, NodeOutput, Selector};
use serde::Deserialize;
use serde_json::Value as Json;

use super::common::{
    ContextConfig, FinishCheck, MemoryConfig, ModelConfig, PromptTemplate, StreamMode, VisionConfig, build_messages,
    check_finish, extract_json_where, finish_reason_value, invoke, model_for, split_think_tags, usage_value,
};
use super::schema;
use crate::types::{ChatResponse, ResponseFormat};

/// How `<think>` tags inside the answer are treated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReasoningFormat {
    /// Keep tags in `text` as produced.
    #[default]
    Tagged,
    /// Move tagged reasoning into `reasoning`.
    Separated,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct StructuredOutput {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    schema: Option<Json>,
}

/// Configuration of the `llm` node.
#[derive(Clone, Debug, Deserialize)]
pub struct LlmConfig {
    pub model: ModelConfig,
    #[serde(default)]
    pub prompt_template: PromptTemplate,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub vision: VisionConfig,
    #[serde(default)]
    pub memory: Option<MemoryConfig>,
    #[serde(default)]
    structured_output: Option<StructuredOutput>,
    #[serde(default, alias = "structured_output_switch_on")]
    structured_output_enabled: Option<bool>,
    #[serde(default)]
    pub reasoning_format: ReasoningFormat,
}

impl LlmConfig {
    /// JSON Schema for structured output, when enabled.
    fn output_schema(&self) -> Option<Json> {
        let so = self.structured_output.as_ref()?;
        let enabled = self.structured_output_enabled.or(so.enabled).unwrap_or(true);
        if !enabled {
            return None;
        }
        let schema = so.schema.clone()?;
        // Accept both `{schema: {...}}` and a bare schema.
        match schema.get("schema") {
            Some(inner) if inner.is_object() => Some(inner.clone()),
            _ => Some(schema),
        }
    }
}

/// The `llm` node.
#[derive(Debug)]
pub struct LlmNode {
    config: LlmConfig,
    schema: Option<Json>,
    /// `model` and `provider` metadata, built once instead of per run.
    model_name: Value,
    provider: Value,
}

impl LlmNode {
    pub fn from_doc(doc: &NodeDoc) -> Result<Self, ConfigError> {
        let config: LlmConfig = parse_config(doc)?;
        if config.prompt_template.messages().is_empty() && config.memory.is_none() {
            return Err(ConfigError::new(doc, "prompt_template must contain at least one message"));
        }
        let schema = config.output_schema();
        let model_name = Value::from(config.model.name.as_str());
        let provider = Value::from(config.model.provider.as_str());
        Ok(Self { config, schema, model_name, provider })
    }

    async fn execute(&self, ctx: NodeContext) -> NodeResult {
        let cfg = &self.config;
        let pool = &ctx.pool;
        let context = cfg.context.resolve(pool);
        let images = cfg.vision.image_parts(pool)?;
        let messages = build_messages(&cfg.prompt_template, pool, context.as_deref(), cfg.memory.as_ref(), images)?;
        if messages.is_empty() {
            return Err(NodeError::invalid_input("prompt rendered to no messages"));
        }
        let mut request = cfg.model.request(messages, pool)?;
        if let Some(schema) = &self.schema {
            request.response_format = Some(ResponseFormat::JsonSchema {
                name: "structured_output".into(),
                schema: schema.clone(),
                strict: false,
            });
        }

        let model = model_for(&ctx, &cfg.model)?;
        let mode = match cfg.reasoning_format {
            ReasoningFormat::Tagged => StreamMode::Raw,
            ReasoningFormat::Separated => StreamMode::SeparateThink,
        };
        let response = invoke(&ctx, model.as_ref(), request, mode).await?;
        let finish = check_finish(&response)?;
        let finish_reason = finish_reason_value(response.finish_reason.as_ref());
        let ChatResponse { text: answer, reasoning: model_reasoning, usage, warnings, .. } = response;

        let (text, mut reasoning) = match cfg.reasoning_format {
            ReasoningFormat::Tagged => (answer, String::new()),
            ReasoningFormat::Separated => split_think_tags(&answer),
        };
        if !model_reasoning.is_empty() {
            reasoning = if reasoning.is_empty() { model_reasoning } else { format!("{model_reasoning}\n{reasoning}") };
        }

        let mut output = NodeOutput::new()
            .with("text", text.as_str())
            .with("usage", usage_value(&usage))
            .with("finish_reason", finish_reason)
            .usage(usage)
            .meta("model", self.model_name.clone())
            .meta("provider", self.provider.clone());
        if !reasoning.is_empty() {
            output = output.with("reasoning", reasoning);
        }
        let mut warnings: Vec<Value> = warnings.into_iter().map(Value::from).collect();
        if let FinishCheck::Unknown(reason) = &finish {
            warnings.push(Value::from(format!("unrecognized finish reason `{reason}`")));
        }
        if !warnings.is_empty() {
            output = output.meta("warnings", Value::from(warnings));
        }
        if let Some(schema) = &self.schema {
            let parsed = extract_json_where(&text, |v| schema::matches_top_level_type(v, schema)).ok_or_else(|| {
                NodeError::new("structured_output", "model output contains no JSON value of the requested type")
            })?;
            schema::validate(&parsed, schema).map_err(|problem| {
                NodeError::new("structured_output", format!("model output does not match the schema: {problem}"))
            })?;
            output = output.with("structured_output", Value::from_json(parsed));
        }
        Ok(output.into())
    }
}

impl Node for LlmNode {
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(self.execute(ctx))
    }

    fn inputs(&self) -> Vec<Selector> {
        let cfg = &self.config;
        let mut out = cfg.prompt_template.selectors();
        out.extend(cfg.context.selectors());
        out.extend(cfg.vision.selectors());
        if let Some(memory) = &cfg.memory {
            out.extend(memory.selectors());
        }
        out.extend(cfg.model.selectors());
        out
    }
}

/// Factory for the node registry.
pub fn factory(doc: &NodeDoc) -> Result<Arc<dyn Node>, ConfigError> {
    Ok(Arc::new(LlmNode::from_doc(doc)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockModel;
    use crate::registry::ModelRegistry;
    use crate::types::{ChatDelta, ContentPart, Role};
    use graphrt_core::event::Event;
    use graphrt_core::{NodeOutcome, Services, VariablePool};
    use std::sync::Mutex;

    fn doc(config: Json) -> NodeDoc {
        NodeDoc::new("llm", "llm", config)
    }

    fn ctx_with(model: Arc<MockModel>, pool: VariablePool) -> (NodeContext, Arc<Mutex<Vec<Event>>>) {
        let services = Services::new().with(ModelRegistry::new().with("openai", model));
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink_events = Arc::clone(&events);
        let ctx = NodeContext::for_test("llm", pool)
            .with_services(Arc::new(services))
            .with_sink(Arc::new(move |e| sink_events.lock().unwrap().push(e)));
        (ctx, events)
    }

    fn completed(r: NodeResult) -> NodeOutput {
        match r.unwrap() {
            NodeOutcome::Completed(o) => o,
            NodeOutcome::Paused(p) => panic!("unexpected pause {p:?}"),
        }
    }

    #[tokio::test]
    async fn content_filter_is_an_error_and_unknown_reasons_are_reported() {
        let node = LlmNode::from_doc(&doc(serde_json::json!({
            "model": {"provider": "openai", "name": "m"},
            "prompt_template": [{"role": "user", "text": "hi"}]
        })))
        .unwrap();
        let run = |deltas: Vec<ChatDelta>| {
            let model = Arc::new(MockModel::new(vec![deltas]));
            let (ctx, _) = ctx_with(model, VariablePool::new());
            node.run(ctx)
        };
        let err = run(vec![ChatDelta::Finish(crate::types::FinishReason::ContentFilter)]).await.unwrap_err();
        assert_eq!(err.kind, "content_filter");
        assert!(!err.retryable);
        let err = run(vec![ChatDelta::Finish(crate::types::FinishReason::Other("malformed_function_call".into()))])
            .await
            .unwrap_err();
        assert_eq!(err.kind, "model_decode");
        assert!(err.retryable);
        let out = completed(
            run(vec![
                ChatDelta::Warning("dropped `format`".into()),
                ChatDelta::Text("ok".into()),
                ChatDelta::Finish(crate::types::FinishReason::Other("language".into())),
            ])
            .await,
        );
        let warnings = out.metadata["warnings"].to_text();
        assert!(warnings.contains("dropped `format`") && warnings.contains("language"), "{warnings}");
    }

    #[tokio::test]
    async fn streams_and_outputs() {
        let model = Arc::new(MockModel::new(vec![vec![
            ChatDelta::Reasoning("why".into()),
            ChatDelta::Text("Hel".into()),
            ChatDelta::Text("lo".into()),
            ChatDelta::Finish(crate::types::FinishReason::Stop),
        ]]));
        let mut pool = VariablePool::new();
        pool.insert("sys", "query", Value::from("hi there"));
        let node = LlmNode::from_doc(&doc(serde_json::json!({
            "model": {"provider": "langgenius/openai/openai", "name": "gpt", "completion_params": {"temperature": 0.2}},
            "prompt_template": [{"role": "system", "text": "be nice"}, {"role": "user", "text": "{{#sys.query#}}"}]
        })))
        .unwrap();
        let (ctx, events) = ctx_with(Arc::clone(&model), pool);
        let out = completed(node.run(ctx).await);
        assert_eq!(out.outputs["text"], Value::from("Hello"));
        assert_eq!(out.outputs["reasoning"], Value::from("why"));
        assert_eq!(out.outputs["finish_reason"], Value::from("stop"));
        let chunks: Vec<(String, String)> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                Event::NodeChunk { field, chunk, .. } => Some((field.to_string(), chunk.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].0, "reasoning");
        let req = &model.requests()[0];
        assert_eq!(req.model, "gpt");
        assert_eq!(req.temperature, Some(0.2));
        assert_eq!(req.messages[0].role, Role::System);
        assert!(matches!(&req.messages[1].content[0], ContentPart::Text { text } if text == "hi there"));
    }

    #[tokio::test]
    async fn structured_output_and_separated_reasoning() {
        let model = Arc::new(MockModel::new(vec![MockModel::text("<think>plan</think>{\"n\": 3}")]));
        let node = LlmNode::from_doc(&doc(serde_json::json!({
            "model": {"provider": "openai", "name": "gpt"},
            "prompt_template": [{"role": "user", "text": "give n"}],
            "structured_output_switch_on": true,
            "structured_output": {"schema": {"type": "object", "properties": {"n": {"type": "integer"}}}},
            "reasoning_format": "separated"
        })))
        .unwrap();
        let (ctx, _) = ctx_with(Arc::clone(&model), VariablePool::new());
        let out = completed(node.run(ctx).await);
        assert_eq!(out.outputs["structured_output"].get_path(&["n"]), Some(Value::Int(3)));
        assert_eq!(out.outputs["reasoning"], Value::from("plan"));
        assert!(matches!(model.requests()[0].response_format, Some(ResponseFormat::JsonSchema { .. })));
    }

    #[tokio::test]
    async fn separated_reasoning_streams_without_think_tags() {
        let model = Arc::new(MockModel::new(vec![vec![
            ChatDelta::Text("<thi".into()),
            ChatDelta::Text("nk>plan</th".into()),
            ChatDelta::Text("ink>\nHel".into()),
            ChatDelta::Text("lo".into()),
            ChatDelta::Finish(crate::types::FinishReason::Stop),
        ]]));
        let node = LlmNode::from_doc(&doc(serde_json::json!({
            "model": {"provider": "openai", "name": "gpt"},
            "prompt_template": [{"role": "user", "text": "hi"}],
            "reasoning_format": "separated"
        })))
        .unwrap();
        let (ctx, events) = ctx_with(model, VariablePool::new());
        let out = completed(node.run(ctx).await);
        let mut streamed = String::new();
        let mut reasoning = String::new();
        for e in events.lock().unwrap().iter() {
            if let Event::NodeChunk { field, chunk, .. } = e {
                match field.as_ref() {
                    "text" => streamed.push_str(chunk),
                    _ => reasoning.push_str(chunk),
                }
            }
        }
        assert_eq!(streamed, "Hello");
        assert_eq!(out.outputs["text"], Value::from(streamed.as_str()));
        assert_eq!(reasoning, "plan");
    }

    #[tokio::test]
    async fn structured_output_is_validated() {
        let model = Arc::new(MockModel::new(vec![MockModel::text("see [1] and {\"m\": 1}")]));
        let node = LlmNode::from_doc(&doc(serde_json::json!({
            "model": {"provider": "openai", "name": "gpt"},
            "prompt_template": [{"role": "user", "text": "give n"}],
            "structured_output": {"schema": {"type": "object", "required": ["n"], "properties": {"n": {"type": "integer"}}}}
        })))
        .unwrap();
        let (ctx, _) = ctx_with(model, VariablePool::new());
        let err = node.run(ctx).await.unwrap_err();
        assert_eq!(err.kind, "structured_output");
        assert!(err.message.contains("`n`"), "{}", err.message);
        assert!(err.retryable);
    }

    #[tokio::test]
    async fn invalid_completion_params_fail_the_node() {
        let model = Arc::new(MockModel::new(vec![MockModel::text("x")]));
        let node = LlmNode::from_doc(&doc(serde_json::json!({
            "model": {"provider": "openai", "name": "gpt", "completion_params": {"temperature": "abc"}},
            "prompt_template": [{"role": "user", "text": "hi"}]
        })))
        .unwrap();
        let (ctx, _) = ctx_with(Arc::clone(&model), VariablePool::new());
        let err = node.run(ctx).await.unwrap_err();
        assert_eq!(err.kind, "invalid_input");
        assert!(err.message.contains("temperature"));
        assert!(model.requests().is_empty());
    }

    #[test]
    fn inputs_cover_every_reference() {
        let node = LlmNode::from_doc(&doc(serde_json::json!({
            "model": {"provider": "openai", "name": "gpt", "completion_params": {"temperature": "{{#cfg.t#}}"}},
            "prompt_template": [{"role": "user", "text": "{{#start.q#}}"}],
            "context": {"enabled": true, "variable_selector": ["kb", "result"]},
            "vision": {"enabled": true, "configs": {"variable_selector": ["start", "files"]}},
            "memory": {"window": {"enabled": false}}
        })))
        .unwrap();
        let inputs: Vec<String> = node.inputs().iter().map(ToString::to_string).collect();
        for expected in ["start.q", "kb.result", "start.files", "sys.history", "sys.query", "cfg.t"] {
            assert!(inputs.iter().any(|i| i == expected), "missing {expected} in {inputs:?}");
        }
    }

    #[tokio::test]
    async fn model_errors_map_to_node_errors() {
        let model = Arc::new(MockModel::new(vec![]));
        model.push_error(crate::error::LlmError::Server { status: 503, message: "down".into() });
        let node = LlmNode::from_doc(&doc(serde_json::json!({
            "model": {"provider": "openai", "name": "gpt"},
            "prompt_template": {"text": "completion style"}
        })))
        .unwrap();
        let (ctx, _) = ctx_with(model, VariablePool::new());
        let err = node.run(ctx).await.unwrap_err();
        assert!(err.retryable);
        assert_eq!(err.kind, "model_server");
    }

    #[tokio::test]
    async fn missing_registry_is_a_fatal_error() {
        let node = LlmNode::from_doc(&doc(serde_json::json!({
            "model": {"provider": "openai", "name": "gpt"},
            "prompt_template": [{"role": "user", "text": "x"}]
        })))
        .unwrap();
        let err = node.run(NodeContext::for_test("llm", VariablePool::new())).await.unwrap_err();
        assert!(!err.retryable);
    }

    #[test]
    fn rejects_empty_prompt() {
        let err = LlmNode::from_doc(&doc(serde_json::json!({"model": {"provider": "p", "name": "m"}}))).unwrap_err();
        assert!(err.message.contains("prompt_template"));
    }
}
