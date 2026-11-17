//! Client for TypeSafe's System One decision API (model family "Jev").
//!
//! A System One request sends one `state` (text, JSON, or images) plus named,
//! typed questions; every question is evaluated in parallel and answered with a
//! typed, calibrated result:
//!
//! | Question | Answer |
//! |---|---|
//! | [`Question::Choice`] | selected option, a probability per option, confidence |
//! | [`Question::Score`]  | expected score over ordered levels, probabilities, confidence |
//! | [`Question::Noul`]   | probability that the yes/no statement is true |
//! | [`Question::Box`]    | box edges (fractions of the image) and optional confidence |
//!
//! Requests go to `POST {base_url}/v1/systemone` with `Authorization: Bearer <key>`.
//! The key is static ([`SystemOneConfig::api_key`]) or fetched per request from a
//! [`CredentialSource`]; a 401/403 invalidates the credential and retries once.
//!
//! The client validates the documented request rules before sending (at least one
//! question, 1–255 choice options, 1–10 score levels, nesting depth, referenced
//! assets, body size) and sets the `TypeSafe-Danger-Zone-Images` header whenever
//! a request carries images or a box question.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::credentials::{Credential, CredentialSource};
use crate::error::LlmError;

/// Default API endpoint.
pub const DEFAULT_BASE_URL: &str = "https://api.typesafe.ai";
/// Default model alias: the newest stable Jev release.
pub const DEFAULT_MODEL: &str = "jev-latest";
/// Registry / service name used by nodes (`model.provider: typesafe`).
pub const PROVIDER: &str = "typesafe";

/// Documented maximum request body (32 MiB).
pub const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
/// Documented maximum nesting of `state`, `instructions` and `criteria`.
pub const MAX_NESTING: usize = 64;
/// Documented maximum options of a choice question.
pub const MAX_CHOICE_OPTIONS: usize = 255;
/// Documented maximum levels of a score question.
pub const MAX_SCORE_LEVELS: usize = 10;
/// Documented maximum length of one asset reference, in bytes.
pub const MAX_REFERENCE_BYTES: usize = 1024;
/// Documented maximum total length of all asset references, in bytes.
pub const MAX_REFERENCES_BYTES: usize = 65_536;

const IMAGES_HEADER: &str = "TypeSafe-Danger-Zone-Images";
const LONG_CONTEXT_HEADER: &str = "TypeSafe-Danger-Zone-Long-Context";
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Client configuration.
#[derive(Clone)]
pub struct SystemOneConfig {
    pub base_url: String,
    /// Static API key. Ignored when `credentials` is set.
    pub api_key: Option<String>,
    /// Per-request credential source (e.g. a token broker on a phone).
    pub credentials: Option<Arc<dyn CredentialSource>>,
    /// Whole-request timeout.
    pub timeout: Duration,
    /// Request bodies above this size are rejected before sending (capped at 32 MiB).
    pub max_request_bytes: usize,
    /// Responses above this size are rejected.
    pub max_response_bytes: usize,
    /// Extra headers (e.g. a tenant header). Not sent when a credential moves the
    /// request to a different origin.
    pub headers: Vec<(String, String)>,
}

impl Default for SystemOneConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_owned(),
            api_key: None,
            credentials: None,
            timeout: Duration::from_secs(60),
            max_request_bytes: MAX_BODY_BYTES,
            max_response_bytes: 8 * 1024 * 1024,
            headers: Vec::new(),
        }
    }
}

impl fmt::Debug for SystemOneConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SystemOneConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("credentials", &self.credentials.as_ref().map(|_| "<source>"))
            .field("timeout", &self.timeout)
            .field("max_request_bytes", &self.max_request_bytes)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("headers", &self.headers.iter().map(|(k, _)| k).collect::<Vec<_>>())
            .finish()
    }
}

