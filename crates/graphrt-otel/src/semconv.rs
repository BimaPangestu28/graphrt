//! Every OpenTelemetry attribute, span and event name graphrt emits for GenAI
//! semantic conventions and vendor dialects, in one place.
//!
//! The GenAI conventions are still "Development" (they moved to
//! `open-telemetry/semantic-conventions-genai`, which has no tagged release yet)
//! and renames keep landing. Keeping the names here means a rename is a one-line
//! change plus, for one transition release, a dual-emit like
//! [`gen_ai::SYSTEM`] next to [`gen_ai::PROVIDER_NAME`].
//!
//! Target: the `semantic-conventions-genai` main branch as of 2026-11.

/// `gen_ai.*` attribute names.
pub mod gen_ai {
    pub const OPERATION_NAME: &str = "gen_ai.operation.name";
    pub const PROVIDER_NAME: &str = "gen_ai.provider.name";
    /// Deprecated, replaced by [`PROVIDER_NAME`]. Still emitted (with the raw
    /// provider name) for one transition release.
    pub const SYSTEM: &str = "gen_ai.system";
    pub const REQUEST_MODEL: &str = "gen_ai.request.model";
    pub const USAGE_INPUT_TOKENS: &str = "gen_ai.usage.input_tokens";
    pub const USAGE_OUTPUT_TOKENS: &str = "gen_ai.usage.output_tokens";
    pub const RESPONSE_FINISH_REASONS: &str = "gen_ai.response.finish_reasons";
    pub const AGENT_NAME: &str = "gen_ai.agent.name";
    pub const CONVERSATION_ID: &str = "gen_ai.conversation.id";
    pub const DATA_SOURCE_ID: &str = "gen_ai.data_source.id";
    pub const TOOL_NAME: &str = "gen_ai.tool.name";
    pub const TOOL_CALL_ID: &str = "gen_ai.tool.call.id";
    pub const TOOL_TYPE: &str = "gen_ai.tool.type";
    /// Opt-in content.
    pub const TOOL_CALL_ARGUMENTS: &str = "gen_ai.tool.call.arguments";
    /// Opt-in content.
    pub const TOOL_CALL_RESULT: &str = "gen_ai.tool.call.result";
    pub const RETRIEVAL_TOP_K: &str = "gen_ai.retrieval.top_k";
    /// Opt-in content.
    pub const RETRIEVAL_QUERY_TEXT: &str = "gen_ai.retrieval.query.text";
    /// Opt-in content.
    pub const RETRIEVAL_DOCUMENTS: &str = "gen_ai.retrieval.documents";
    /// Opt-in content (structured `AnyValue`).
    pub const INPUT_MESSAGES: &str = "gen_ai.input.messages";
    /// Opt-in content (structured `AnyValue`).
    pub const OUTPUT_MESSAGES: &str = "gen_ai.output.messages";
    /// Opt-in content (structured `AnyValue`).
    pub const SYSTEM_INSTRUCTIONS: &str = "gen_ai.system_instructions";
    /// Evaluation result ([`super::EVALUATION_RESULT_EVENT`]): the evaluator's name.
    pub const EVALUATION_NAME: &str = "gen_ai.evaluation.name";
    /// Evaluation result: the numeric score (double).
    pub const EVALUATION_SCORE_VALUE: &str = "gen_ai.evaluation.score.value";
    /// Evaluation result: a low-cardinality label (`pass`, `fail`...).
    pub const EVALUATION_SCORE_LABEL: &str = "gen_ai.evaluation.score.label";
    /// Evaluation result: why the evaluator decided so.
    pub const EVALUATION_EXPLANATION: &str = "gen_ai.evaluation.explanation";
}

/// Well-known `gen_ai.operation.name` values used by graphrt.
pub mod operation {
    pub const CHAT: &str = "chat";
    pub const INVOKE_AGENT: &str = "invoke_agent";
    pub const INVOKE_WORKFLOW: &str = "invoke_workflow";
    pub const EXECUTE_TOOL: &str = "execute_tool";
    pub const RETRIEVAL: &str = "retrieval";
}

/// Span event carrying content when capture is `EVENT_ONLY` / `SPAN_AND_EVENT`.
pub const CONTENT_EVENT: &str = "gen_ai.client.inference.operation.details";

/// Event carrying an evaluation result, parented to the evaluated operation.
/// graphrt's exporter has no logs signal, so it is a span event (on an
/// `evaluate {name}` span in the evaluated run's trace).
pub const EVALUATION_RESULT_EVENT: &str = "gen_ai.evaluation.result";

/// Span name prefix of an evaluation: `evaluate {evaluator name}`.
pub const EVALUATE_SPAN: &str = "evaluate";

/// General (non-GenAI) semantic convention names.
pub mod general {
    pub const SESSION_ID: &str = "session.id";
    pub const USER_ID: &str = "user.id";
    pub const ERROR_TYPE: &str = "error.type";
    pub const EXCEPTION_TYPE: &str = "exception.type";
    pub const EXCEPTION_MESSAGE: &str = "exception.message";
    pub const ERROR_MESSAGE: &str = "error.message";
}

