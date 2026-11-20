//! Request headers mapped to secret start inputs (`graphrt serve --input-header`).
//!
//! A chat app that calls `graphrt serve` usually holds a per-user credential (a
//! Supabase or other OAuth JWT) that tools must use on the user's behalf. Sending
//! it in the body works, but mixes it with ordinary input and cannot reuse the
//! `Authorization` header, which carries the serve API key. This module lets the
//! deployment name a header per input instead, mirroring the webhook `headers:`
//! mapping:
//!
//! ```text
//! graphrt serve flows/ --input-header user_jwt=X-User-Authorization
//! ```
//!
//! - The mapping applies to every served workflow whose start node declares the
//!   input; such a declaration must be `type: secret`, checked at startup.
//! - A leading `Bearer ` is stripped and the value becomes [`Value::Secret`], so it
//!   is masked in events, SSE, the run store, logs and OTel, and redacted from
//!   snapshots.
//! - Snapshots never hold the plaintext, so `POST /v1/runs/{id}/resume` reads the
//!   same header again and re-supplies it as a resume secret.
//! - A missing header for a `required` input is `401`; the same input in the body
//!   (or in resume `secrets`) is `400`, so the value has exactly one source.
//!
//! See `docs/serving.md`.

use std::str::FromStr;
use std::sync::Arc;

use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use graphrt_core::{Graph, Value};
use indexmap::IndexMap;

use crate::error::CliError;
use crate::serve::error_response;

/// A request refused by a header mapping; answers `{error}` with its status.
#[derive(Debug)]
pub struct HeaderInputError {
    status: StatusCode,
    message: String,
}

impl HeaderInputError {
    fn new(status: StatusCode, message: &str) -> Self {
        Self { status, message: message.to_owned() }
    }
}

impl IntoResponse for HeaderInputError {
    fn into_response(self) -> Response {
        error_response(self.status, &self.message)
    }
}

/// One `--input-header INPUT=HEADER` flag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeaderInputSpec {
    /// Start input that receives the header value.
    pub input: String,
    /// Request header read for it.
    pub header: HeaderName,
}

impl FromStr for HeaderInputSpec {
    type Err = CliError;

    /// Parse `INPUT=HEADER`, e.g. `user_jwt=X-User-Authorization`.
    ///
    /// @throws CliError (usage) - On a missing `=`, an empty input or an invalid header name.
    fn from_str(spec: &str) -> Result<Self, Self::Err> {
        let invalid = |why: &str| CliError::usage(format!("--input-header `{spec}`: {why}"));
        let (input, header) = spec.split_once('=').ok_or_else(|| invalid("expected INPUT=HEADER"))?;
        let input = input.trim();
        if input.is_empty() {
            return Err(invalid("the input name is empty"));
        }
        let header = HeaderName::from_str(header.trim()).map_err(|_| invalid("invalid header name"))?;
        if header == axum::http::header::AUTHORIZATION {
            return Err(invalid("`Authorization` carries the serve API key; use another header"));
        }
        Ok(Self { input: input.to_owned(), header })
    }
}

/// A mapped input as declared by one workflow.
#[derive(Debug)]
struct Binding {
    input: String,
    header: HeaderName,
    required: bool,
    /// `"<start node id>.<input>"`, the resume secret selector.
    selector: String,
}

/// The header-mapped inputs of one served workflow.
#[derive(Debug)]
pub struct HeaderBindings {
    bindings: Vec<Binding>,
}

impl HeaderBindings {
    /// Bindings of `graph` for `specs`: one per spec whose input the start node declares.
    ///
    /// @returns `None` when the workflow declares none of the mapped inputs.
    /// @throws String - When a mapped input is declared with a type other than `secret`.
    pub fn resolve(specs: &[HeaderInputSpec], workflow: &str, graph: &Graph) -> Result<Option<Self>, String> {
        let Some(start) = graph.root_scope().and_then(|scope| graph.node(scope.entry)).filter(|n| &*n.kind == "start")
        else {
            return Ok(None);
        };
        let declared = start.doc.config.get("variables").and_then(serde_json::Value::as_array);
        let mut bindings = Vec::new();
        for spec in specs {
            let Some(decl) = declared.into_iter().flatten().find(|d| d["variable"].as_str() == Some(&spec.input))
            else {
                continue;
            };
            let kind = decl["type"].as_str().unwrap_or("text-input");
            if kind != "secret" {
                return Err(format!(
                    "workflow `{workflow}`: input `{}` comes from header `{}` (--input-header) and must be \
                     declared `type: secret`, not `{kind}`",
                    spec.input, spec.header
                ));
            }
            bindings.push(Binding {
                input: spec.input.clone(),
                header: spec.header.clone(),
                required: decl["required"].as_bool().unwrap_or(false),
                selector: format!("{}.{}", start.id, spec.input),
            });
        }
        Ok((!bindings.is_empty()).then_some(Self { bindings }))
    }