impl SystemOneConfig {
    /// Configuration with a static API key.
    pub fn with_api_key(api_key: impl Into<String>) -> Self {
        Self { api_key: Some(api_key.into()), ..Self::default() }
    }

    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub fn credentials(mut self, source: Arc<dyn CredentialSource>) -> Self {
        self.credentials = Some(source);
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Criteria of a yes/no question: what counts as yes and as no.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NoulCriteria {
    #[serde(rename = "true", default, skip_serializing_if = "Option::is_none")]
    pub yes: Option<Json>,
    #[serde(rename = "false", default, skip_serializing_if = "Option::is_none")]
    pub no: Option<Json>,
}

/// A typed question.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
#[non_exhaustive]
pub enum Question {
    /// Select one option. `criteria` maps option names to descriptions (may be `null`).
    Choice {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<Json>,
        criteria: IndexMap<String, Json>,
    },
    /// Rate against ordered levels; level `i` scores `i`.
    Score {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<Json>,
        criteria: Vec<Json>,
    },
    /// Probability that a statement is true / the answer is yes.
    Noul {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<Json>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
    /// Locate something in the request's image.
    Box {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<Json>,
    },
}

impl Question {
    pub fn choice(instructions: impl Into<String>, criteria: IndexMap<String, Json>) -> Self {
        Question::Choice { instructions: Some(Json::String(instructions.into())), criteria }
    }

    pub fn score(instructions: impl Into<String>, levels: Vec<Json>) -> Self {
        Question::Score { instructions: Some(Json::String(instructions.into())), criteria: levels }
    }

    pub fn noul(instructions: impl Into<String>) -> Self {
        Question::Noul { instructions: Some(Json::String(instructions.into())), criteria: None }
    }

    pub fn noul_with(instructions: impl Into<String>, criteria: NoulCriteria) -> Self {
        Question::Noul { instructions: Some(Json::String(instructions.into())), criteria: Some(criteria) }
    }

    pub fn bounding_box(instructions: impl Into<String>) -> Self {
        Question::Box { instructions: Some(Json::String(instructions.into())) }
    }

    /// Wire name of the question type.
    pub fn kind(&self) -> &'static str {
        match self {
            Question::Choice { .. } => "choice",
            Question::Score { .. } => "score",
            Question::Noul { .. } => "noul",
            Question::Box { .. } => "box",
        }
    }
}

/// An image asset, referenced by its key wherever it appears in a request string.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageAsset {
    #[serde(rename = "type")]
    pub kind: String,
    /// `png`, `jpg` or `jpeg`.
    pub format: String,
    /// Standard base64 with padding, no `data:` prefix.
    pub data_base64: String,
}

impl fmt::Debug for ImageAsset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImageAsset")
            .field("format", &self.format)
            .field("data_base64", &format_args!("<{} bytes>", self.data_base64.len()))
            .finish()
    }
}

impl ImageAsset {
    /// Asset from raw image bytes. `format` must be `png`, `jpg` or `jpeg`.
    pub fn new(format: &str, bytes: &[u8]) -> Result<Self, LlmError> {
        let format = normalize_format(format)
            .ok_or_else(|| LlmError::BadRequest(format!("unsupported image format `{format}` (png or jpg)")))?;
        Ok(Self {
            kind: "image".to_owned(),
            format: format.to_owned(),
            data_base64: graphrt_core::file::base64_encode(bytes),
        })
    }
}

/// Map a format name or media type to `png` / `jpg`.
pub fn normalize_format(raw: &str) -> Option<&'static str> {
    let lowered = raw.trim().to_ascii_lowercase();
    let short = lowered.rsplit('/').next().unwrap_or(&lowered);
    match short {
        "png" => Some("png"),
        "jpg" | "jpeg" | "pjpeg" => Some("jpg"),
        _ => None,
    }
}

/// A System One request.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SystemOneRequest {
    pub model: String,
    pub state: Json,
    pub questions: IndexMap<String, Question>,
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub assets: IndexMap<String, ImageAsset>,
    /// Send `TypeSafe-Danger-Zone-Long-Context: true` (262k-token limits, slower).
    #[serde(skip)]
    pub long_context: bool,
}

impl SystemOneRequest {
    pub fn new(model: impl Into<String>, state: Json) -> Self {
        Self { model: model.into(), state, questions: IndexMap::new(), assets: IndexMap::new(), long_context: false }
    }

    pub fn question(mut self, name: impl Into<String>, question: Question) -> Self {
        self.questions.insert(name.into(), question);
        self
    }

    pub fn asset(mut self, reference: impl Into<String>, asset: ImageAsset) -> Self {
        self.assets.insert(reference.into(), asset);
        self
    }

    /// Whether the images header must be sent.
    pub fn needs_images_header(&self) -> bool {
        !self.assets.is_empty() || self.questions.values().any(|q| matches!(q, Question::Box { .. }))
    }

