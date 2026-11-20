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
//! Score ids are deterministic (a hash of the run, the source, the name and,
//! for feedback, the user), so re-scoring or re-sending feedback replaces the
//! earlier score instead of adding another, and uploads (Langfuse) are
//! idempotent.

use std::sync::Arc;

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use futures::future::BoxFuture;
use graphrt_core::store::{RunRecord, StoreResult, now_ms};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::serve::{AppState, AuthenticatedUser, error_response};

/// Scores kept per run; a new score beyond this is refused (`409`).
pub const MAX_SCORES_PER_RUN: usize = 256;
const MAX_NAME_BYTES: usize = 200;
/// Langfuse's limit for text scores.
const MAX_TEXT_VALUE_CHARS: usize = 500;
const MAX_COMMENT_BYTES: usize = 4096;
const MAX_USER_BYTES: usize = 256;

/// Who produced a score.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
}

/// A score value: a number, a boolean or a short text (a category).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
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
}

/// One stored score of a run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Score {
    /// Deterministic: see [`score_id`].
    pub id: String,
    pub name: String,
    pub value: ScoreValue,
    /// A low-cardinality label (`pass` / `fail` / `unknown` for evaluators).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The evaluator's explanation, or the person's comment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    pub source: ScoreSource,
    /// The assertion type of an online evaluator (`judge`, `regex`...).
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Who gave the feedback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Judge workflow, fingerprint and verdict (judge evaluators).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge: Option<serde_json::Value>,
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
    /// @throws StoreError::Conflict - The run already has [`MAX_SCORES_PER_RUN`] scores.
    fn put_scores<'a>(&'a self, run_id: &'a str, scores: Vec<Score>) -> BoxFuture<'a, StoreResult<()>>;

    /// The scores of a run, oldest first.
    ///
    /// @throws StoreError::NotFound - The run does not exist.
    fn scores<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<Vec<Score>>>;
}

/// Upsert `new` into `stored` by id.
///
/// @throws StoreError::Conflict - When that would exceed [`MAX_SCORES_PER_RUN`].
pub(crate) fn merge_scores(stored: &mut Vec<Score>, new: Vec<Score>, run_id: &str) -> StoreResult<()> {
    for score in new {
        match stored.iter().position(|s| s.id == score.id) {
            Some(index) => {
                if let Some(slot) = stored.get_mut(index) {
                    *slot = score;
                }
            }
            None if stored.len() >= MAX_SCORES_PER_RUN => {
                return Err(graphrt_core::store::StoreError::Conflict(format!(
                    "run `{run_id}` already has {MAX_SCORES_PER_RUN} scores"
                )));
            }
            None => stored.push(score),
        }
    }
    Ok(())
}

/// Body of `POST /v1/runs/{run_id}/feedback`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedbackRequest {
    pub name: String,
    pub value: ScoreValue,
    #[serde(default)]
    pub comment: Option<String>,
    /// Who gives the feedback. With a verified user token, its subject instead.
    #[serde(default)]
    pub user: Option<String>,
}

impl FeedbackRequest {
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
        Ok(scores) => Json(serde_json::json!({ "run_id": run_id, "scores": scores })).into_response(),
        Err(e) => crate::serve::store_error(&e),
    }
}

/// `POST /v1/runs/{run_id}/feedback`: a `human` score. With a verified user
/// token (`--verify-jwt`), only on the user's own runs, and as that user.
pub async fn post_feedback(
    State(state): State<Arc<AppState>>,
    UrlPath(run_id): UrlPath<String>,
    user: Option<Extension<AuthenticatedUser>>,
    Json(body): Json<FeedbackRequest>,
) -> Response {
    if let Err(message) = body.validate() {
        return error_response(StatusCode::BAD_REQUEST, &message);
    }
    let record = match state.store().get(&run_id).await {
        Ok(Some(record)) => record,
        Ok(None) => return error_response(StatusCode::NOT_FOUND, "unknown run"),
        Err(e) => return crate::serve::store_error(&e),
    };
    let author = match user {
        Some(Extension(AuthenticatedUser(subject))) => {
            // A user token may only rate the user's own runs: those it started.
            if record.user_id.as_deref() != Some(subject.as_str()) {
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
    publish(&state, &record, &score, None);
    (StatusCode::CREATED, Json(score)).into_response()
}

/// When an evaluation ran, in Unix nanoseconds, for its span (`None`: now).
pub type Timing = Option<(u64, u64)>;

/// Tell the outside world about a stored score: an `evaluate {name}` span in
/// the run's trace and the `graphrt.eval.score` metric (with OpenTelemetry),
/// and a Langfuse upload (`--langfuse-scores`).
pub fn publish(state: &AppState, record: &RunRecord, score: &Score, timing: Timing) {
    #[cfg(feature = "otel")]
    if let Some(telemetry) = state.telemetry() {
        let ids = (record.trace_id.as_deref(), record.span_id.as_deref());
        telemetry.record_evaluation(ids, &record.workflow, &evaluation_result(score), timing);
    }
    #[cfg(not(feature = "otel"))]
    let _ = (state, record, score, timing);
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
        let parse = |json: &str| serde_json::from_str::<ScoreValue>(json).unwrap();
        assert_eq!(parse("0.5"), ScoreValue::Number(0.5));
        assert_eq!(parse("true"), ScoreValue::Bool(true));
        assert_eq!(parse("\"great\""), ScoreValue::Text("great".into()));
        assert!(serde_json::from_str::<ScoreValue>("{}").is_err());
        assert_eq!(ScoreValue::Bool(false).as_number(), Some(0.0));
    }

    #[test]
    fn feedback_is_validated() {
        let body = |json: &str| serde_json::from_str::<FeedbackRequest>(json).unwrap().validate();
        assert!(body(r#"{"name": "thumbs", "value": true, "comment": "nice"}"#).is_ok());
        assert!(body(r#"{"name": " ", "value": 1}"#).unwrap_err().contains("name"));
        assert!(body(r#"{"name": "a\nb", "value": 1}"#).is_err());
        assert!(body(&format!(r#"{{"name": "n", "value": "{}"}}"#, "x".repeat(501))).is_err());
        assert!(body(r#"{"name": "n", "value": 1, "user": ""}"#).unwrap_err().contains("user"));
        assert!(serde_json::from_str::<FeedbackRequest>(r#"{"name": "n", "value": 1, "extra": 2}"#).is_err());
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
}
