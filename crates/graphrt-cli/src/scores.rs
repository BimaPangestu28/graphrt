//! Evaluation scores of served runs (cargo feature `scores`).
//!
//! A run collects scores from online evaluators (`--evaluators`, source
//! `online`) and from people (`POST /v1/runs/{id}/feedback`, source `human`).
//! Stores keep them next to the run (`runs/<id>/scores.json` in a directory
//! store, in memory otherwise), so they are bounded and deleted with it
//! (`--max-runs`, `--store-ttl-days`, `DELETE /v1/runs/{id}`).
//!
//! | Method | Path | Body | Response |
//! | --- | --- | --- | --- |
//! | GET | `/v1/runs/{run_id}/scores` | – | `{run_id, scores}` |
//! | POST | `/v1/runs/{run_id}/feedback` | `{name, value, comment?, user?}` | `201` + the stored score |
//!
//! Scores are converted to and from JSON by hand (no serde derives): the
//! `serde_json::Value` code is in every build already, while derived
//! (de)serializers would add code per format for this small type.
//!
//! Score ids are deterministic (a hash of the run, the source, the name and,
//! for feedback, the user), so re-scoring or re-sending feedback replaces the
//! earlier score instead of adding another, and uploads (Langfuse) are
//! idempotent.

use std::sync::Arc;

use axum::Json;
use axum::extract::{FromRequest, Path as UrlPath, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures::future::BoxFuture;
use graphrt_core::store::{RunRecord, StoreResult, now_ms};
use serde_json::{Map, Value as JsonValue};
use sha2::{Digest, Sha256};

use crate::serve::{AppState, error_response};

/// Scores kept per run and per [`ScoreSource`]; a new score beyond this is
/// refused (`409`). Counted per source so feedback, which end users can send
/// on their own runs, never takes the room online evaluators need.
pub const MAX_SCORES_PER_RUN: usize = 256;
const MAX_NAME_BYTES: usize = 200;
/// Langfuse's limit for text scores.
const MAX_TEXT_VALUE_CHARS: usize = 500;
const MAX_COMMENT_BYTES: usize = 4096;
const MAX_USER_BYTES: usize = 256;

/// Who produced a score.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScoreSource {
    /// An online evaluator of this server (`--evaluators`).
    Online,
    /// Feedback through the API.
    Human,
}

impl ScoreSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Human => "human",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "online" => Some(Self::Online),
            "human" => Some(Self::Human),
            _ => None,
        }
    }
}

/// A score value: a number, a boolean or a short text (a category).
#[derive(Clone, Debug, PartialEq)]
pub enum ScoreValue {
    Bool(bool),
    Number(f64),
    Text(String),
}

impl ScoreValue {
    /// The value as a number, when it has one (`true` is 1).
    pub fn as_number(&self) -> Option<f64> {
        match self {
            Self::Bool(flag) => Some(if *flag { 1.0 } else { 0.0 }),
            Self::Number(n) => Some(*n),
            Self::Text(_) => None,
        }
    }

    /// A JSON number, boolean or string.
    pub fn from_json(value: &JsonValue) -> Option<Self> {
        match value {
            JsonValue::Bool(flag) => Some(Self::Bool(*flag)),
            JsonValue::Number(n) => n.as_f64().map(Self::Number),
            JsonValue::String(text) => Some(Self::Text(text.clone())),
            _ => None,
        }
    }

    pub fn to_json(&self) -> JsonValue {
        match self {
            Self::Bool(flag) => JsonValue::Bool(*flag),
            Self::Number(n) => serde_json::Number::from_f64(*n).map_or(JsonValue::Null, JsonValue::Number),
            Self::Text(text) => JsonValue::String(text.clone()),
        }
    }
}

/// One stored score of a run. JSON: `{id, name, value, label?, comment?,
/// source, type?, user?, judge?, created_ms}`.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Score {
    /// Deterministic: see [`score_id`].
    pub id: String,
    pub name: String,
    pub value: ScoreValue,
    /// A low-cardinality label (`pass` / `fail` / `unknown` for evaluators).
    pub label: Option<String>,
    /// The evaluator's explanation, or the person's comment.
    pub comment: Option<String>,
    pub source: ScoreSource,
    /// The assertion type of an online evaluator (`judge`, `regex`...); `type` in JSON.
    pub kind: Option<String>,
    /// Who gave the feedback.
    pub user: Option<String>,
    /// Judge workflow, fingerprint and verdict (judge evaluators).
    pub judge: Option<JsonValue>,
    pub created_ms: u64,
}

