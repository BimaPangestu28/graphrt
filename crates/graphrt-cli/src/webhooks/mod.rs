//! Webhook triggers for `graphrt serve` (`--webhooks <file>`).
//!
//! `POST /v1/hooks/{hook}` starts an async run of the hook's workflow from an
//! arbitrary JSON body, so GitHub-, Stripe- or Slack-style senders can call the
//! server directly:
//!
//! - the body is mapped to start inputs by the hook's `inputs` (input name ->
//!   JSON Pointer) and `headers` (input name -> header) maps, or passed whole as
//!   one input (`body_input`, default `body`);
//! - the delivery is authenticated by an HMAC-SHA256 signature when the hook has
//!   a `signature` (bearer API key otherwise, like the rest of the API);
//! - an idempotency key header makes redelivery return the first run instead of
//!   starting another (the run id is derived from the key, so the run store
//!   itself deduplicates, durably with `--store`);
//! - the response is `202 {run_id, status, duplicate, run, events}`;
//! - an optional `callback` POSTs a signed result when the run finishes, fails,
//!   aborts or pauses (see [`callback`]).
//!
//! Mappings live in this serve-side file, not in the workflow: the workflow stays
//! a portable function of its declared inputs, and sender-specific wiring
//! (payload shapes, secrets, callback URLs) stays with the deployment.
//!
//! ```yaml
//! hooks:
//!   github-push:
//!     workflow: triage
//!     inputs: { repo: /repository/full_name, ref: /ref }
//!     headers: { event: X-GitHub-Event }
//!     signature: { scheme: github, secret_env: GITHUB_WEBHOOK_SECRET }
//!     idempotency_header: X-GitHub-Delivery
//! ```
//!
//! See `docs/serving.md`.

#[cfg(feature = "http")]
pub mod callback;
pub mod signature;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use graphrt_core::Value;
use indexmap::IndexMap;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::CliError;
use crate::serve::{AppState, Launch, error_response, launch_detached, traceparent};
pub use signature::{SignatureScheme, Verifier};

/// Default header carrying the sender's idempotency key.
pub const DEFAULT_IDEMPOTENCY_HEADER: &str = "idempotency-key";
/// Input receiving the whole body when a hook declares no `inputs` mapping.
pub const DEFAULT_BODY_INPUT: &str = "body";
/// Longest accepted idempotency key.
const MAX_IDEMPOTENCY_KEY_CHARS: usize = 256;

/// The `--webhooks` file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WebhooksFile {
    #[serde(default)]
    hooks: IndexMap<String, HookSpec>,
}

/// One hook as written in the file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HookSpec {
    workflow: String,
    /// Input name -> JSON Pointer (RFC 6901) into the body.
    #[serde(default)]
    inputs: IndexMap<String, String>,
    /// Input name -> request header whose value becomes the input.
    #[serde(default)]
    headers: IndexMap<String, String>,
    /// Pass the whole body as this input (default `body` when `inputs` is empty).
    body_input: Option<String>,
    signature: Option<SignatureSpec>,
    idempotency_header: Option<String>,
    callback: Option<CallbackSpec>,
}

/// Where a secret comes from: an environment variable (process environment or
/// `--env-file`) or a file such as a mounted container secret.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretSpec {
    pub secret_env: Option<String>,
    pub secret_file: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignatureSpec {
    #[serde(default)]
    scheme: SignatureScheme,
    #[serde(flatten)]
    secret: SecretSpec,
    header: Option<String>,
    prefix: Option<String>,
    tolerance_secs: Option<u64>,
}

/// `callback` of a hook, as written in the file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallbackSpec {
    pub url: String,
    #[serde(flatten)]
    pub secret: SecretSpec,
    /// Statuses that trigger a callback: `succeeded`, `failed`, `aborted`, `paused`.
    pub on: Option<Vec<String>>,
    pub max_attempts: Option<u32>,
    pub backoff_ms: Option<u64>,
    pub timeout_ms: Option<u64>,
}

/// A configured hook.
#[derive(Debug)]
pub struct Hook {
    pub id: String,
    pub workflow: String,
    inputs: IndexMap<String, String>,
    headers: IndexMap<String, String>,
    body_input: Option<String>,
    verifier: Option<Verifier>,
    idempotency_header: String,
    #[cfg(feature = "http")]
    callback: Option<Arc<callback::CallbackTarget>>,
}