    /// Whether `input` is one of this workflow's header-mapped inputs.
    pub fn maps(&self, input: &str) -> bool {
        self.bindings.iter().any(|b| b.input == input)
    }

    /// Add the mapped header values to the start `inputs` of a new run.
    ///
    /// @throws HeaderInputError (400) - When the body already sets a mapped input.
    /// @throws HeaderInputError (401) - When the header of a required input is missing.
    pub fn apply_to_inputs(
        &self,
        headers: &HeaderMap,
        inputs: &mut IndexMap<String, Value>,
    ) -> Result<(), HeaderInputError> {
        for binding in &self.bindings {
            if inputs.contains_key(&binding.input) {
                return Err(HeaderInputError::new(
                    StatusCode::BAD_REQUEST,
                    &format!(
                        "input `{}` comes from the `{}` header; do not send it in the body",
                        binding.input, binding.header
                    ),
                ));
            }
            if let Some(token) = binding.read(headers)? {
                inputs.insert(binding.input.clone(), Value::Secret(Arc::from(token)));
            }
        }
        Ok(())
    }

    /// Add the mapped header values to the resume `secrets` (snapshots never hold them).
    ///
    /// @throws HeaderInputError (400) - When the body already re-supplies a mapped secret.
    /// @throws HeaderInputError (401) - When the header of a required input is missing.
    pub fn apply_to_resume(
        &self,
        headers: &HeaderMap,
        secrets: &mut IndexMap<String, String>,
    ) -> Result<(), HeaderInputError> {
        for binding in &self.bindings {
            if secrets.contains_key(&binding.selector) {
                return Err(HeaderInputError::new(
                    StatusCode::BAD_REQUEST,
                    &format!(
                        "secret `{}` comes from the `{}` header; do not send it in the body",
                        binding.selector, binding.header
                    ),
                ));
            }
            if let Some(token) = binding.read(headers)? {
                secrets.insert(binding.selector.clone(), token.to_owned());
            }
        }
        Ok(())
    }
}

impl Binding {
    /// The header value without a leading `Bearer `, `None` when absent and optional.
    fn read<'h>(&self, headers: &'h HeaderMap) -> Result<Option<&'h str>, HeaderInputError> {
        let token = headers.get(&self.header).and_then(|v| v.to_str().ok()).map(strip_bearer).filter(|t| !t.is_empty());
        match token {
            Some(token) => Ok(Some(token)),
            None if self.required => Err(HeaderInputError::new(
                StatusCode::UNAUTHORIZED,
                &format!("missing `{}` header (required for input `{}`)", self.header, self.input),
            )),
            None => Ok(None),
        }
    }
}

/// `value` without a leading `Bearer ` (any case) and surrounding whitespace.
pub(crate) fn strip_bearer(value: &str) -> &str {
    let value = value.trim_start();
    match value.get(..7) {
        Some(scheme) if scheme.eq_ignore_ascii_case("bearer ") => value.get(7..).unwrap_or_default().trim(),
        _ => value.trim_end(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_input_header_flags() {
        let spec: HeaderInputSpec = "user_jwt=X-User-Authorization".parse().expect("valid");
        assert_eq!(spec.input, "user_jwt");
        assert_eq!(spec.header.as_str(), "x-user-authorization");
        assert!("user_jwt".parse::<HeaderInputSpec>().is_err());
        assert!("=X-A".parse::<HeaderInputSpec>().is_err());
        assert!("a=bad header".parse::<HeaderInputSpec>().is_err());
        assert!("a=Authorization".parse::<HeaderInputSpec>().is_err());
    }

    #[test]
    fn strips_the_bearer_scheme() {
        assert_eq!(strip_bearer("Bearer abc"), "abc");
        assert_eq!(strip_bearer("bearer  abc "), "abc");
        assert_eq!(strip_bearer("abc"), "abc");
        assert_eq!(strip_bearer("Bearer"), "Bearer");
        assert_eq!(strip_bearer("Bearer "), "");
    }
}
