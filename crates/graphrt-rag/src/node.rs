//! The `knowledge-retrieval` node.

use std::sync::Arc;

use futures::future::BoxFuture;
use graphrt_core::catalog::{NodeCategory, NodeDescriptor, OutputDescriptor};
use graphrt_core::dsl::NodeDoc;
use graphrt_core::node::{ConfigError, Node, NodeContext, NodeError, NodeOutput, NodeResult, Registry, parse_config};
use graphrt_core::template::Template;
use graphrt_core::{Selector, Value, ValueType};
use serde::Deserialize;

use crate::filter::MetadataFilter;
use crate::kb::KnowledgeBase;
use crate::types::{Hit, HybridWeights, RetrievalRequest};

/// Node kind.
pub const KIND: &str = "knowledge-retrieval";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    /// Use only the dataset whose best hit scores highest.
    Single,
    /// Merge hits from all datasets.
    #[default]
    Multiple,
}

#[derive(Clone, Debug, Deserialize)]
struct VectorSetting {
    #[serde(default)]
    vector_weight: f32,
}

#[derive(Clone, Debug, Deserialize)]
struct KeywordSetting {
    #[serde(default)]
    keyword_weight: f32,
}

/// Accepts `{vector, keyword}` or `{vector_setting: {vector_weight}, keyword_setting: {keyword_weight}}`.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum WeightsConfig {
    Flat { vector: f32, keyword: f32 },
    Nested { vector_setting: VectorSetting, keyword_setting: KeywordSetting },
}