impl Hook {
    /// Start inputs for a delivery.
    fn map_inputs(&self, body: &serde_json::Value, headers: &HeaderMap) -> IndexMap<String, Value> {
        let mut inputs = IndexMap::new();
        if let Some(name) = &self.body_input {
            inputs.insert(name.clone(), Value::from_json(body.clone()));
        }
        // Missing pointers and headers leave the input unset: the start node then
        // applies its default or fails the run for a required input.
        for (name, pointer) in &self.inputs {
            if let Some(value) = body.pointer(pointer) {
                inputs.insert(name.clone(), Value::from_json(value.clone()));
            }
        }
        for (name, header) in &self.headers {
            if let Some(value) = headers.get(header).and_then(|v| v.to_str().ok()) {
                inputs.insert(name.clone(), Value::from(value));
            }
        }
        inputs
    }

    /// Run id derived from the idempotency key, so a redelivery maps to the same run.
    fn idempotent_run_id(&self, key: &str) -> String {
        let digest = Sha256::new().chain_update(self.id.as_bytes()).chain_update([0]).chain_update(key).finalize();
        format!("hook-{}", crate::hex::to_hex(digest.get(..16).unwrap_or_default()))
    }

    /// Whether deliveries authenticate with a signature instead of the bearer key.
    pub fn is_signed(&self) -> bool {
        self.verifier.is_some()
    }
}

/// The hooks served under `/v1/hooks`.
#[derive(Debug, Default)]
pub struct Webhooks {
    hooks: IndexMap<String, Hook>,
}

impl Webhooks {
    /// Load a webhooks file (YAML or JSON) and resolve its secrets.
    ///
    /// @param path - The `--webhooks` file.
    /// @param vars - Process environment overlaid with `--env-file`, for `secret_env`.
    /// @param allow_private_network - Egress policy of callbacks (`--allow-private-network`).
    /// @throws CliError (usage) - On an invalid file, a missing secret, or a callback
    ///   URL the egress policy refuses.
    pub fn load(path: &Path, vars: &[(String, String)], allow_private_network: bool) -> Result<Self, CliError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| CliError::usage(format!("cannot read webhooks file `{}`: {e}", path.display())))?;
        let value = crate::setup::parse_structured(&text, &format!("webhooks file `{}`", path.display()))?;
        Self::from_value(value, vars, allow_private_network)
            .map_err(|e| CliError::usage(format!("webhooks file `{}`: {e}", path.display())))
    }

    /// Build hooks from an already parsed webhooks document. Errors are plain messages.
    pub fn from_value(
        value: serde_json::Value,
        vars: &[(String, String)],
        allow_private_network: bool,
    ) -> Result<Self, String> {
        let file: WebhooksFile = serde_json::from_value(value).map_err(|e| e.to_string())?;
        let mut hooks = IndexMap::new();
        for (id, spec) in file.hooks {
            let hook = build_hook(&id, spec, vars, allow_private_network).map_err(|e| format!("hook `{id}`: {e}"))?;
            hooks.insert(id, hook);
        }
        Ok(Self { hooks })
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    pub fn hooks(&self) -> impl Iterator<Item = &Hook> {
        self.hooks.values()
    }

    fn get(&self, id: &str) -> Option<&Hook> {
        self.hooks.get(id)
    }
}

fn build_hook(id: &str, spec: HookSpec, vars: &[(String, String)], allow_private: bool) -> Result<Hook, String> {
    let id_ok = !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !id_ok {
        return Err("ids are 1-64 ASCII letters, digits, `-`, `_` or `.`".into());
    }
    if let Some((name, pointer)) = spec.inputs.iter().find(|(_, p)| !p.is_empty() && !p.starts_with('/')) {
        return Err(format!("input `{name}`: `{pointer}` is not a JSON Pointer (it must start with `/`)"));
    }
    let body_input = spec
        .body_input
        .or_else(|| spec.inputs.is_empty().then(|| DEFAULT_BODY_INPUT.to_owned()))
        .filter(|n| !n.is_empty());
    let verifier = match spec.signature {
        None => None,
        Some(signature) => {
            let secret = resolve_secret(&signature.secret, vars).map_err(|e| format!("signature: {e}"))?;
            let verifier = Verifier::new(signature.scheme, secret, signature.header, signature.prefix);
            Some(match signature.tolerance_secs {
                Some(secs) => verifier.with_tolerance(secs),
                None => verifier,
            })
        }
    };
    #[cfg(feature = "http")]
    let callback = match spec.callback {
        Some(callback) => Some(Arc::new(callback::CallbackTarget::from_spec(callback, vars, allow_private)?)),
        None => None,
    };
    #[cfg(not(feature = "http"))]
    {
        let _ = allow_private;
        if spec.callback.is_some() {
            return Err("`callback` requires a build with the `http` feature".into());
        }
    }
    Ok(Hook {
        id: id.to_owned(),
        workflow: spec.workflow,
        inputs: spec.inputs,
        headers: spec.headers,
        body_input,
        verifier,
        idempotency_header: spec
            .idempotency_header
            .map(|h| h.to_ascii_lowercase())
            .unwrap_or_else(|| DEFAULT_IDEMPOTENCY_HEADER.to_owned()),
        #[cfg(feature = "http")]
        callback,
    })
}

