//! Tools offered by a guest module (`tools` in its manifest, `graphrt_tool_call`).
//!
//! Every call gets a fresh instance like a node run. The per-call tool context
//! (the agent's `tool_context`, e.g. an end user's JWT) reaches the guest only
//! when the plugin is trusted with plaintext secrets, and its values are scrubbed
//! from whatever the guest hands back, so a tool cannot leak them to the model.

use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;
use graphrt_core::tool::{ToolCallContext, ToolError, ToolOutput, ToolRuntime, ToolSpec, check_images};
use graphrt_core::value::Value;

use crate::abi::{ToolCallContextData, ToolCallRequest, ToolCallResponse};
use crate::host::Call;
use crate::plugin::{Inner, off_the_runtime};

/// What replaces a context secret found in a tool result.
const SCRUBBED: &str = "******";

/// Tool runtime of one plugin, registered under the plugin's name.
pub(crate) struct WasmTools {
    inner: Arc<Inner>,
}

impl fmt::Debug for WasmTools {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.inner.manifest.tools.iter().map(|t| t.name.as_str()).collect();
        f.debug_struct("WasmTools").field("plugin", &self.inner.manifest.name).field("tools", &names).finish()
    }
}

impl WasmTools {
    pub(crate) fn new(inner: Arc<Inner>) -> Self {
        Self { inner }
    }

    fn error(&self, tool: &str, message: impl fmt::Display) -> ToolError {
        ToolError { tool: tool.to_owned(), message: format!("plugin `{}`: {message}", self.inner.manifest.name) }
    }

    /// Encode the request, refusing a context this plugin may not receive.
    fn request(
        &self,
        name: &str,
        arguments: serde_json::Value,
        context: &ToolCallContext,
    ) -> Result<Vec<u8>, ToolError> {
        if !self.inner.manifest.tools.iter().any(|t| t.name == name) {
            return Err(self.error(name, format_args!("has no tool `{name}`")));
        }
        if !context.is_empty() && !self.accepts_secrets() {
            return Err(self.error(
                name,
                "may not receive a tool call context: it needs `permissions.secrets` and must be trusted",
            ));
        }
        let request = ToolCallRequest {
            name: name.to_owned(),
            arguments,
            context: ToolCallContextData { secrets: context.secrets.clone() },
            env: self.inner.host.env.clone(),
        };
        serde_json::to_vec(&request).map_err(|e| self.error(name, format_args!("encoding request: {e}")))
    }

    /// Decode the guest's answer into a tool output (or a refused call).
    fn decode(&self, name: &str, bytes: &[u8], context: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let response: ToolCallResponse = serde_json::from_slice(bytes)
            .map_err(|e| self.error(name, format_args!("tool result is not valid JSON: {e}")))?;
        if let Some(error) = response.error {
            let message = scrub_text(&format!("{}: {}", error.kind, error.message), context);
            return Err(self.error(name, message));
        }
        check_images(&response.images).map_err(|e| self.error(name, format_args!("invalid `images`: {e}")))?;
        Ok(ToolOutput {
            text: scrub_text(&response.text, context),
            data: Value::from_json(scrub_json(response.data, context)),
            is_error: response.is_error,
            images: response.images,
        })
    }
}

impl ToolRuntime for WasmTools {
    fn list(&self) -> BoxFuture<'_, Result<Vec<ToolSpec>, ToolError>> {
        let tools = self.inner.manifest.tools.clone();
        Box::pin(async move { Ok(tools) })
    }

    fn call<'a>(
        &'a self,
        name: &'a str,
        arguments: serde_json::Value,
        context: &'a ToolCallContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input = self.request(name, arguments, context)?;
            let inner = self.inner.clone();
            let output = off_the_runtime(move || {
                let egress = inner.host.egress.clone();
                inner.compiled.call_logged(Call::Tool(&input), None, egress)
            })
            .await
            .map_err(|e| self.error(name, e))?
            .map_err(|failure| self.error(name, scrub_text(&failure.error.to_string(), context)))?;
            self.decode(name, &output.bytes, context)
        })
    }

    /// Only plugins granted plaintext secrets (`permissions.secrets` + trusted).
    fn accepts_secrets(&self) -> bool {
        self.inner.compiled.limits().expose_secrets
    }
}

/// `text` with every non-empty context secret replaced by [`SCRUBBED`].
fn scrub_text(text: &str, context: &ToolCallContext) -> String {
    context.secrets.values().filter(|secret| !secret.is_empty()).fold(text.to_owned(), |text, secret| {
        if text.contains(secret.as_str()) { text.replace(secret.as_str(), SCRUBBED) } else { text }
    })
}

/// `value` with context secrets scrubbed from every string, object keys included.
fn scrub_json(value: serde_json::Value, context: &ToolCallContext) -> serde_json::Value {
    use serde_json::Value as Json;
    if context.is_empty() {
        return value;
    }
    match value {
        Json::String(text) => Json::String(scrub_text(&text, context)),
        Json::Array(items) => Json::Array(items.into_iter().map(|v| scrub_json(v, context)).collect()),
        Json::Object(map) => {
            Json::Object(map.into_iter().map(|(k, v)| (scrub_text(&k, context), scrub_json(v, context))).collect())
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubs_secrets_everywhere() {
        let mut context = ToolCallContext::default();
        context.secrets.insert("user_jwt".into(), "eyJ.secret.sig".into());
        context.secrets.insert("empty".into(), String::new());
        assert_eq!(scrub_text("Bearer eyJ.secret.sig!", &context), "Bearer ******!");
        let scrubbed = scrub_json(serde_json::json!({"eyJ.secret.sig": ["a eyJ.secret.sig", 1]}), &context);
        assert_eq!(scrubbed, serde_json::json!({"******": ["a ******", 1]}));
        assert_eq!(scrub_text("plain", ToolCallContext::empty()), "plain");
    }
}