    /// Check the documented request rules. Returns the serialized body.
    pub fn validate(&self, max_bytes: usize) -> Result<Vec<u8>, LlmError> {
        let bad = |m: String| Err(LlmError::BadRequest(m));
        if self.model.trim().is_empty() {
            return bad("model must not be empty".into());
        }
        if self.questions.is_empty() {
            return bad("a request needs at least one question".into());
        }
        check_depth("state", &self.state)?;
        for (name, question) in &self.questions {
            if name.is_empty() {
                return bad("question names must not be empty".into());
            }
            match question {
                Question::Choice { instructions, criteria } => {
                    if criteria.is_empty() || criteria.len() > MAX_CHOICE_OPTIONS {
                        return bad(format!(
                            "choice question `{name}` needs 1 to {MAX_CHOICE_OPTIONS} options, got {}",
                            criteria.len()
                        ));
                    }
                    check_opt_depth(name, instructions.as_ref())?;
                    for value in criteria.values() {
                        check_depth(name, value)?;
                    }
                }
                Question::Score { instructions, criteria } => {
                    if criteria.is_empty() || criteria.len() > MAX_SCORE_LEVELS {
                        return bad(format!(
                            "score question `{name}` needs 1 to {MAX_SCORE_LEVELS} levels, got {}",
                            criteria.len()
                        ));
                    }
                    check_opt_depth(name, instructions.as_ref())?;
                    for value in criteria {
                        check_depth(name, value)?;
                    }
                }
                Question::Noul { instructions, criteria } => {
                    check_opt_depth(name, instructions.as_ref())?;
                    if let Some(c) = criteria {
                        check_opt_depth(name, c.yes.as_ref())?;
                        check_opt_depth(name, c.no.as_ref())?;
                    }
                }
                Question::Box { instructions } => check_opt_depth(name, instructions.as_ref())?,
            }
        }
        let mut total_reference_bytes = 0usize;
        for (reference, asset) in &self.assets {
            if reference.trim().is_empty() || reference.len() > MAX_REFERENCE_BYTES {
                return bad(format!("asset references must be 1 to {MAX_REFERENCE_BYTES} bytes"));
            }
            total_reference_bytes = total_reference_bytes.saturating_add(reference.len());
            if normalize_format(&asset.format).is_none() {
                return bad(format!("asset `{reference}` has unsupported format `{}`", asset.format));
            }
        }
        if total_reference_bytes > MAX_REFERENCES_BYTES {
            return bad(format!("asset references exceed {MAX_REFERENCES_BYTES} bytes in total"));
        }
        if !self.assets.is_empty() {
            // Every asset must be referenced somewhere in the request's strings.
            let mut text = String::new();
            collect_strings(&self.state, &mut text);
            for q in self.questions.values() {
                if let Ok(json) = serde_json::to_value(q) {
                    collect_strings(&json, &mut text);
                }
            }
            if let Some(unused) = self.assets.keys().find(|r| !text.contains(r.as_str())) {
                return bad(format!("asset `{unused}` is not referenced in the state or any question"));
            }
        }
        let body = serde_json::to_vec(self).map_err(|e| LlmError::BadRequest(format!("cannot encode request: {e}")))?;
        let limit = max_bytes.min(MAX_BODY_BYTES);
        if body.len() > limit {
            return bad(format!("request body is {} bytes, over the {limit}-byte limit", body.len()));
        }
        Ok(body)
    }
}

fn check_opt_depth(name: &str, value: Option<&Json>) -> Result<(), LlmError> {
    match value {
        Some(v) => check_depth(name, v),
        None => Ok(()),
    }
}

fn check_depth(what: &str, value: &Json) -> Result<(), LlmError> {
    fn depth(value: &Json) -> usize {
        match value {
            Json::Array(items) => 1 + items.iter().map(depth).max().unwrap_or(0),
            Json::Object(map) => 1 + map.values().map(depth).max().unwrap_or(0),
            _ => 0,
        }
    }
    if depth(value) > MAX_NESTING {
        return Err(LlmError::BadRequest(format!("`{what}` is nested deeper than {MAX_NESTING} levels")));
    }
    Ok(())
}

fn collect_strings(value: &Json, out: &mut String) {
    match value {
        Json::String(s) => {
            out.push_str(s);
            out.push('\n');
        }
        Json::Array(items) => items.iter().for_each(|v| collect_strings(v, out)),
        Json::Object(map) => map.values().for_each(|v| collect_strings(v, out)),
        _ => {}
    }
}