/// Read a secret from `secret_env` (environment or `--env-file`) or `secret_file`.
///
/// @throws String - When neither or both are set, or the secret is missing or empty.
pub fn resolve_secret(spec: &SecretSpec, vars: &[(String, String)]) -> Result<Vec<u8>, String> {
    let secret = match (&spec.secret_env, &spec.secret_file) {
        (Some(name), None) => vars
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .ok_or_else(|| format!("environment variable `{name}` is not set"))?,
        (None, Some(path)) => std::fs::read_to_string(path)
            .map(|text| text.trim_end_matches(['\r', '\n']).to_owned())
            .map_err(|e| format!("cannot read secret file `{}`: {e}", path.display()))?,
        _ => return Err("set exactly one of `secret_env` and `secret_file`".into()),
    };
    if secret.is_empty() {
        return Err("the secret is empty".into());
    }
    Ok(secret.into_bytes())
}

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default()
}

/// `GET /v1/hooks`: the configured hooks (no secrets).
pub(crate) async fn list_hooks(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let hooks: Vec<serde_json::Value> = state
        .webhooks()
        .hooks()
        .map(|hook| {
            serde_json::json!({
                "id": hook.id,
                "path": format!("/v1/hooks/{}", hook.id),
                "workflow": hook.workflow,
                "signature": hook.verifier.as_ref().map(Verifier::scheme),
                "idempotency_header": hook.idempotency_header,
                "callback": hook_has_callback(hook),
            })
        })
        .collect();
    Json(serde_json::json!({ "hooks": hooks }))
}

fn hook_has_callback(hook: &Hook) -> bool {
    #[cfg(feature = "http")]
    return hook.callback.is_some();
    #[cfg(not(feature = "http"))]
    {
        let _ = hook;
        false
    }
}

/// `POST /v1/hooks/{hook}`: start a run from a webhook delivery.
pub(crate) async fn receive(
    State(state): State<Arc<AppState>>,
    UrlPath(hook_id): UrlPath<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let Some(hook) = state.webhooks().get(&hook_id) else {
        return error_response(StatusCode::NOT_FOUND, &format!("unknown hook `{hook_id}`"));
    };
    // Size limit (`--max-body-bytes`) violations surface here as 413.
    let body = match body {
        Ok(body) => body,
        Err(rejection) => return error_response(rejection.status(), &rejection.body_text()),
    };
    if let Some(refused) = refuse_unauthenticated_delivery(&state, hook, &headers, &body) {
        return refused;
    }
    let json: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(json) => json,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &format!("body is not valid JSON: {e}")),
    };
    let run_id = match idempotency_key(&headers, &hook.idempotency_header) {
        Ok(key) => key.map(|key| hook.idempotent_run_id(key)),
        Err(message) => return error_response(StatusCode::BAD_REQUEST, &message),
    };
    let inputs = hook.map_inputs(&json, &headers);
    match launch_detached(&state, &hook.workflow, run_id, inputs, traceparent(&headers)).await {
        Ok(launch) => accepted_delivery(hook, launch),
        Err(response) => response,
    }
}

/// `202` for a delivery: the new run (whose completion callback starts now),
/// or the existing run of a redelivery.
fn accepted_delivery(hook: &Hook, launch: Launch) -> Response {
    #[cfg(not(feature = "http"))]
    let _ = hook;
    let (run_id, status, duplicate) = match launch {
        Launch::Started(active) => {
            let run_id = active.run_id.clone();
            #[cfg(feature = "http")]
            if let Some(target) = &hook.callback {
                callback::spawn_delivery(Arc::clone(target), &hook.id, &hook.workflow, active);
            }
            (run_id, "running".to_owned(), false)
        }
        Launch::Duplicate { run_id, status } => (run_id, status, true),
    };
    let body = serde_json::json!({
        "run_id": run_id,
        "status": status,
        "duplicate": duplicate,
        "run": format!("/v1/runs/{run_id}"),
        "events": format!("/v1/runs/{run_id}/events"),
    });
    (StatusCode::ACCEPTED, Json(body)).into_response()
}