impl Score {
    /// A score with an id derived from `run_id`, `source`, `name` and `user`.
    pub fn new(run_id: &str, name: &str, value: ScoreValue, source: ScoreSource, user: Option<String>) -> Self {
        Self {
            id: score_id(run_id, source, name, user.as_deref()),
            name: name.to_owned(),
            value,
            label: None,
            comment: None,
            source,
            kind: None,
            user,
            judge: None,
            created_ms: now_ms(),
        }
    }

    /// The JSON form (stored and returned by the API).
    pub fn to_json(&self) -> JsonValue {
        let mut map = Map::new();
        map.insert("id".into(), self.id.as_str().into());
        map.insert("name".into(), self.name.as_str().into());
        map.insert("value".into(), self.value.to_json());
        let text_fields = [("label", &self.label), ("comment", &self.comment)];
        for (key, field) in text_fields {
            if let Some(text) = field {
                map.insert(key.into(), text.as_str().into());
            }
        }
        map.insert("source".into(), self.source.as_str().into());
        for (key, field) in [("type", &self.kind), ("user", &self.user)] {
            if let Some(text) = field {
                map.insert(key.into(), text.as_str().into());
            }
        }
        if let Some(judge) = &self.judge {
            map.insert("judge".into(), judge.clone());
        }
        map.insert("created_ms".into(), self.created_ms.into());
        JsonValue::Object(map)
    }

    /// Read the JSON form back. `None` when a required field is missing or wrong.
    pub fn from_json(value: &JsonValue) -> Option<Self> {
        let text = |key: &str| value.get(key).and_then(JsonValue::as_str).map(str::to_owned);
        Some(Self {
            id: text("id")?,
            name: text("name")?,
            value: ScoreValue::from_json(value.get("value")?)?,
            label: text("label"),
            comment: text("comment"),
            source: ScoreSource::parse(value.get("source")?.as_str()?)?,
            kind: text("type"),
            user: text("user"),
            judge: value.get("judge").filter(|j| !j.is_null()).cloned(),
            created_ms: value.get("created_ms").and_then(JsonValue::as_u64).unwrap_or_default(),
        })
    }
}

/// Scores as a JSON array.
pub fn scores_json(scores: &[Score]) -> JsonValue {
    JsonValue::Array(scores.iter().map(Score::to_json).collect())
}

