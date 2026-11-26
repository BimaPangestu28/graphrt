//! `question-classifier` node: routes to the branch of the class chosen by a model.
//!
//! The selected class id is the branch handle. Outputs: `class_id`,
//! `class_name`, `class_label`, `usage`. An answer that names no configured
//! class fails the node, unless `fallback_class` names the class to use; a
//! fallback is marked with metadata `fallback: true` and `raw_answer`.
//!
//! With `model.provider: typesafe` (or `engine: typesafe`) the node asks
//! TypeSafe's System One API one choice question instead of prompting a chat
//! model. That path adds the `probabilities` (class id -> probability) and
//! `confidence` outputs and honours `min_confidence`, `min_probability` and the
//! optional `companion_noul` check; a decision below a threshold routes to
//! `fallback_class`, or fails with the non-retryable `low_confidence` error.

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
use crate::types::Message;

/// One class.
#[derive(Clone, Debug, Deserialize)]
pub struct ClassConfig {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub label: Option<String>,
    /// When this class applies (System One engine: used as the option description).
    #[serde(default)]
    pub description: Option<String>,
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
    /// `chat` (default) or `typesafe`. `model.provider: typesafe` also selects the System One engine.
    #[serde(default)]
    pub engine: Option<String>,
    /// System One engine: minimum confidence of the decision (0..1).
    #[serde(default)]
    pub min_confidence: Option<f64>,
    /// System One engine: minimum probability of the chosen class (0..1).
    #[serde(default)]
    pub min_probability: Option<f64>,
    /// System One engine: also ask whether any class fits at all (a yes/no question).
    #[serde(default)]
    pub companion_noul: bool,
    /// System One engine: minimum probability of "some class fits" when `companion_noul` is on.
    #[serde(default = "default_companion_threshold")]
    pub companion_threshold: f64,
}

