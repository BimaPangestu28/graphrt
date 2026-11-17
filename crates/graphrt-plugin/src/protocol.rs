//! JSON-RPC 2.0 messages of the plugin protocol.

use serde::{Deserialize, Serialize};

use crate::error::CallError;

/// Notification forwarded from a plugin while a request is in flight.
#[derive(Clone, Debug, PartialEq)]
pub struct Notification {
    pub method: String,
    pub params: serde_json::Value,
}

impl Notification {
    /// The `request_id` a notification belongs to, if any.
    pub fn request_id(&self) -> Option<&str> {
        self.params.get("request_id").and_then(|v| v.as_str())
    }
}

/// An incoming message, classified.
#[derive(Debug)]
pub(crate) enum Incoming {
    /// Response to one of our requests.
    Response { id: String, result: Result<serde_json::Value, CallError> },
    /// Notification from the plugin.
    Notification(Notification),
    /// A request from the plugin (unsupported; answered with "method not found").
    Request { id: serde_json::Value, method: String },
}

#[derive(Deserialize)]
struct RawMessage {
    #[serde(default)]
    id: Option<serde_json::Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<serde_json::Value>,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<RawError>,
}

#[derive(Deserialize)]
struct RawError {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
    #[serde(default)]
    data: Option<serde_json::Value>,
}

/// Parse one message line.
pub(crate) fn parse(line: &str) -> Result<Incoming, CallError> {
    let raw: RawMessage =
        serde_json::from_str(line).map_err(|e| CallError::Protocol(format!("invalid JSON-RPC message: {e}")))?;
    match (raw.id, raw.method) {
        (Some(id), Some(method)) => Ok(Incoming::Request { id, method }),
        (None, Some(method)) => {
            Ok(Incoming::Notification(Notification { method, params: raw.params.unwrap_or(serde_json::Value::Null) }))
        }
        (Some(id), None) => {
            let id = match id {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            };
            let result = match (raw.result, raw.error) {
                (_, Some(err)) => Err(CallError::Remote { code: err.code, message: err.message, data: err.data }),
                (Some(result), None) => Ok(result),
                (None, None) => Ok(serde_json::Value::Null),
            };
            Ok(Incoming::Response { id, result })
        }
        (None, None) => Err(CallError::Protocol("message has neither `id` nor `method`".into())),
    }
}

/// Encode a request.
pub(crate) fn request(id: &str, method: &str, params: &serde_json::Value) -> String {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

/// Encode a notification.
pub(crate) fn notification(method: &str, params: &serde_json::Value) -> String {
    serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string()
}

/// Encode a "method not found" error response.
pub(crate) fn method_not_found(id: &serde_json::Value, method: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": crate::error::METHOD_NOT_FOUND, "message": format!("host does not implement `{method}`")}
    })
    .to_string()
}

/// `node.run` result.
#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct RunResultMsg {
    #[serde(default)]
    pub outputs: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub writes: Vec<WriteMsg>,
    #[serde(default)]
    pub usage: Option<graphrt_core::Usage>,
    #[serde(default)]
    pub metadata: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub pause: Option<PauseMsg>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct WriteMsg {
    pub selector: graphrt_core::Selector,
    #[serde(default)]
    pub value: serde_json::Value,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct PauseMsg {
    #[serde(default = "default_pause_kind")]
    pub kind: String,
    #[serde(default)]
    pub data: serde_json::Value,
    #[serde(default)]
    pub state: serde_json::Value,
}

fn default_pause_kind() -> String {
    "plugin".to_owned()
}

/// `tool.call` result.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ToolResultMsg {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub data: serde_json::Value,
    #[serde(default)]
    pub is_error: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_messages() {
        assert!(matches!(parse(r#"{"jsonrpc":"2.0","id":"r-1","result":{"a":1}}"#), Ok(Incoming::Response { .. })));
        assert!(matches!(
            parse(r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32000,"message":"x"}}"#),
            Ok(Incoming::Response { id, result: Err(_) }) if id == "3"
        ));
        assert!(matches!(
            parse(r#"{"jsonrpc":"2.0","method":"node.chunk","params":{"request_id":"r-1"}}"#),
            Ok(Incoming::Notification(n)) if n.request_id() == Some("r-1")
        ));
        assert!(matches!(parse(r#"{"jsonrpc":"2.0","id":1,"method":"x"}"#), Ok(Incoming::Request { .. })));
        assert!(parse("not json").is_err());
        assert!(parse("{}").is_err());
    }
}