/// Box edges as fractions of the image (top/bottom from the top, left/right from the left).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct BoxCoordinates {
    pub top: f64,
    pub bottom: f64,
    pub left: f64,
    pub right: f64,
}

/// A typed answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
#[non_exhaustive]
pub enum Answer {
    Choice {
        choice: String,
        confidence: f64,
        probabilities: IndexMap<String, f64>,
    },
    Score {
        score: f64,
        confidence: f64,
        #[serde(default)]
        legend: IndexMap<String, Json>,
        probabilities: IndexMap<String, f64>,
    },
    Noul {
        noul: f64,
    },
    Box {
        #[serde(rename = "box")]
        coordinates: BoxCoordinates,
        #[serde(default)]
        confidence: Option<f64>,
    },
}

impl Answer {
    /// Probability of the selected choice, for choice answers.
    pub fn choice_probability(&self) -> Option<f64> {
        match self {
            Answer::Choice { choice, probabilities, .. } => probabilities.get(choice).copied(),
            _ => None,
        }
    }

    /// Confidence, when the answer carries one.
    pub fn confidence(&self) -> Option<f64> {
        match self {
            Answer::Choice { confidence, .. } | Answer::Score { confidence, .. } => Some(*confidence),
            Answer::Box { confidence, .. } => *confidence,
            Answer::Noul { .. } => None,
        }
    }
}

/// Token usage of a request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemOneUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl SystemOneUsage {
    /// As the runtime's usage type.
    pub fn to_usage(self) -> graphrt_core::Usage {
        graphrt_core::Usage {
            prompt_tokens: self.input_tokens,
            completion_tokens: self.output_tokens,
            total_tokens: self.input_tokens.saturating_add(self.output_tokens),
        }
    }
}

/// A System One response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SystemOneResponse {
    /// Versioned model id that answered (may differ from the requested alias).
    pub model: String,
    pub answers: IndexMap<String, Answer>,
    pub usage: SystemOneUsage,
}

impl SystemOneResponse {
    pub fn answer(&self, name: &str) -> Option<&Answer> {
        self.answers.get(name)
    }
}

/// System One API client. Register it in [`graphrt_core::Services`] as `Arc<SystemOneClient>`.
#[derive(Clone)]
pub struct SystemOneClient {
    config: SystemOneConfig,
    client: reqwest::Client,
}

impl fmt::Debug for SystemOneClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SystemOneClient").field("config", &self.config).finish_non_exhaustive()
    }
}

impl SystemOneClient {
    pub fn new(config: SystemOneConfig) -> Result<Self, LlmError> {
        if config.api_key.is_none() && config.credentials.is_none() {
            return Err(LlmError::Config("TypeSafe client needs an API key or a credential source".into()));
        }
        let client = crate::http::build_client(Duration::from_secs(10))?;
        Ok(Self { config, client })
    }

    /// Build from `TYPESAFE_API_KEY` (required) and `TYPESAFE_BASE_URL` (optional).
    /// Returns `Ok(None)` when no key is set.
    pub fn from_vars(vars: &[(String, String)]) -> Result<Option<Self>, LlmError> {
        let get = |k: &str| vars.iter().find(|(name, v)| name == k && !v.is_empty()).map(|(_, v)| v.clone());
        let Some(key) = get("TYPESAFE_API_KEY") else {
            if get("TYPESAFE_BASE_URL").is_some() {
                return Err(LlmError::Config("TYPESAFE_BASE_URL is set but TYPESAFE_API_KEY is not".into()));
            }
            return Ok(None);
        };
        let mut config = SystemOneConfig::with_api_key(key);
        if let Some(base) = get("TYPESAFE_BASE_URL") {
            config.base_url = base;
        }
        Self::new(config).map(Some)
    }

    /// Build from the process environment (see [`SystemOneClient::from_vars`]).
    pub fn from_env() -> Result<Option<Self>, LlmError> {
        let vars: Vec<(String, String)> = std::env::vars().collect();
        Self::from_vars(&vars)
    }

    pub fn config(&self) -> &SystemOneConfig {
        &self.config
    }

    fn endpoint(base_url: &str) -> String {
        format!("{}/v1/systemone", base_url.trim_end_matches('/'))
    }

