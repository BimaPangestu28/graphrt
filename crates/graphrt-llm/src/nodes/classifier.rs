//! `question-classifier` node: routes to the branch of the class chosen by a model.
//!
//! The selected class id is the branch handle. Outputs: `class_id`,
//! `class_name`, `class_label`, `usage`. An answer that names no configured
//! class fails the node, unless `fallback_class` names the class to use; a
//! fallback is marked with metadata `fallback: true` and `raw_answer`.

use std::sync::Arc;

use futures::future::BoxFuture;
use graphrt_core::dsl::NodeDoc;
use graphrt_core::node::{ConfigError, NodeResult, NodeRole, parse_config};
use graphrt_core::template::Template;
use graphrt_core::{Node, NodeContext, NodeError, NodeOutput, Selector};
use serde::Deserialize;
use serde_json::Value as Json;

use super::common::{
    MemoryConfig, ModelConfig, StreamMode, VisionConfig, extract_json, invoke, model_for, usage_value,
};
use crate::types::{ContentPart, Message};

/// One class.
#[derive(Clone, Debug, Deserialize)]
pub struct ClassConfig {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub label: Option<String>,
}

/// Configuration of the `question-classifier` node.
#[derive(Clone, Debug, Deserialize)]
pub struct ClassifierConfig {
    pub model: ModelConfig,
    #[serde(alias = "query")]
    pub query_variable_selector: Selector,
    pub classes: Vec<ClassConfig>,
    #[serde(default)]
    pub instruction: Option<Template>,
    #[serde(default)]
    pub memory: Option<MemoryConfig>,
    #[serde(default)]
    pub vision: VisionConfig,
    /// Class id used when the model's answer matches no class. Without it, such answers fail the node.
    #[serde(default)]
    pub fallback_class: Option<String>,
}

/// The `question-classifier` node.
#[derive(Debug)]
pub struct ClassifierNode {
    config: ClassifierConfig,
}

impl ClassifierNode {
    pub fn from_doc(doc: &NodeDoc) -> Result<Self, ConfigError> {
        let config: ClassifierConfig = parse_config(doc)?;
        if config.classes.is_empty() {
            return Err(ConfigError::new(doc, "classes must not be empty"));
        }
        let mut seen = std::collections::HashSet::new();
        for class in &config.classes {
            if class.id.is_empty() || !seen.insert(class.id.as_str()) {
                return Err(ConfigError::new(doc, format!("class ids must be unique and non-empty (`{}`)", class.id)));
            }
        }
        if let Some(fallback) = &config.fallback_class {
            if !config.classes.iter().any(|c| &c.id == fallback) {
                return Err(ConfigError::new(doc, format!("fallback_class `{fallback}` is not a configured class")));
            }
        }
        Ok(Self { config })
    }

    fn system_prompt(&self, instruction: &str) -> String {
        let categories: Vec<Json> = self
            .config
            .classes
            .iter()
            .map(|c| serde_json::json!({"category_id": c.id, "category_name": c.name}))
            .collect();
        let mut prompt = String::from(
            "You are a text classification engine. Classify the user's input into exactly one of these categories:\n",
        );
        prompt.push_str(&Json::Array(categories).to_string());
        if !instruction.trim().is_empty() {
            prompt.push_str("\n\nAdditional instructions:\n");
            prompt.push_str(instruction.trim());
        }
        prompt.push_str(
            "\n\nRespond only with a JSON object of the form {\"category_id\": \"<id>\", \"category_name\": \"<name>\"}.",
        );
        prompt
    }

    /// Map a model answer to a configured class index.
    fn match_class(&self, answer: &str) -> Option<usize> {
        let classes = &self.config.classes;
        let by_id = |id: &str| classes.iter().position(|c| c.id == id.trim());
        let by_name = |name: &str| classes.iter().position(|c| c.name.trim().eq_ignore_ascii_case(name.trim()));
        if let Some(json) = extract_json(answer) {
            let field = |key: &str| match json.get(key) {
                Some(Json::String(s)) => Some(s.clone()),
                Some(Json::Number(n)) => Some(n.to_string()),
                _ => None,
            };
            if let Some(i) = field("category_id").as_deref().and_then(by_id) {
                return Some(i);
            }
            if let Some(i) = field("category_name").as_deref().and_then(by_name) {
                return Some(i);
            }
        }
        by_id(answer).or_else(|| by_name(answer))
    }

