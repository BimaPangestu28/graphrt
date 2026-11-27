//! Scripted model providers (feature `scripted-llm`): run agent graphs with no
//! network and no key, for app tests, design previews and the smoke tests of
//! this repository.
//!
//! ```json
//! "scripted_models": {
//!   "demo": [
//!     { "tool_calls": [{ "name": "battery", "arguments": {} }] },
//!     { "text": "Your battery is at {tool_result}." }
//!   ]
//! }
//! ```
//!
//! A workflow then uses `model: {provider: demo, name: any}`. The step a request
//! gets is the number of assistant turns since the last user message, so the
//! same script plays out the same way for every conversation, across pauses and
//! resumes and with runs in parallel. Requests past the last step get the last
//! step again. In `text`, `{tool_result}` is replaced with the text of the
//! latest tool result. Text streams word by word.

use std::sync::Arc;

use futures::future::BoxFuture;
use graphrt_core::event::Usage;
use graphrt_llm::LlmError;
use graphrt_llm::types::{ChatDelta, ChatModel, ChatRequest, ChatStream, FinishReason, Role};
use serde::Deserialize;

use crate::GraphrtError;

/// One scripted tool call.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScriptedCall {
    /// Call id; `call_<step>_<index>` when left out.
    #[serde(default)]
    id: Option<String>,
    /// The tool's name as the model sees it (the tool name without its provider).
    name: String,
    #[serde(default = "empty_object")]
    arguments: serde_json::Value,
}

fn empty_object() -> serde_json::Value {
    serde_json::json!({})
}

/// One scripted response: tool calls, or text.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScriptedStep {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ScriptedCall>,
}

/// Most steps of one script.
const MAX_STEPS: usize = 256;

#[derive(Debug)]
pub(crate) struct ScriptedModel {
    steps: Vec<ScriptedStep>,
}

impl ScriptedModel {
    pub(crate) fn new(name: &str, steps: Vec<ScriptedStep>) -> Result<Self, GraphrtError> {
        if steps.is_empty() || steps.len() > MAX_STEPS {
            return Err(GraphrtError::config(format!("scripted_models.{name}: give 1 to {MAX_STEPS} steps")));
        }
        for (index, step) in steps.iter().enumerate() {
            if step.text.is_some() == !step.tool_calls.is_empty() {
                return Err(GraphrtError::config(format!(
                    "scripted_models.{name}[{index}]: a step has either `text` or `tool_calls`"
                )));
            }
        }
        Ok(Self { steps })
    }

    fn step_of(&self, request: &ChatRequest) -> usize {
        let since_user = request.messages.iter().rev().take_while(|m| m.role != Role::User);
        let turns = since_user.filter(|m| m.role == Role::Assistant).count();
        turns.min(self.steps.len().saturating_sub(1))
    }

    fn deltas(&self, request: &ChatRequest) -> Vec<ChatDelta> {
        let index = self.step_of(request);
        let Some(step) = self.steps.get(index) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let finish = if let Some(text) = &step.text {
            let latest = request.messages.iter().rev().find(|m| m.role == Role::Tool).map(|m| m.text());
            let text = text.replace("{tool_result}", latest.as_deref().unwrap_or(""));
            out.extend(text.split_inclusive(' ').map(|word| ChatDelta::Text(word.to_owned())));
            FinishReason::Stop
        } else {
            for (call_index, call) in step.tool_calls.iter().enumerate() {
                out.push(ChatDelta::ToolCall {
                    index: call_index,
                    id: Some(call.id.clone().unwrap_or_else(|| format!("call_{index}_{call_index}"))),
                    name: Some(call.name.clone()),
                    arguments: call.arguments.to_string(),
                });
            }
            FinishReason::ToolCalls
        };
        out.push(ChatDelta::Usage(Usage { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 }));
        out.push(ChatDelta::Finish(finish));
        out
    }
}

impl ChatModel for ScriptedModel {
    fn chat(&self, request: ChatRequest) -> BoxFuture<'_, Result<ChatStream, LlmError>> {
        let deltas = self.deltas(&request);
        Box::pin(async move {
            let stream: ChatStream = Box::pin(futures::stream::iter(deltas.into_iter().map(Ok)));
            Ok(stream)
        })
    }
}

/// Register the providers of the config's `scripted_models`.
pub(crate) fn register(
    models: &mut graphrt_llm::ModelRegistry,
    scripts: indexmap::IndexMap<String, Vec<ScriptedStep>>,
) -> Result<(), GraphrtError> {
    for (name, steps) in scripts {
        let model = ScriptedModel::new(&name, steps)?;
        models.add(name, Arc::new(model));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphrt_llm::types::Message;
    use serde_json::json;

    fn model() -> ScriptedModel {
        let steps: Vec<ScriptedStep> = serde_json::from_value(json!([
            {"tool_calls": [{"name": "battery"}]},
            {"text": "Battery: {tool_result}"},
        ]))
        .unwrap();
        ScriptedModel::new("demo", steps).unwrap()
    }

    #[test]
    fn the_step_follows_the_conversation() {
        let model = model();
        let mut request = ChatRequest::new("m", vec![Message::system("s"), Message::user("hi")]);
        let first = model.deltas(&request);
        assert!(matches!(&first[0], ChatDelta::ToolCall { name: Some(n), id: Some(id), arguments, .. }
            if n == "battery" && id == "call_0_0" && arguments == "{}"));
        request.messages.push(Message::assistant("calling"));
        request.messages.push(Message::tool("call_0_0", "81 %"));
        let text: String = model
            .deltas(&request)
            .into_iter()
            .filter_map(|d| if let ChatDelta::Text(t) = d { Some(t) } else { None })
            .collect();
        assert_eq!(text, "Battery: 81 %");
        // A new user message starts the script again.
        request.messages.push(Message::user("again"));
        assert!(matches!(model.deltas(&request)[0], ChatDelta::ToolCall { .. }));
    }

    #[test]
    fn scripts_are_checked() {
        let bad = |steps: serde_json::Value| {
            let steps: Vec<ScriptedStep> = serde_json::from_value(steps).unwrap();
            ScriptedModel::new("demo", steps).is_err()
        };
        assert!(bad(json!([])));
        assert!(bad(json!([{}])));
        assert!(bad(json!([{"text": "a", "tool_calls": [{"name": "x"}]}])));
        assert!(serde_json::from_value::<Vec<ScriptedStep>>(json!([{"txt": "a"}])).is_err());
    }
}