    async fn credential(&self) -> Result<Credential, LlmError> {
        match (&self.config.credentials, &self.config.api_key) {
            (Some(source), _) => source.credential(PROVIDER).await,
            (None, Some(key)) => Ok(Credential::new(key.clone())),
            (None, None) => Err(LlmError::Config("no TypeSafe credentials configured".into())),
        }
    }

    /// Evaluate the request's questions.
    pub async fn ask(&self, request: &SystemOneRequest) -> Result<SystemOneResponse, LlmError> {
        let body = request.validate(self.config.max_request_bytes)?;
        let mut refreshed = false;
        loop {
            let credential = self.credential().await?;
            match self.send_once(request, &body, &credential).await {
                Err(LlmError::Auth(_)) if !refreshed && self.config.credentials.is_some() => {
                    // The credential was rejected: drop any cached value and retry once.
                    if let Some(source) = &self.config.credentials {
                        source.invalidate(PROVIDER);
                    }
                    refreshed = true;
                }
                other => return other,
            }
        }
    }

    async fn send_once(
        &self,
        request: &SystemOneRequest,
        body: &[u8],
        credential: &Credential,
    ) -> Result<SystemOneResponse, LlmError> {
        // A credential may move the request to a proxy (thin-proxy pattern).
        let endpoint = crate::http::resolve_endpoint(&self.config.base_url, credential.base_url.as_deref())?;
        let mut builder = self
            .client
            .post(Self::endpoint(&endpoint.base_url))
            .timeout(self.config.timeout)
            .bearer_auth(&credential.secret)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_vec());
        if endpoint.send_static {
            for (name, value) in &self.config.headers {
                builder = builder.header(name.as_str(), value.as_str());
            }
        }
        for (name, value) in &credential.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        if request.needs_images_header() {
            builder = builder.header(IMAGES_HEADER, "true");
        }
        if request.long_context {
            builder = builder.header(LONG_CONTEXT_HEADER, "true");
        }
        let response = builder.send().await.map_err(network_error)?;
        let status = response.status().as_u16();
        let retry_after = retry_after_ms(response.headers());
        if (300..400).contains(&status) {
            return Err(LlmError::BadRequest(format!(
                "TypeSafe endpoint answered with a redirect (HTTP {status}); redirects are not followed"
            )));
        }
        if !(200..300).contains(&status) {
            let body = match read_capped(response, MAX_ERROR_BODY_BYTES).await {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(e) => format!("(error body unreadable: {e})"),
            };
            return Err(error_for_status(status, &body, retry_after));
        }
        let bytes = read_capped(response, self.config.max_response_bytes).await?;
        if bytes.len() > self.config.max_response_bytes {
            return Err(LlmError::Decode(format!(
                "response exceeds the {}-byte limit",
                self.config.max_response_bytes
            )));
        }
        let parsed: SystemOneResponse = serde_json::from_slice(&bytes)
            .map_err(|e| LlmError::Decode(format!("unexpected System One response: {e}")))?;
        if let Some(missing) = request.questions.keys().find(|name| !parsed.answers.contains_key(*name)) {
            return Err(LlmError::Decode(format!("response has no answer for question `{missing}`")));
        }
        for (name, question) in &request.questions {
            if let Some(answer) = parsed.answers.get(name) {
                check_answer(name, question, answer)?;
            }
        }
        Ok(parsed)
    }
}

/// Check that an answer matches its question: same type, the chosen option is one
/// of the criteria, and the probability of the chosen option is present.
fn check_answer(name: &str, question: &Question, answer: &Answer) -> Result<(), LlmError> {
    let bad = |msg: String| Err(LlmError::Decode(format!("answer to `{name}`: {msg}")));
    match (question, answer) {
        (Question::Choice { criteria, .. }, Answer::Choice { choice, probabilities, .. }) => {
            if !criteria.contains_key(choice) {
                return bad(format!("chose `{choice}`, which is not one of the options"));
            }
            if !probabilities.contains_key(choice) {
                return bad(format!("no probability for the chosen option `{choice}`"));
            }
            Ok(())
        }
        (Question::Score { .. }, Answer::Score { probabilities, .. }) => {
            if probabilities.is_empty() {
                return bad("score answer without probabilities".into());
            }
            Ok(())
        }
        (Question::Noul { .. }, Answer::Noul { .. }) | (Question::Box { .. }, Answer::Box { .. }) => Ok(()),
        (q, _) => bad(format!("answer type does not match the `{}` question", q.kind())),
    }
}

