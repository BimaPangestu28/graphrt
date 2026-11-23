//! The request of a run as the store keeps it, so that after a gateway restart
//! a run without a checkpoint is re-dispatched instead of lost (plan §3.4, I1),
//! while no secret is ever written (I6, decision 7).
//!
//! At start the gateway strips from `inputs` every start input the version
//! declares `type: secret` and every input mapped from a request header (a
//! user's JWT), and records only their names in `needs_secrets`. The result is
//! written in the transaction that creates the run. After a restart:
//!
//! - `needs_secrets` empty: the run is re-dispatched from the stored request
//!   with the same run id;
//! - otherwise: it ends `owner_lost` with `credentials_required`, naming the
//!   inputs, and the client submits it again.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json};

use crate::supervisor::RunInput;

/// Error text prefix of a run that cannot restart without its secrets.
pub const CREDENTIALS_REQUIRED: &str = "credentials_required";

/// A run request without its secrets, persisted in the `requests` table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StoredRequest {
    /// The request body, without the stripped inputs and the transport flags.
    pub body: Map<String, Json>,
    /// When the request's credentials expire (JWT `exp`, unix seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// Names of the stripped inputs (sorted): the run cannot restart without them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs_secrets: Vec<String>,
}

impl StoredRequest {
    /// Strips the secrets of `input`.
    ///
    /// @param input - The request as accepted (RunInput).
    /// @param secret_inputs - Start inputs the version declares `type: secret`;
    ///   `None` (not registered) strips every input.
    /// @param header_inputs - Inputs filled from request headers (credentials).
    /// @returns The request to persist (StoredRequest).
    ///
    /// @example
    /// let stored = StoredRequest::strip(&input, Some(&["api_token".into()]), &[]);
    /// assert_eq!(stored.needs_secrets, ["api_token"]);
    pub fn strip(input: &RunInput, secret_inputs: Option<&[String]>, header_inputs: &[&str]) -> Self {
        let mut body = input.body.clone();
        body.remove("stream");
        body.remove("async");
        let mut needs_secrets = Vec::new();
        match body.get_mut("inputs") {
            Some(Json::Object(inputs)) => inputs.retain(|name, _| {
                let secret = secret_inputs.is_none_or(|declared| declared.iter().any(|known| known == name))
                    || header_inputs.contains(&name.as_str());
                if secret {
                    needs_secrets.push(name.clone());
                }
                !secret
            }),
            // Not an object: the agent refuses it, and nothing in it is persisted.
            Some(_) => {
                body.remove("inputs");
                needs_secrets.push("inputs".to_owned());
            }
            None => {}
        }
        needs_secrets.sort_unstable();
        Self { body, expires_at: input.expires_at, needs_secrets }
    }

    /// The request a restarted run is dispatched with.
    pub fn into_input(self) -> RunInput {
        RunInput { body: self.body, expires_at: self.expires_at }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn input(body: Json) -> RunInput {
        RunInput { body: body.as_object().cloned().unwrap_or_default(), expires_at: Some(42) }
    }

    #[test]
    fn declared_and_header_secrets_are_stripped_and_named() {
        let request = input(json!({
            "inputs": { "text": "hi", "api_token": "SECRET", "user_jwt": "JWT" },
            "user_id": "u1", "stream": true, "async": false,
        }));
        let declared = ["api_token".to_owned()];
        let stored = StoredRequest::strip(&request, Some(&declared), &["user_jwt"]);
        assert_eq!(stored.body, *json!({ "inputs": { "text": "hi" }, "user_id": "u1" }).as_object().unwrap());
        assert_eq!(stored.needs_secrets, ["api_token", "user_jwt"]);
        assert_eq!(stored.expires_at, Some(42));
        let text = serde_json::to_string(&stored).unwrap();
        assert!(!text.contains("SECRET") && !text.contains("JWT\""), "{text}");
    }

    #[test]
    fn a_request_without_secrets_needs_none() {
        let stored = StoredRequest::strip(&input(json!({ "inputs": { "text": "hi" } })), Some(&[]), &[]);
        assert!(stored.needs_secrets.is_empty());
        assert_eq!(serde_json::to_string(&stored).unwrap(), r#"{"body":{"inputs":{"text":"hi"}},"expires_at":42}"#);
    }

    #[test]
    fn an_unregistered_version_strips_every_input() {
        let stored = StoredRequest::strip(&input(json!({ "inputs": { "a": 1, "b": 2 }, "tags": ["x"] })), None, &[]);
        assert_eq!(stored.body, *json!({ "inputs": {}, "tags": ["x"] }).as_object().unwrap());
        assert_eq!(stored.needs_secrets, ["a", "b"]);
    }

    #[test]
    fn non_object_inputs_are_not_persisted() {
        let stored = StoredRequest::strip(&input(json!({ "inputs": "SECRET" })), Some(&[]), &[]);
        assert!(!stored.body.contains_key("inputs"));
        assert_eq!(stored.needs_secrets, ["inputs"]);
    }
}
