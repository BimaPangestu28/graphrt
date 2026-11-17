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
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|e| CallError::Protocol(format!("invalid JSON-RPC message: {e}")))?;
    // `"result": null` is a valid (empty) result; a missing `result` is not.
    let has_result = value.get("result").is_some();
    let raw: RawMessage =
        serde_json::from_value(value).map_err(|e| CallError::Protocol(format!("invalid JSON-RPC message: {e}")))?;
    match (raw.id, raw.method) {
        (Some(id), Some(method)) => Ok(Incoming::Request { id, method }),
        (None, Some(method)) => {
            Ok(Incoming::Notification(Notification { method, params: raw.params.unwrap_or(serde_json::Value::Null) }))
        }
        (Some(serde_json::Value::Null), None) => {
            // A plugin answers `id: null` only when it could not read a request at all
            // (oversized or malformed line): the request it belonged to is lost.
            let detail = raw.error.map(|e| e.message).unwrap_or_else(|| "no detail".into());
            Err(CallError::Protocol(format!("plugin could not read a request: {detail}")))
        }
        (Some(id), None) => {
            let id = match id {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            };
            let result = match (raw.error, has_result) {
                (Some(err), _) => Err(CallError::Remote { code: err.code, message: err.message, data: err.data }),
                (None, true) => Ok(raw.result.unwrap_or(serde_json::Value::Null)),
                (None, false) => Err(CallError::Protocol("response has neither `result` nor `error`".into())),
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub(crate) struct WriteMsg {
    pub selector: graphrt_core::Selector,
    #[serde(default)]
    pub value: serde_json::Value,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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

    #[test]
    fn response_without_result_or_error_is_a_protocol_error() {
        assert!(matches!(
            parse(r#"{"jsonrpc":"2.0","id":"r-1"}"#),
            Ok(Incoming::Response { result: Err(CallError::Protocol(_)), .. })
        ));
        // An explicit null result is a valid (empty) result at the protocol level.
        assert!(matches!(
            parse(r#"{"jsonrpc":"2.0","id":"r-1","result":null}"#),
            Ok(Incoming::Response { result: Ok(serde_json::Value::Null), .. })
        ));
        // `id: null` means the plugin lost a request.
        assert!(matches!(
            parse(r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}"#),
            Err(CallError::Protocol(_))
        ));
    }

    #[test]
    fn result_messages_reject_unknown_fields() {
        assert!(serde_json::from_str::<RunResultMsg>(r#"{"output":{"a":1}}"#).is_err());
        assert!(serde_json::from_str::<RunResultMsg>(r#"{"outputs":{"a":1}}"#).is_ok());
        assert!(serde_json::from_str::<RunResultMsg>("null").is_err());
        assert!(serde_json::from_str::<ToolResultMsg>(r#"{"txt":"x"}"#).is_err());
    }
}