fn network_error(e: reqwest::Error) -> LlmError {
    if e.is_timeout() {
        return LlmError::Network("request timed out".into());
    }
    LlmError::Network(e.without_url().to_string())
}

/// Read a body, stopping one byte past `limit` so callers can detect overflow.
async fn read_capped(response: reqwest::Response, limit: usize) -> Result<Vec<u8>, LlmError> {
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(network_error)?;
        let room = limit.saturating_add(1).saturating_sub(bytes.len());
        bytes.extend(chunk.iter().take(room));
        if bytes.len() > limit {
            break;
        }
    }
    Ok(bytes)
}

fn retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    if let Some(ms) = headers.get("retry-after-ms").and_then(|v| v.to_str().ok()) {
        return ms.trim().parse::<u64>().ok();
    }
    let text = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?.trim();
    let seconds: f64 = text.parse().ok()?;
    if seconds.is_finite() && seconds >= 0.0 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Some((seconds * 1000.0) as u64)
    } else {
        None
    }
}

/// Human-readable summary of an error body (`detail` string, object or validation list).
fn summarize(body: &str) -> String {
    const MAX: usize = 1000;
    let text = match serde_json::from_str::<Json>(body) {
        Ok(json) => match json.get("detail") {
            Some(Json::String(s)) => s.clone(),
            Some(Json::Array(items)) => items
                .iter()
                .map(|item| {
                    let loc = item
                        .get("loc")
                        .and_then(Json::as_array)
                        .map(|l| {
                            l.iter()
                                .map(|p| p.as_str().map(str::to_owned).unwrap_or_else(|| p.to_string()))
                                .collect::<Vec<_>>()
                                .join(".")
                        })
                        .unwrap_or_default();
                    let msg = item.get("msg").and_then(Json::as_str).unwrap_or("invalid");
                    if loc.is_empty() { msg.to_owned() } else { format!("{loc}: {msg}") }
                })
                .collect::<Vec<_>>()
                .join("; "),
            Some(other) => {
                other.get("message").and_then(Json::as_str).map(str::to_owned).unwrap_or_else(|| other.to_string())
            }
            None => body.trim().to_owned(),
        },
        Err(_) => body.trim().to_owned(),
    };
    if text.chars().count() > MAX {
        let mut cut: String = text.chars().take(MAX).collect();
        cut.push('…');
        cut
    } else {
        text
    }
}