/// Langfuse (v4) attributes. Langfuse copies nothing from the root to other
/// observations, so trace-level ones go on every span.
pub mod langfuse {
    pub const SESSION_ID: &str = "langfuse.session.id";
    pub const USER_ID: &str = "langfuse.user.id";
    pub const TRACE_TAGS: &str = "langfuse.trace.tags";
    /// Prefix: one attribute per metadata key.
    pub const TRACE_METADATA_PREFIX: &str = "langfuse.trace.metadata.";
    /// Highest-precedence input of an observation (JSON text).
    pub const OBSERVATION_INPUT: &str = "langfuse.observation.input";
    /// Highest-precedence output of an observation (JSON text).
    pub const OBSERVATION_OUTPUT: &str = "langfuse.observation.output";
    /// Experiments over OTLP (one trace per dataset item, no enclosing span).
    /// On every span of the item's trace.
    pub const EXPERIMENT_ID: &str = "langfuse.experiment.id";
    pub const EXPERIMENT_NAME: &str = "langfuse.experiment.name";
    pub const EXPERIMENT_DATASET_ID: &str = "langfuse.experiment.dataset.id";
    /// Prefix: one attribute per experiment metadata key.
    pub const EXPERIMENT_METADATA_PREFIX: &str = "langfuse.experiment.metadata.";
    /// On the item's root span.
    pub const EXPERIMENT_DESCRIPTION: &str = "langfuse.experiment.description";
    pub const EXPERIMENT_ITEM_ID: &str = "langfuse.experiment.item.id";
    /// Must equal the root span's own span id.
    pub const EXPERIMENT_ITEM_ROOT_OBSERVATION_ID: &str = "langfuse.experiment.item.root_observation_id";
    /// JSON text.
    pub const EXPERIMENT_ITEM_EXPECTED_OUTPUT: &str = "langfuse.experiment.item.expected_output";
    /// Prefix: one attribute per item metadata key.
    pub const EXPERIMENT_ITEM_METADATA_PREFIX: &str = "langfuse.experiment.item.metadata.";
}

/// Comet Opik: the project of every span (as a resource attribute, which every
/// span carries; Opik does not inherit it from the root).
pub const OPIK_PROJECT_NAME: &str = "opik.project_name";

/// Arize Phoenix / OpenInference: the project spans are filed under.
pub const OPENINFERENCE_PROJECT_NAME: &str = "openinference.project.name";

/// graphrt's own attributes that sit next to GenAI ones.
pub mod graphrt {
    /// The precise graphrt operation where `gen_ai.operation.name` only has a
    /// generic well-known value (e.g. `classify` for `system-one`, reported as `chat`).
    pub const OPERATION_KIND: &str = "graphrt.operation.kind";
    pub const WORKFLOW_NAME: &str = "graphrt.workflow.name";
    pub const NODE_TITLE: &str = "graphrt.node.title";
    /// Run input (JSON text, opt-in content) on the root span.
    pub const RUN_INPUT: &str = "graphrt.run.input";
    /// Run output (JSON text, opt-in content) on the root span.
    pub const RUN_OUTPUT: &str = "graphrt.run.output";
    /// Prefix of caller metadata on the root span.
    pub const RUN_METADATA_PREFIX: &str = "graphrt.run.metadata.";
    pub const RUN_TAGS: &str = "graphrt.run.tags";
    /// Set on the run's root span; exporters never drop spans carrying it.
    pub const RUN_STATUS: &str = "graphrt.run.status";
    pub const RUN_ID: &str = "graphrt.run.id";
    /// Who produced an evaluation score: `online` (an evaluator of `serve`),
    /// `human` (feedback) or `offline` (`graphrt eval`).
    pub const EVALUATION_SOURCE: &str = "graphrt.evaluation.source";
    /// The assertion type behind an evaluation (`judge`, `regex`...).
    pub const EVALUATION_TYPE: &str = "graphrt.evaluation.type";
    /// Metric: evaluation scores (histogram, by evaluator name).
    pub const EVAL_SCORE_METRIC: &str = "graphrt.eval.score";
}

/// `gen_ai.provider.name` value for a graphrt provider name. Known providers map
/// to their well-known semconv value; others pass through unchanged.
pub fn provider_name(provider: &str) -> &str {
    match provider {
        "gemini" | "google" | "google_genai" => "gcp.gemini",
        "vertex" | "vertex_ai" | "vertexai" => "gcp.vertex_ai",
        "bedrock" | "aws_bedrock" => "aws.bedrock",
        "azure" | "azure_openai" | "azure-openai" => "azure.ai.openai",
        "mistral" | "mistralai" => "mistral_ai",
        "xai" | "grok" => "x_ai",
        "watsonx" => "ibm.watsonx.ai",
        other => other,
    }
}

/// Span name `{operation} {subject}`, or just the operation when there is no subject.
pub fn span_name(operation: &str, subject: Option<&str>) -> String {
    match subject.filter(|s| !s.is_empty()) {
        Some(subject) => format!("{operation} {subject}"),
        None => operation.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn providers_map_to_well_known_values() {
        assert_eq!(provider_name("openai"), "openai");
        assert_eq!(provider_name("anthropic"), "anthropic");
        assert_eq!(provider_name("gemini"), "gcp.gemini");
        assert_eq!(provider_name("ollama"), "ollama");
        assert_eq!(span_name("chat", Some("gpt-4o")), "chat gpt-4o");
        assert_eq!(span_name("retrieval", None), "retrieval");
        assert_eq!(span_name("chat", Some("")), "chat");
    }
}