fn default_companion_threshold() -> f64 {
    0.5
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
        if let Some(fallback) = &config.fallback_class
            && !config.classes.iter().any(|c| &c.id == fallback)
        {
            return Err(ConfigError::new(doc, format!("fallback_class `{fallback}` is not a configured class")));
        }
        match config.engine.as_deref() {
            None | Some("chat" | "typesafe") => {}
            Some(other) => {
                return Err(ConfigError::new(doc, format!("unknown engine `{other}` (expected `chat` or `typesafe`)")));
            }
        }
        for (name, value) in [
            ("min_confidence", config.min_confidence),
            ("min_probability", config.min_probability),
            ("companion_threshold", Some(config.companion_threshold)),
        ] {
            if let Some(v) = value
                && !(0.0..=1.0).contains(&v)
            {
                return Err(ConfigError::new(doc, format!("{name} must be between 0 and 1")));
            }
        }
        if config.engine.as_deref() == Some("chat") && config.model.provider == "typesafe" {
            return Err(ConfigError::new(
                doc,
                "`engine: chat` conflicts with `model.provider: typesafe` (TypeSafe is not a chat model)",
            ));
        }
        let node = Self { config };
        if node.uses_system_one() {
            if node.config.classes.len() > 255 {
                return Err(ConfigError::new(doc, "the typesafe engine supports at most 255 classes"));
            }
            // Chat-only settings would be silently ignored by the System One engine.
            let unsupported = [
                ("memory", node.config.memory.is_some()),
                ("vision", node.config.vision.enabled),
                ("model.completion_params", !node.config.model.completion_params.is_empty()),
            ];
            if let Some((name, _)) = unsupported.iter().find(|(_, set)| *set) {
                return Err(ConfigError::new(
                    doc,
                    format!("`{name}` is not supported by the typesafe engine (it only applies to chat models)"),
                ));
            }
        } else if node.config.min_confidence.is_some()
            || node.config.min_probability.is_some()
            || node.config.companion_noul
        {
            return Err(ConfigError::new(
                doc,
                "min_confidence, min_probability and companion_noul need the typesafe engine",
            ));
        }
        Ok(node)
    }

    /// Whether this node uses TypeSafe's System One API instead of a chat model.
    pub fn uses_system_one(&self) -> bool {
        self.config.engine.as_deref() == Some("typesafe") || self.config.model.provider == "typesafe"
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
        if self.uses_system_one() {
            return self.execute_system_one(ctx).await;
        }
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
        #[cfg(feature = "vision-preparer")]
        user.content.extend(cfg.vision.prepared_image_parts(&ctx).await?);
        #[cfg(not(feature = "vision-preparer"))]
        user.content.extend(cfg.vision.image_parts(pool)?);
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

#[cfg(not(feature = "typesafe"))]
impl ClassifierNode {
    async fn execute_system_one(&self, _ctx: NodeContext) -> NodeResult {
        Err(NodeError::fatal("missing_feature", "this build has no TypeSafe support (feature `typesafe`)"))
    }
}

#[cfg(feature = "typesafe")]
impl ClassifierNode {
    const CHOICE: &'static str = "category";
    const FITS: &'static str = "fits_any_category";

    /// Build the System One request: one choice question (plus the companion yes/no question).
    fn system_one_request(&self, query: String, instruction: &str) -> crate::typesafe::SystemOneRequest {
        use crate::typesafe::{DEFAULT_MODEL, NoulCriteria, Question, SystemOneRequest};
        let cfg = &self.config;
        let criteria: indexmap::IndexMap<String, Json> = cfg
            .classes
            .iter()
            .map(|c| {
                let text = match c.description.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
                    Some(d) => format!("{}: {d}", c.name),
                    None => c.name.clone(),
                };
                (c.id.clone(), Json::String(text))
            })
            .collect();
        let mut instructions = String::from("Which category does `state` belong to?");
        if !instruction.trim().is_empty() {
            instructions.push('\n');
            instructions.push_str(instruction.trim());
        }
        let model = if cfg.model.name.trim().is_empty() { DEFAULT_MODEL } else { cfg.model.name.as_str() };
        let mut request = SystemOneRequest::new(model, Json::String(query))
            .question(Self::CHOICE, Question::choice(instructions, criteria));
        if cfg.companion_noul {
            let names: Vec<&str> = cfg.classes.iter().map(|c| c.name.as_str()).collect();
            let statement = format!("`state` clearly fits one of these categories: {}.", names.join(", "));
            let fits = NoulCriteria {
                yes: Some(Json::from("The message is about one of the listed categories.")),
                no: Some(Json::from("The message is about something else, or none of the categories apply.")),
            };
            request = request.question(Self::FITS, Question::noul_with(statement, fits));
        }
        request
    }

    async fn execute_system_one(&self, ctx: NodeContext) -> NodeResult {
        use crate::typesafe::{Answer, SystemOneClient};
        let cfg = &self.config;
        let pool = &ctx.pool;
        let query = pool.get(&cfg.query_variable_selector).map(|v| v.to_text()).ok_or_else(|| {
            NodeError::invalid_input(format!("query variable `{}` is missing", cfg.query_variable_selector))
        })?;
        let instruction = cfg.instruction.as_ref().map(|t| t.render(pool)).unwrap_or_default();
        let client =
            ctx.require_service::<Arc<SystemOneClient>>("TypeSafe client (graphrt_llm::typesafe::SystemOneClient)")?;
        let request = self.system_one_request(query, &instruction);
        let response = client.ask(&request).await?;

        let Some(Answer::Choice { choice, confidence, probabilities }) = response.answer(Self::CHOICE) else {
            return Err(NodeError::new("model_decode", "System One returned no choice answer"));
        };
        let chosen =
            cfg.classes.iter().position(|c| &c.id == choice).ok_or_else(|| {
                NodeError::new("classification", format!("System One chose unknown class `{choice}`"))
            })?;
        let probability = probabilities.get(choice).copied().ok_or_else(|| {
            NodeError::new("model_decode", format!("System One gave no probability for the chosen class `{choice}`"))
        })?;
        let fits = match response.answer(Self::FITS) {
            Some(Answer::Noul { noul }) => Some(*noul),
            _ => None,
        };

        let mut reasons: Vec<String> = Vec::new();
        if let Some(min) = cfg.min_confidence
            && *confidence < min
        {
            reasons.push(format!("confidence {confidence:.3} < {min}"));
        }
        if let Some(min) = cfg.min_probability
            && probability < min
        {
            reasons.push(format!("probability {probability:.3} < {min}"));
        }
        if cfg.companion_noul {
            match fits {
                Some(p) if p < cfg.companion_threshold => {
                    reasons.push(format!("fits-any probability {p:.3} < {}", cfg.companion_threshold));
                }
                Some(_) => {}
                None => return Err(NodeError::new("model_decode", "System One returned no companion answer")),
            }
        }

        let (index, low_confidence) = if reasons.is_empty() {
            (chosen, false)
        } else {
            match &cfg.fallback_class {
                Some(fallback) => {
                    let i = cfg.classes.iter().position(|c| &c.id == fallback).ok_or_else(|| {
                        NodeError::fatal("classification", format!("fallback class `{fallback}` is not configured"))
                    })?;
                    (i, true)
                }
                None => {
                    return Err(NodeError::fatal(
                        "low_confidence",
                        format!("classification below threshold: {}", reasons.join(", ")),
                    ));
                }
            }
        };

        let class =
            cfg.classes.get(index).ok_or_else(|| NodeError::fatal("classification", "class index out of range"))?;
        let label = class.label.clone().filter(|l| !l.is_empty()).unwrap_or_else(|| format!("CLASS {}", index + 1));
        let usage = response.usage.to_usage();
        let probability_map: graphrt_core::value::Map =
            probabilities.iter().map(|(k, v)| (k.clone(), graphrt_core::Value::Float(*v))).collect();
        let mut output = NodeOutput::new()
            .with("class_id", class.id.as_str())
            .with("class_name", class.name.as_str())
            .with("class_label", label)
            .with("probabilities", graphrt_core::Value::from(probability_map))
            .with("confidence", *confidence)
            .with("usage", usage_value(&usage))
            .usage(usage)
            .branch(class.id.clone())
            .meta("engine", "typesafe")
            .meta("model", response.model.as_str());
        if let Some(p) = fits {
            output = output.meta("fits_any_probability", p);
        }
        if low_confidence {
            output = output
                .meta("fallback", true)
                .meta("low_confidence", true)
                .meta("predicted_class", choice.as_str())
                .meta("reasons", reasons.join(", "));
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

    fn branch_handles(&self) -> Option<Vec<String>> {
        Some(self.config.classes.iter().map(|c| c.id.clone()).collect())
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

    #[test]
    fn typesafe_engine_rejects_chat_only_settings() {
        let doc = |extra: Json| {
            let mut config = serde_json::json!({
                "model": {"provider": "typesafe", "name": "jev-latest"},
                "query_variable_selector": "sys.query",
                "classes": [{"id": "a", "name": "x"}],
            });
            if let (Some(c), Json::Object(e)) = (config.as_object_mut(), extra) {
                c.extend(e);
            }
            NodeDoc::new("qc", "question-classifier", config)
        };
        assert!(ClassifierNode::from_doc(&doc(serde_json::json!({}))).is_ok());
        for bad in [
            serde_json::json!({"engine": "chat"}),
            serde_json::json!({"memory": {"window": {"enabled": true, "size": 3}}}),
            serde_json::json!({"vision": {"enabled": true}}),
            serde_json::json!({"model": {"provider": "typesafe", "name": "jev", "completion_params": {"temperature": 0}}}),
        ] {
            assert!(ClassifierNode::from_doc(&doc(bad.clone())).is_err(), "{bad}");
        }
    }
}