    async fn execute(&self, ctx: NodeContext) -> NodeResult {
        let cfg = &self.config;
        let pool = &ctx.pool;
        let query = pool.get(&cfg.query_variable_selector).map(|v| v.to_text()).ok_or_else(|| {
            NodeError::invalid_input(format!("query variable `{}` is missing", cfg.query_variable_selector))
        })?;
        let instruction = cfg.instruction.as_ref().map(|t| t.render(pool)).unwrap_or_default();

        let mut messages = vec![Message::system(self.system_prompt(&instruction))];
        if let Some(memory) = &cfg.memory {
            messages.extend(memory.history(pool)?);
        }
        let mut user = Message::user(query);
        user.content
            .extend(cfg.vision.image_parts(pool)?.into_iter().filter(|p| matches!(p, ContentPart::Image { .. })));
        messages.push(user);

        let model = model_for(&ctx, &cfg.model)?;
        let request = cfg.model.request(messages, pool)?;
        let response = invoke(&ctx, model.as_ref(), request, StreamMode::Off).await?;

        let (index, fallback) = match self.match_class(&response.text) {
            Some(i) => (i, false),
            None => match &cfg.fallback_class {
                Some(fallback) => {
                    let i = cfg.classes.iter().position(|c| &c.id == fallback).ok_or_else(|| {
                        NodeError::fatal("classification", format!("fallback class `{fallback}` is not configured"))
                    })?;
                    (i, true)
                }
                None => {
                    return Err(NodeError::new(
                        "classification",
                        format!("model answer matched no configured class: {}", response.text.trim()),
                    ));
                }
            },
        };
        let class =
            cfg.classes.get(index).ok_or_else(|| NodeError::fatal("classification", "class index out of range"))?;
        let label = class.label.clone().filter(|l| !l.is_empty()).unwrap_or_else(|| format!("CLASS {}", index + 1));
        let mut output = NodeOutput::new()
            .with("class_id", class.id.as_str())
            .with("class_name", class.name.as_str())
            .with("class_label", label)
            .with("usage", usage_value(&response.usage))
            .usage(response.usage)
            .branch(class.id.clone());
        if fallback {
            output = output.meta("fallback", true).meta("raw_answer", response.text.trim());
        }
        Ok(output.into())
    }
}

impl Node for ClassifierNode {
    fn role(&self) -> NodeRole {
        NodeRole::Branch
    }

    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(self.execute(ctx))
    }

    fn inputs(&self) -> Vec<Selector> {
        let cfg = &self.config;
        let mut out = vec![cfg.query_variable_selector.clone()];
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
    Ok(Arc::new(ClassifierNode::from_doc(doc)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockModel;
    use crate::registry::ModelRegistry;
    use graphrt_core::value::Value;
    use graphrt_core::{NodeOutcome, Services, VariablePool};

    fn node(extra: Json) -> ClassifierNode {
        let mut config = serde_json::json!({
            "model": {"provider": "openai", "name": "gpt"},
            "query_variable_selector": ["sys", "query"],
            "classes": [{"id": "1", "name": "billing"}, {"id": "2", "name": "support", "label": "Help"}],
        });
        if let (Some(c), Json::Object(e)) = (config.as_object_mut(), extra) {
            c.extend(e);
        }
        ClassifierNode::from_doc(&NodeDoc::new("qc", "question-classifier", config)).unwrap()
    }

    async fn classify(node: &ClassifierNode, answer: &str) -> NodeResult {
        let model = Arc::new(MockModel::new(vec![MockModel::text(answer)]));
        let mut pool = VariablePool::new();
        pool.insert("sys", "query", Value::from("my card was charged twice"));
        let ctx = NodeContext::for_test("qc", pool)
            .with_services(Arc::new(Services::new().with(ModelRegistry::new().with("openai", model))));
        node.run(ctx).await
    }

    fn output(r: NodeResult) -> NodeOutput {
        match r.unwrap() {
            NodeOutcome::Completed(o) => o,
            NodeOutcome::Paused(_) => panic!("paused"),
        }
    }

    #[tokio::test]
    async fn routes_by_id_and_name() {
        let n = node(serde_json::json!({}));
        let out = output(classify(&n, "```json\n{\"category_id\": \"2\", \"category_name\": \"support\"}\n```").await);
        assert_eq!(out.branch.as_deref(), Some("2"));
        assert_eq!(out.outputs["class_label"], Value::from("Help"));
        let out = output(classify(&n, "{\"category_name\": \"Billing\"}").await);
        assert_eq!(out.branch.as_deref(), Some("1"));
        assert_eq!(out.outputs["class_label"], Value::from("CLASS 1"));
    }

    #[tokio::test]
    async fn unknown_class_fails_unless_fallback() {
        let n = node(serde_json::json!({}));
        let err = classify(&n, "{\"category_id\": \"9\"}").await.unwrap_err();
        assert_eq!(err.kind, "classification");
        let n = node(serde_json::json!({"fallback_class": "2"}));
        let out = output(classify(&n, "gibberish").await);
        assert_eq!(out.branch.as_deref(), Some("2"));
        assert_eq!(out.metadata.get("fallback"), Some(&Value::Bool(true)));
        assert_eq!(out.metadata.get("raw_answer"), Some(&Value::from("gibberish")));
        let out = output(classify(&n, "{\"category_id\": \"1\"}").await);
        assert!(!out.metadata.contains_key("fallback"));
    }

    #[test]
    fn config_validation() {
        let bad = NodeDoc::new(
            "qc",
            "question-classifier",
            serde_json::json!({"model": {"provider": "p", "name": "m"}, "query_variable_selector": "sys.query", "classes": []}),
        );
        assert!(ClassifierNode::from_doc(&bad).is_err());
        let bad = NodeDoc::new(
            "qc",
            "question-classifier",
            serde_json::json!({"model": {"provider": "p", "name": "m"}, "query_variable_selector": "sys.query",
                "classes": [{"id": "a", "name": "x"}], "fallback_class": "zzz"}),
        );
        assert!(ClassifierNode::from_doc(&bad).is_err());
    }
}