/// The id of a score: 32 hex digits of SHA-256 over the run id, the source,
/// the name and the user. Stable across restarts and versions, so a score
/// written twice is the same score (replaced in the store, and an idempotent
/// upload to Langfuse).
pub fn score_id(run_id: &str, source: ScoreSource, name: &str, user: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    for part in [run_id, source.as_str(), name, user.unwrap_or_default()] {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    let digest = hasher.finalize();
    crate::hex::to_hex(digest.get(..16).unwrap_or_default())
}

/// Where scores are kept: implemented by the run stores, next to each run.
pub trait ScoreStore: Send + Sync + std::fmt::Debug {
    /// Add scores to an existing run, replacing stored ones with the same id.
    ///
    /// @throws StoreError::NotFound - The run does not exist.
    /// @throws StoreError::Conflict - The run already has [`MAX_SCORES_PER_RUN`] scores of that source.
    fn put_scores<'a>(&'a self, run_id: &'a str, scores: Vec<Score>) -> BoxFuture<'a, StoreResult<()>>;

    /// The scores of a run, oldest first.
    ///
    /// @throws StoreError::NotFound - The run does not exist.
    fn scores<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<Vec<Score>>>;
}

/// Upsert `new` into `stored` by id.
///
/// @throws StoreError::Conflict - When that would give a run more than
///   [`MAX_SCORES_PER_RUN`] scores of one source.
pub(crate) fn merge_scores(stored: &mut Vec<Score>, new: Vec<Score>, run_id: &str) -> StoreResult<()> {
    for score in new {
        match stored.iter().position(|s| s.id == score.id) {
            Some(index) => {
                if let Some(slot) = stored.get_mut(index) {
                    *slot = score;
                }
            }
            None if stored.iter().filter(|s| s.source == score.source).count() >= MAX_SCORES_PER_RUN => {
                return Err(graphrt_core::store::StoreError::Conflict(format!(
                    "run `{run_id}` already has {MAX_SCORES_PER_RUN} {} scores",
                    score.source.as_str()
                )));
            }
            None => stored.push(score),
        }
    }
    Ok(())
}

/// Body of `POST /v1/runs/{run_id}/feedback`.
#[derive(Debug)]
pub struct FeedbackRequest {
    pub name: String,
    pub value: ScoreValue,
    pub comment: Option<String>,
    /// Who gives the feedback. With a verified user token, its subject instead.
    pub user: Option<String>,
}

impl FeedbackRequest {
    /// Parse and validate a request body.
    ///
    /// @throws String - A message for a `400` response (unknown keys, wrong
    ///   types, sizes).
    pub fn from_json(body: &JsonValue) -> Result<Self, String> {
        let JsonValue::Object(map) = body else { return Err("the body must be a JSON object".into()) };
        if let Some(key) = map.keys().find(|k| !matches!(k.as_str(), "name" | "value" | "comment" | "user")) {
            return Err(format!("unknown field `{key}` (expected name, value, comment, user)"));
        }
        let optional_text = |key: &str| match map.get(key) {
            None | Some(JsonValue::Null) => Ok(None),
            Some(JsonValue::String(text)) => Ok(Some(text.clone())),
            Some(_) => Err(format!("`{key}` must be a string")),
        };
        let request = Self {
            name: optional_text("name")?.ok_or("`name` is required")?,
            value: map
                .get("value")
                .and_then(ScoreValue::from_json)
                .ok_or("`value` must be a number, a boolean or a string")?,
            comment: optional_text("comment")?,
            user: optional_text("user")?,
        };
        request.validate()?;
        Ok(request)
    }

    /// Check sizes and values.
    ///
    /// @throws String - A message for a `400` response.
    pub fn validate(&self) -> Result<(), String> {
        let name = self.name.trim();
        if name.is_empty() || name.len() > MAX_NAME_BYTES || name.chars().any(char::is_control) {
            return Err(format!("`name` must be 1-{MAX_NAME_BYTES} bytes without control characters"));
        }
        match &self.value {
            ScoreValue::Number(n) if !n.is_finite() => return Err("`value` must be a finite number".into()),
            ScoreValue::Text(text) if text.trim().is_empty() || text.chars().count() > MAX_TEXT_VALUE_CHARS => {
                return Err(format!("a text `value` must be 1-{MAX_TEXT_VALUE_CHARS} characters"));
            }
            _ => {}
        }
        if self.comment.as_ref().is_some_and(|c| c.len() > MAX_COMMENT_BYTES) {
            return Err(format!("`comment` must be at most {MAX_COMMENT_BYTES} bytes"));
        }
        if self.user.as_ref().is_some_and(|u| u.is_empty() || u.len() > MAX_USER_BYTES) {
            return Err(format!("`user` must be 1-{MAX_USER_BYTES} bytes"));
        }
        Ok(())
    }
}

/// `GET /v1/runs/{run_id}/scores`.
pub async fn get_scores(State(state): State<Arc<AppState>>, UrlPath(run_id): UrlPath<String>) -> Response {
    match state.score_store().scores(&run_id).await {
        Ok(scores) => Json(serde_json::json!({ "run_id": run_id, "scores": scores_json(&scores) })).into_response(),
        Err(e) => crate::serve::store_error(&e),
    }
}

/// `POST /v1/runs/{run_id}/feedback`: a `human` score. With a verified user
/// token (`--verify-jwt`), only on the user's own runs, and as that user.
pub async fn post_feedback(
    State(state): State<Arc<AppState>>,
    UrlPath(run_id): UrlPath<String>,
    request: Request,
) -> Response {
    // Authorized here rather than by a middleware layer: one handler, less code.
    // The body is extracted only afterwards, so unauthenticated callers get a
    // `401` without the server parsing their JSON, as on every other route.
    let user = match state.authorize_user_or_key(request.headers()).await {
        Ok(user) => user,
        Err(refused) => return refused,
    };
    let body = match Json::<JsonValue>::from_request(request, &()).await {
        Ok(Json(body)) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let body = match FeedbackRequest::from_json(&body) {
        Ok(body) => body,
        Err(message) => return error_response(StatusCode::BAD_REQUEST, &message),
    };
    let record = match state.store().get(&run_id).await {
        Ok(Some(record)) => record,
        Ok(None) => return error_response(StatusCode::NOT_FOUND, "unknown run"),
        Err(e) => return crate::serve::store_error(&e),
    };
    let author = match user {
        Some(subject) => {
            if !is_verified_owner(&record, &subject) {
                return error_response(StatusCode::FORBIDDEN, "this run does not belong to the authenticated user");
            }
            Some(subject)
        }
        None => body.user,
    };
    let mut score = Score::new(&run_id, body.name.trim(), body.value, ScoreSource::Human, author);
    score.comment = body.comment.filter(|c| !c.is_empty());
    if let Err(e) = state.score_store().put_scores(&run_id, vec![score.clone()]).await {
        return crate::serve::store_error(&e);
    }
    state.score_sinks().publish(&record, &score, None);
    (StatusCode::CREATED, Json(score.to_json())).into_response()
}

/// Whether the user `subject` of a verified token owns `record`'s run: the run
/// was started with that user's token. A `user_id` an API-key caller put in the
/// request body is only a label (anyone holding the key can write any value),
/// so it never makes a run rateable by that user.
fn is_verified_owner(record: &RunRecord, subject: &str) -> bool {
    record.user_verified && record.user_id.as_deref() == Some(subject)
}

/// When an evaluation ran, in Unix nanoseconds, for its span (`None`: now).
pub type Timing = Option<(u64, u64)>;

/// Where stored scores are reported besides the store: an `evaluate {name}`
/// span in the run's trace and the `graphrt.eval.score` metric (with
/// OpenTelemetry), and a Langfuse upload (`--langfuse-scores`).
#[derive(Clone, Debug, Default)]
pub struct ScoreSinks {
    #[cfg(feature = "otel")]
    pub telemetry: Option<Arc<crate::otel::Telemetry>>,
    #[cfg(feature = "langfuse")]
    pub langfuse: Option<Arc<crate::langfuse::LangfuseUploader>>,
}

impl ScoreSinks {
    /// Report a stored score of `record`'s run.
    pub fn publish(&self, record: &RunRecord, score: &Score, timing: Timing) {
        #[cfg(feature = "otel")]
        if let Some(telemetry) = &self.telemetry {
            let ids = (record.trace_id.as_deref(), record.span_id.as_deref());
            telemetry.record_evaluation(ids, &record.workflow, &evaluation_result(score), timing);
        }
        #[cfg(feature = "langfuse")]
        if let Some(langfuse) = &self.langfuse {
            langfuse.submit(record, score);
        }
        #[cfg(not(feature = "otel"))]
        let _ = (record, score, timing);
    }

    /// Count a sampled run of `workflow` the online evaluation queue had no room for.
    pub fn dropped(&self, workflow: &str) {
        #[cfg(feature = "otel")]
        if let Some(telemetry) = &self.telemetry {
            telemetry.metrics().record_eval_dropped(workflow);
        }
        #[cfg(not(feature = "otel"))]
        let _ = workflow;
    }
}

/// The OpenTelemetry view of a score: a numeric value (booleans as 0/1), a
/// label (an evaluator's verdict, or a text value), the explanation or comment.
#[cfg(feature = "otel")]
pub fn evaluation_result(score: &Score) -> graphrt_otel::EvaluationResult {
    let mut result = graphrt_otel::EvaluationResult::new(score.name.as_str());
    result.score = score.value.as_number();
    result.label = match (&score.label, &score.value) {
        (Some(label), _) => Some(label.clone()),
        (None, ScoreValue::Text(text)) => Some(text.clone()),
        (None, ScoreValue::Bool(flag)) => Some(if *flag { "true" } else { "false" }.to_owned()),
        (None, ScoreValue::Number(_)) => None,
    };
    result.explanation.clone_from(&score.comment);
    result.source = Some(score.source.as_str().to_owned());
    result.kind.clone_from(&score.kind);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_and_distinct() {
        let a = score_id("run-1", ScoreSource::Human, "thumbs", Some("u1"));
        assert_eq!(a.len(), 32);
        assert_eq!(a, score_id("run-1", ScoreSource::Human, "thumbs", Some("u1")));
        assert_ne!(a, score_id("run-1", ScoreSource::Human, "thumbs", Some("u2")));
        assert_ne!(a, score_id("run-1", ScoreSource::Online, "thumbs", Some("u1")));
        assert_ne!(score_id("ab", ScoreSource::Human, "c", None), score_id("a", ScoreSource::Human, "bc", None));
    }

    #[test]
    fn values_parse_as_number_bool_or_text() {
        let parse = |json: &str| ScoreValue::from_json(&serde_json::from_str(json).unwrap()).unwrap();
        assert_eq!(parse("0.5"), ScoreValue::Number(0.5));
        assert_eq!(parse("true"), ScoreValue::Bool(true));
        assert_eq!(parse("\"great\""), ScoreValue::Text("great".into()));
        assert!(ScoreValue::from_json(&serde_json::json!({})).is_none());
        assert_eq!(ScoreValue::Bool(false).as_number(), Some(0.0));
    }

    #[test]
    fn feedback_is_validated() {
        let body = |json: &str| FeedbackRequest::from_json(&serde_json::from_str(json).unwrap()).map(|_| ());
        assert!(body(r#"{"name": "thumbs", "value": true, "comment": "nice"}"#).is_ok());
        assert!(body(r#"{"name": " ", "value": 1}"#).unwrap_err().contains("name"));
        assert!(body(r#"{"name": "a\nb", "value": 1}"#).is_err());
        assert!(body(&format!(r#"{{"name": "n", "value": "{}"}}"#, "x".repeat(501))).is_err());
        assert!(body(r#"{"name": "n", "value": 1, "user": ""}"#).unwrap_err().contains("user"));
        assert!(body(r#"{"name": "n", "value": 1, "extra": 2}"#).unwrap_err().contains("extra"));
        assert!(body(r#"{"name": "n", "value": {"a": 1}}"#).is_err());
        assert!(body(r#"{"value": 1}"#).unwrap_err().contains("name"));
        assert!(body("[1]").is_err());
    }

    #[test]
    fn scores_round_trip_through_json() {
        let mut score = Score::new("r", "judge", ScoreValue::Number(0.5), ScoreSource::Online, None);
        score.label = Some("pass".into());
        score.kind = Some("judge".into());
        score.judge = Some(serde_json::json!({"verdict": "pass"}));
        let json = score.to_json();
        assert_eq!(json["type"], "judge");
        assert!(json.get("user").is_none());
        assert_eq!(Score::from_json(&json), Some(score));
        assert!(Score::from_json(&serde_json::json!({"id": "x"})).is_none());
    }

    #[test]
    fn merging_replaces_by_id_and_caps() {
        let score = |name: &str| Score::new("r", name, ScoreValue::Number(1.0), ScoreSource::Online, None);
        let mut stored = vec![score("a")];
        let mut again = score("a");
        again.value = ScoreValue::Number(0.0);
        merge_scores(&mut stored, vec![again, score("b")], "r").unwrap();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].value, ScoreValue::Number(0.0));
        let mut full: Vec<Score> = (0..MAX_SCORES_PER_RUN).map(|i| score(&i.to_string())).collect();
        assert!(merge_scores(&mut full, vec![score("one more")], "r").is_err());
        assert!(merge_scores(&mut full, vec![score("0")], "r").is_ok(), "replacing still works");
    }

    #[test]
    fn human_feedback_cannot_crowd_out_online_scores() {
        let human = |name: &str| Score::new("r", name, ScoreValue::Bool(true), ScoreSource::Human, Some("u".into()));
        let mut stored: Vec<Score> = (0..MAX_SCORES_PER_RUN).map(|i| human(&i.to_string())).collect();
        assert!(merge_scores(&mut stored, vec![human("one more")], "r").is_err(), "human scores are capped");
        let online = Score::new("r", "safety", ScoreValue::Number(0.0), ScoreSource::Online, None);
        merge_scores(&mut stored, vec![online], "r").expect("an evaluator still has room");
        assert_eq!(stored.iter().filter(|s| s.source == ScoreSource::Online).count(), 1);
    }
}