/// Map a documented HTTP status to an error.
pub fn error_for_status(status: u16, body: &str, retry_after_ms: Option<u64>) -> LlmError {
    let message = summarize(body);
    match status {
        401 | 403 => LlmError::Auth(message),
        429 => LlmError::RateLimit { message, retry_after_ms },
        408 => LlmError::Network(message),
        400 if body.contains("max_tokens_exceeded") => LlmError::ContextLength(message),
        413 => LlmError::BadRequest(format!("request body too large: {message}")),
        s if s >= 500 => {
            if s == 529 && retry_after_ms.is_some() {
                // Overloaded: surface the server's hint through the rate-limit variant.
                LlmError::RateLimit { message: format!("overloaded: {message}"), retry_after_ms }
            } else {
                LlmError::Server { status: s, message }
            }
        }
        _ => LlmError::BadRequest(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone_request() -> SystemOneRequest {
        let mut criteria = IndexMap::new();
        criteria.insert("angry".to_owned(), Json::from("An upset or hostile message"));
        criteria.insert("calm".to_owned(), Json::from("A neutral or polite message"));
        criteria.insert("excited".to_owned(), Json::from("An enthusiastic or eager message"));
        SystemOneRequest::new(
            DEFAULT_MODEL,
            Json::from("I was charged twice for my subscription this month. Please refund the extra charge."),
        )
        .question("tone", Question::choice("What is the tone of this message?", criteria))
    }

    #[test]
    fn serializes_like_the_documented_example() {
        let body = tone_request().validate(MAX_BODY_BYTES).unwrap();
        let json: Json = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "model": "jev-latest",
                "state": "I was charged twice for my subscription this month. Please refund the extra charge.",
                "questions": {"tone": {"type": "choice", "instructions": "What is the tone of this message?",
                    "criteria": {"angry": "An upset or hostile message", "calm": "A neutral or polite message",
                                 "excited": "An enthusiastic or eager message"}}}
            })
        );
    }

    #[test]
    fn parses_documented_answers() {
        let raw = r#"{"model":"jev-latest","answers":{
            "billing":{"type":"noul","noul":0.98},
            "tone":{"type":"choice","choice":"calm","confidence":0.7,"probabilities":{"angry":0.15,"calm":0.8,"excited":0.05}},
            "urgency":{"type":"score","score":1.7,"confidence":0.55,"legend":{"0":"Can wait","1":"Needs attention this week","2":"Needs attention today"},"probabilities":{"0":0.1,"1":0.1,"2":0.8}},
            "red_rectangle":{"type":"box","box":{"top":0.2,"bottom":0.66,"left":0.19,"right":0.68},"confidence":null}},
            "usage":{"input_tokens":120,"output_tokens":12}}"#;
        let r: SystemOneResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(r.answer("billing"), Some(&Answer::Noul { noul: 0.98 }));
        assert_eq!(r.answer("tone").and_then(Answer::choice_probability), Some(0.8));
        assert_eq!(r.answer("tone").and_then(Answer::confidence), Some(0.7));
        assert!(matches!(r.answer("red_rectangle"), Some(Answer::Box { confidence: None, .. })));
        assert_eq!(r.usage.to_usage().total_tokens, 132);
    }

    #[test]
    fn validation_rules() {
        let empty = SystemOneRequest::new(DEFAULT_MODEL, Json::from("x"));
        assert!(empty.validate(MAX_BODY_BYTES).is_err());

        let too_many: IndexMap<String, Json> = (0..256).map(|i| (format!("o{i}"), Json::Null)).collect();
        let r = SystemOneRequest::new(DEFAULT_MODEL, Json::from("x")).question("c", Question::choice("?", too_many));
        assert!(r.validate(MAX_BODY_BYTES).unwrap_err().to_string().contains("255"));

        let levels: Vec<Json> = (0..11).map(|i| Json::from(i.to_string())).collect();
        let r = SystemOneRequest::new(DEFAULT_MODEL, Json::from("x")).question("s", Question::score("?", levels));
        assert!(r.validate(MAX_BODY_BYTES).is_err());

        let mut deep = Json::from("leaf");
        for _ in 0..70 {
            deep = Json::Array(vec![deep]);
        }
        let r = SystemOneRequest::new(DEFAULT_MODEL, deep).question("n", Question::noul("?"));
        assert!(r.validate(MAX_BODY_BYTES).unwrap_err().to_string().contains("nested"));

        let asset = ImageAsset::new("image/png", b"\x89PNG").unwrap();
        let r = SystemOneRequest::new(DEFAULT_MODEL, Json::from("no reference"))
            .question("n", Question::noul("?"))
            .asset("ref-1234", asset.clone());
        assert!(r.validate(MAX_BODY_BYTES).unwrap_err().to_string().contains("not referenced"));
        let r = SystemOneRequest::new(DEFAULT_MODEL, serde_json::json!({"photo": "ref-1234"}))
            .question("n", Question::noul("?"))
            .asset("ref-1234", asset);
        assert!(r.validate(MAX_BODY_BYTES).is_ok());
        assert!(r.needs_images_header());

        assert!(tone_request().validate(10).unwrap_err().to_string().contains("limit"));
        assert!(ImageAsset::new("gif", b"x").is_err());
    }

    #[test]
    fn status_mapping() {
        assert!(matches!(error_for_status(401, "{\"detail\":\"bad key\"}", None), LlmError::Auth(m) if m == "bad key"));
        let rl = error_for_status(429, "{}", Some(1500));
        assert_eq!(rl, LlmError::RateLimit { message: "{}".into(), retry_after_ms: Some(1500) });
        let v = error_for_status(
            422,
            r#"{"detail":[{"loc":["body","state"],"msg":"Field required","type":"missing"}]}"#,
            None,
        );
        assert!(matches!(&v, LlmError::BadRequest(m) if m == "body.state: Field required"), "{v:?}");
        assert!(!v.is_retryable());
        assert!(matches!(
            error_for_status(400, "{\"detail\":\"max_tokens_exceeded\"}", None),
            LlmError::ContextLength(_)
        ));
        assert!(error_for_status(529, "overloaded", None).is_retryable());
        assert!(error_for_status(529, "overloaded", Some(10)).is_retryable());
    }

    #[test]
    fn debug_redacts_key() {
        let c = SystemOneConfig::with_api_key("ts-secret");
        assert!(!format!("{c:?}").contains("ts-secret"));
    }
}