impl WeightsConfig {
    fn to_weights(&self) -> HybridWeights {
        match self {
            WeightsConfig::Flat { vector, keyword } => HybridWeights { vector: *vector, keyword: *keyword },
            WeightsConfig::Nested { vector_setting, keyword_setting } => {
                HybridWeights { vector: vector_setting.vector_weight, keyword: keyword_setting.keyword_weight }
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct MultipleConfig {
    #[serde(default = "default_top_k")]
    top_k: usize,
    #[serde(default)]
    score_threshold: Option<f32>,
    #[serde(default)]
    reranking_enable: bool,
    /// `reranking_model` (use the dataset reranker) or `weighted_score` (hybrid weights only).
    #[serde(default)]
    reranking_mode: Option<String>,
    #[serde(default)]
    weights: Option<WeightsConfig>,
}

fn default_top_k() -> usize {
    4
}

impl Default for MultipleConfig {
    fn default() -> Self {
        Self {
            top_k: default_top_k(),
            score_threshold: None,
            reranking_enable: false,
            reranking_mode: None,
            weights: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct Config {
    query_variable_selector: Selector,
    #[serde(default)]
    dataset_ids: Vec<String>,
    #[serde(default)]
    retrieval_mode: Mode,
    #[serde(default)]
    multiple_retrieval_config: Option<MultipleConfig>,
    /// `disabled`, `manual` (use the conditions) or `automatic` (not supported; ignored).
    #[serde(default)]
    metadata_filtering_mode: Option<String>,
    #[serde(default)]
    metadata_filtering_conditions: Option<MetadataFilter>,
}

/// Retrieves chunks from a [`KnowledgeBase`] service.
///
/// Output `result` is an array of `{content, title, score, dataset_id,
/// document_id, id, metadata}` objects, ready for an LLM node's context.
#[derive(Debug)]
pub struct KnowledgeRetrievalNode {
    query: Selector,
    datasets: Vec<String>,
    mode: Mode,
    top_k: usize,
    threshold: Option<f32>,
    rerank: bool,
    weights: HybridWeights,
    filter: Option<MetadataFilter>,
}

impl KnowledgeRetrievalNode {
    /// Build from a node document.
    pub fn from_doc(doc: &NodeDoc) -> Result<Self, ConfigError> {
        let config: Config = parse_config(doc)?;
        if config.dataset_ids.is_empty() {
            return Err(ConfigError::new(doc, "`dataset_ids` must list at least one dataset"));
        }
        let multi = config.multiple_retrieval_config.unwrap_or_default();
        if multi.top_k == 0 {
            return Err(ConfigError::new(doc, "`top_k` must be at least 1"));
        }
        let weighted_only = multi.reranking_mode.as_deref() == Some("weighted_score");
        let weights = multi.weights.as_ref().map(WeightsConfig::to_weights).unwrap_or_default();
        if weights.vector < 0.0 || weights.keyword < 0.0 || weights.vector + weights.keyword <= 0.0 {
            return Err(ConfigError::new(doc, "hybrid weights must be non-negative and not both zero"));
        }
        let filter = match config.metadata_filtering_mode.as_deref() {
            Some("disabled") => None,
            None | Some("manual") => config.metadata_filtering_conditions.filter(|f| !f.is_empty()),
            Some("automatic") => {
                return Err(ConfigError::new(
                    doc,
                    "`metadata_filtering_mode: automatic` is not supported; use `manual`",
                ));
            }
            Some(other) => {
                return Err(ConfigError::new(
                    doc,
                    format!("unknown `metadata_filtering_mode` `{other}` (expected disabled or manual)"),
                ));
            }
        };
        if let Some(f) = &filter {
            f.validate().map_err(|e| ConfigError::new(doc, e.to_string()))?;
        }
        Ok(Self {
            query: config.query_variable_selector,
            datasets: config.dataset_ids,
            mode: config.retrieval_mode,
            top_k: multi.top_k.min(100),
            threshold: multi.score_threshold,
            rerank: multi.reranking_enable && !weighted_only,
            weights,
            filter,
        })
    }

    /// Resolve `{{#...#}}` templates in string condition values.
    fn resolved_filter(&self, ctx: &NodeContext) -> Option<MetadataFilter> {
        let mut filter = self.filter.clone()?;
        for c in &mut filter.conditions {
            if let serde_json::Value::String(s) = &c.value {
                let template = Template::parse(s);
                if !template.is_static() {
                    c.value = serde_json::Value::String(template.render(&ctx.pool));
                }
            }
        }
        Some(filter)
    }

    async fn execute(&self, ctx: NodeContext) -> NodeResult {
        // A missing variable is a wiring mistake; an empty value is a legitimate empty query.
        let query = ctx
            .get(&self.query)
            .ok_or_else(|| NodeError::invalid_input(format!("query variable `{}` is not set", self.query)))?
            .to_text();
        if query.trim().is_empty() {
            return Ok(NodeOutput::new().with("result", Value::array([])).meta("skipped", "empty query").into());
        }
        let kb = ctx.require_service::<KnowledgeBase>("knowledge base")?;
        for id in &self.datasets {
            if !kb.contains(id) {
                return Err(NodeError::fatal("invalid_input", format!("unknown dataset `{id}`")));
            }
        }
        // Content-capturing hosts (OpenTelemetry `retrieval` spans) also record the query.
        let reported_query = ctx.captures_content().then(|| query.clone());
        let request = RetrievalRequest::new(query, self.datasets.clone())
            .top_k(self.top_k)
            .threshold(self.threshold)
            .filter(self.resolved_filter(&ctx))
            .weights(self.weights)
            .rerank(self.rerank);
        let hits = match self.mode {
            Mode::Multiple => kb.retrieve(&request).await,
            Mode::Single => kb.retrieve_single(&request).await,
        }
        .map_err(NodeError::from)?;

        let count = hits.len();
        let result = Value::array(hits.into_iter().map(hit_value));
        let mut output = NodeOutput::new()
            .with("result", result)
            .meta("datasets", Value::array(self.datasets.iter().map(|d| Value::from(d.as_str()))))
            .meta("hits", i64::try_from(count).unwrap_or(i64::MAX));
        if let Some(query) = reported_query {
            output = output.meta("query", query);
        }
        Ok(output.into())
    }
}

fn hit_value(hit: Hit) -> Value {
    Value::object([
        ("content", Value::from(hit.content)),
        ("title", Value::from(hit.title)),
        ("score", Value::Float(f64::from(hit.score))),
        ("dataset_id", Value::from(hit.dataset_id)),
        ("document_id", Value::from(hit.document_id)),
        ("id", Value::from(hit.id)),
        // Metadata comes from external stores: never let it forge file values.
        ("metadata", Value::from_json(serde_json::Value::Object(hit.metadata))),
    ])
}

impl Node for KnowledgeRetrievalNode {
    fn run(&self, ctx: NodeContext) -> BoxFuture<'_, NodeResult> {
        Box::pin(self.execute(ctx))
    }

    fn inputs(&self) -> Vec<Selector> {
        let mut out = vec![self.query.clone()];
        if let Some(filter) = &self.filter {
            for c in &filter.conditions {
                if let serde_json::Value::String(s) = &c.value {
                    out.extend(Template::parse(s).selectors().cloned());
                }
            }
        }
        out
    }
}

/// Catalog entry for the node.
pub fn descriptor() -> NodeDescriptor {
    NodeDescriptor::new(KIND, "Knowledge Retrieval", NodeCategory::Knowledge)
        .describe("Search knowledge datasets (vector, keyword or hybrid, with optional reranking) and return matching chunks.")
        .icon("database")
        .source("graphrt-rag")
        .schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["query_variable_selector", "dataset_ids"],
            "properties": {
                "query_variable_selector": {
                    "description": "Variable holding the search query, e.g. [\"sys\", \"query\"].",
                    "oneOf": [{"type": "array", "items": {"type": "string"}, "minItems": 2}, {"type": "string"}]
                },
                "dataset_ids": {"type": "array", "items": {"type": "string"}, "minItems": 1},
                "retrieval_mode": {"type": "string", "enum": ["single", "multiple"], "default": "multiple"},
                "multiple_retrieval_config": {
                    "type": "object",
                    "properties": {
                        "top_k": {"type": "integer", "minimum": 1, "maximum": 100, "default": 4},
                        "score_threshold": {"type": ["number", "null"]},
                        "reranking_enable": {"type": "boolean", "default": false},
                        "reranking_mode": {"type": "string", "enum": ["reranking_model", "weighted_score"]},
                        "weights": {
                            "type": "object",
                            "description": "Hybrid weights: {vector, keyword} or {vector_setting: {vector_weight}, keyword_setting: {keyword_weight}}."
                        }
                    }
                },
                "metadata_filtering_mode": {"type": "string", "enum": ["disabled", "manual"]},
                "metadata_filtering_conditions": {
                    "type": "object",
                    "properties": {
                        "logical_operator": {"type": "string", "enum": ["and", "or"]},
                        "conditions": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "required": ["name", "comparison_operator"],
                                "properties": {
                                    "name": {"type": "string"},
                                    "comparison_operator": {"type": "string"},
                                    "value": {}
                                }
                            }
                        }
                    }
                }
            }
        }))
        .output(
            OutputDescriptor::new("result", ValueType::ArrayObject)
                .describe("Matching chunks: {content, title, score, dataset_id, document_id, id, metadata}."),
        )
}

/// Register the `knowledge-retrieval` node.
pub fn register(registry: &mut Registry) {
    registry.register_lazily_described(KIND, descriptor, |doc| {
        let node: Arc<dyn Node> = Arc::new(KnowledgeRetrievalNode::from_doc(doc)?);
        Ok(node)
    });
}