/// `401` unless the delivery carries a valid signature (hooks with a
/// `signature`) or the bearer API key (the others).
fn refuse_unauthenticated_delivery(
    state: &AppState,
    hook: &Hook,
    headers: &HeaderMap,
    body: &[u8],
) -> Option<Response> {
    let Some(verifier) = &hook.verifier else { return state.refuse_without_api_key(headers) };
    let refusal = verifier.verify(headers, body, unix_now_secs()).err()?;
    Some(error_response(StatusCode::UNAUTHORIZED, &refusal.to_string()))
}

/// The delivery's idempotency key, if it sent one.
///
/// @throws String - When the header is empty, too long or not visible ASCII.
fn idempotency_key<'a>(headers: &'a HeaderMap, header: &str) -> Result<Option<&'a str>, String> {
    let Some(value) = headers.get(header) else { return Ok(None) };
    let key = value.to_str().map(str::trim).unwrap_or_default();
    if key.is_empty() || key.len() > MAX_IDEMPOTENCY_KEY_CHARS {
        return Err(format!("`{header}` must be 1-{MAX_IDEMPOTENCY_KEY_CHARS} visible ASCII characters"));
    }
    Ok(Some(key))
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;
    use serde_json::json;

    use super::*;

    fn hooks(value: serde_json::Value) -> Result<Webhooks, String> {
        Webhooks::from_value(value, &[("SECRET".into(), "s".into())], false)
    }

    #[test]
    fn whole_body_is_the_default_input() {
        let webhooks = hooks(json!({ "hooks": { "h": { "workflow": "w" } } })).unwrap();
        let hook = webhooks.get("h").unwrap();
        let inputs = hook.map_inputs(&json!({ "a": 1 }), &HeaderMap::new());
        assert_eq!(inputs.get("body"), Some(&Value::from_json(json!({ "a": 1 }))));
    }

    #[test]
    fn pointers_and_headers_map_to_inputs() {
        let webhooks = hooks(json!({ "hooks": { "h": {
            "workflow": "w",
            "inputs": { "repo": "/repository/name", "missing": "/nope" },
            "headers": { "event": "X-GitHub-Event" },
        } } }))
        .unwrap();
        let hook = webhooks.get("h").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", HeaderValue::from_static("push"));
        let inputs = hook.map_inputs(&json!({ "repository": { "name": "graphrt" } }), &headers);
        assert_eq!(inputs.get("repo"), Some(&Value::from("graphrt")));
        assert_eq!(inputs.get("event"), Some(&Value::from("push")));
        assert!(!inputs.contains_key("missing"));
        assert!(!inputs.contains_key("body"), "no whole body when `inputs` is declared");
    }

    #[test]
    fn invalid_configs_are_rejected() {
        let err = |v| hooks(v).unwrap_err();
        assert!(err(json!({ "hooks": { "bad id": { "workflow": "w" } } })).contains("ids are"));
        assert!(err(json!({ "hooks": { "h": { "workflow": "w", "inputs": { "a": "x" } } } })).contains("JSON Pointer"));
        assert!(
            err(json!({ "hooks": { "h": { "workflow": "w", "signature": { "secret_env": "NOPE" } } } }))
                .contains("`NOPE` is not set")
        );
        assert!(err(json!({ "hooks": { "h": { "workflow": "w", "signature": {} } } })).contains("exactly one"));
        assert!(err(json!({ "hooks": { "h": { "workflow": "w", "typo": 1 } } })).contains("unknown field"));
    }

    #[test]
    fn idempotent_ids_are_stable_per_hook_and_key() {
        let webhooks = hooks(json!({ "hooks": { "a": { "workflow": "w" }, "b": { "workflow": "w" } } })).unwrap();
        let a = webhooks.get("a").unwrap();
        let b = webhooks.get("b").unwrap();
        assert_eq!(a.idempotent_run_id("k1"), a.idempotent_run_id("k1"));
        assert_ne!(a.idempotent_run_id("k1"), a.idempotent_run_id("k2"));
        assert_ne!(a.idempotent_run_id("k1"), b.idempotent_run_id("k1"));
        assert!(graphrt_core::store::validate_run_id(&a.idempotent_run_id("k1")).is_ok());
    }
}
