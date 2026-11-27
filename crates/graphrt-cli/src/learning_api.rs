//! `/v1/learning`: review what the served agents learned (cargo feature `learning`).
//!
//! The same [`LessonStore`] backs the `learning` of the served `agent` nodes,
//! so a lesson approved here is read by the next run.
//!
//! | Method | Path | Body or query | Response |
//! | --- | --- | --- | --- |
//! | GET | `/v1/learning/{agent}/lessons` | `?user=`, `?shared=true`, `?status=` | `{lessons: [Lesson]}`, each with `origin_sentence` |
//! | POST | `/v1/learning/{agent}/lessons` | `{text, user?, kind?}` | `201` + `Lesson` (approved, trust `owner`) |
//! | PATCH | `/v1/learning/{agent}/lessons/{id}` | one of `{decision}`, `{text}`, `{promote: true}` | `Lesson` |
//! | DELETE | `/v1/learning/{agent}/lessons/{id}` | – | `204` |
//! | POST | `/v1/learning/{agent}/forget` | `{user}` or `{run_id}` | `{removed}` |
//! | POST | `/v1/learning/{agent}/distill` | nothing, or `{model?}` | `{observations, lessons, skipped, kept}` |
//! | POST | `/v1/learning/{agent}/skill-draft` | `{model, tools?}` | `{draft: SkillDraft}`, or `{draft: null, needed}`. Nothing is installed |
//! | GET | `/v1/learning/{agent}/policy` | – | `LearningPolicy` |
//! | PATCH | `/v1/learning/{agent}/policy` | `{auto_approve_user_feedback}` | `LearningPolicy` |
//! | GET | `/v1/learning/{agent}/audit` | – | `{entries: [AuditEntry]}` |
//!
//! Every route is an owner action: it needs the API key, a user token is not
//! enough, and a server that listens beyond loopback without a key refuses
//! them all. `actor` in a body names who acted for the audit log (default
//! `api`).
//!
//! **Routes that ask a model.** `distill` with `model` also asks that model
//! what the recorded signals teach, and `skill-draft` asks it to write a
//! skill from the procedures the agent learned. The model is one of this
//! server's configured providers. What it reads is passed as quoted material,
//! and what it answers is never trusted: a suggested lesson is a proposal
//! with trust `derived`, which no policy approves, and a draft is text for a
//! person to read and post to `POST /v1/skills`.
//!
//! Signals are captured without a route of their own: feedback posted to
//! `POST /v1/runs/{id}/feedback` is recorded for every agent that the run's
//! workflow names in `learning.agent` (see [`capture_feedback`]).

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use graphrt_learn::{
    Decision, Error as LearnError, Forget, Learner, Lesson, LessonFilter, LessonKind, LessonStatus, LessonStore,
    NewLesson, Observation, ObservationKind, UserFilter,
};
use serde::Deserialize;
use serde_json::{Value as Json_, json};

use crate::http_input::{JsonBody, QueryParams};
use crate::serve::{AppState, error_response};

const DEFAULT_ACTOR: &str = "api";

/// Body of `POST /v1/learning/{agent}/lessons`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewLessonBody {
    text: String,
    user: Option<String>,
    #[serde(default)]
    kind: LessonKind,
    actor: Option<String>,
}

/// Body of `PATCH /v1/learning/{agent}/lessons/{id}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LessonPatch {
    /// `approve`, `reject` or `retire`.
    decision: Option<String>,
    text: Option<String>,
    #[serde(default)]
    promote: bool,
    actor: Option<String>,
}

/// Body of `POST /v1/learning/{agent}/forget`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgetBody {
    user: Option<String>,
    run_id: Option<String>,
    actor: Option<String>,
}

/// Body of `POST /v1/learning/{agent}/distill`. An empty body means no model.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DistillBody {
    /// `provider/name`, a provider this server has configured: also ask it
    /// for lessons. Its suggestions are proposals like any other.
    model: Option<String>,
}

/// Body of `POST /v1/learning/{agent}/skill-draft`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillDraftBody {
    /// `provider/name`, a provider this server has configured.
    model: String,
    /// Tools the draft may ask for. Asking grants nothing.
    #[serde(default)]
    tools: Vec<String>,
}

/// A model distiller that gives up after [`crate::serve::MODEL_ANSWER_TIMEOUT`].
/// Only the model call is bounded: the pass around it always finishes, so a
/// slow model never leaves the queue half written.
struct TimedDistiller(graphrt_learn::ModelDistiller);

impl graphrt_learn::Distiller for TimedDistiller {
    fn distill<'a>(
        &'a self,
        agent: &'a str,
        observations: &'a [Observation],
    ) -> futures::future::BoxFuture<'a, Result<Vec<graphrt_learn::Candidate>, String>> {
        Box::pin(async move {
            match tokio::time::timeout(crate::serve::MODEL_ANSWER_TIMEOUT, self.0.distill(agent, observations)).await {
                Ok(answer) => answer,
                Err(_) => Err("the model did not answer in time".to_owned()),
            }
        })
    }
}

/// Body of `PATCH /v1/learning/{agent}/policy`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyPatch {
    auto_approve_user_feedback: bool,
    actor: Option<String>,
}

fn store(state: &AppState) -> Result<&Arc<LessonStore>, Response> {
    state.lesson_store().ok_or_else(|| error_response(StatusCode::NOT_FOUND, "this server has no lesson store"))
}

/// Map a lesson store error to an HTTP response. File paths stay in the log.
fn learn_error(error: &LearnError) -> Response {
    let status = match error {
        LearnError::NotFound { .. } => StatusCode::NOT_FOUND,
        LearnError::Full { .. } | LearnError::TooManyProposals { .. } | LearnError::NotAllowed(_) => {
            StatusCode::CONFLICT
        }
        LearnError::Io { .. } | LearnError::Damaged(_) => {
            eprintln!("lesson store error: {error}");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "lesson store error (details in the server log)");
        }
        _ => StatusCode::BAD_REQUEST,
    };
    error_response(status, &error.to_string())
}

/// A lesson as JSON, with its source as a sentence for a person.
fn lesson_json(lesson: &Lesson) -> Json_ {
    let mut value = serde_json::to_value(lesson).unwrap_or(Json_::Null);
    if let Some(fields) = value.as_object_mut() {
        fields.insert("origin_sentence".to_owned(), Json_::String(lesson.origin_sentence()));
    }
    value
}

fn actor(name: Option<&String>) -> &str {
    name.map(String::as_str).filter(|name| !name.trim().is_empty()).unwrap_or(DEFAULT_ACTOR)
}

/// `GET /v1/learning/{agent}/lessons`
pub async fn list_lessons(
    State(state): State<Arc<AppState>>,
    UrlPath(agent): UrlPath<String>,
    RawQuery(query): RawQuery,
) -> Response {
    let store = match store(&state) {
        Ok(store) => store,
        Err(refused) => return refused,
    };
    let query = QueryParams::parse(query.as_deref());
    let shared = match query.boolean("shared") {
        Ok(shared) => shared.unwrap_or(false),
        Err(refused) => return refused,
    };
    let user = match (query.string("user"), shared) {
        (Some(user), _) => UserFilter::User(user),
        (None, true) => UserFilter::AgentWide,
        (None, false) => UserFilter::Any,
    };
    let status = match query.get("status") {
        None => None,
        Some("proposed") => Some(LessonStatus::Proposed),
        Some("approved") => Some(LessonStatus::Approved),
        Some("rejected") => Some(LessonStatus::Rejected),
        Some("retired") => Some(LessonStatus::Retired),
        Some(_) => {
            return error_response(StatusCode::BAD_REQUEST, "status must be proposed, approved, rejected or retired");
        }
    };
    match store.list(&agent, &LessonFilter { user, status }) {
        Ok(lessons) => Json(json!({ "lessons": lessons.iter().map(lesson_json).collect::<Vec<_>>() })).into_response(),
        Err(error) => learn_error(&error),
    }
}

/// `POST /v1/learning/{agent}/lessons`: the owner adds a lesson.
pub async fn add_lesson(
    State(state): State<Arc<AppState>>,
    UrlPath(agent): UrlPath<String>,
    JsonBody(body): JsonBody<NewLessonBody>,
) -> Response {
    let store = match store(&state) {
        Ok(store) => store,
        Err(refused) => return refused,
    };
    let new = NewLesson { agent, user: body.user, kind: body.kind, text: body.text };
    match store.add(new, actor(body.actor.as_ref())) {
        Ok(lesson) => (StatusCode::CREATED, Json(lesson_json(&lesson))).into_response(),
        Err(error) => learn_error(&error),
    }
}

/// `PATCH /v1/learning/{agent}/lessons/{id}`: decide, edit or promote.
pub async fn patch_lesson(
    State(state): State<Arc<AppState>>,
    UrlPath((agent, id)): UrlPath<(String, String)>,
    JsonBody(patch): JsonBody<LessonPatch>,
) -> Response {
    let store = match store(&state) {
        Ok(store) => store,
        Err(refused) => return refused,
    };
    let who = actor(patch.actor.as_ref());
    let changes =
        usize::from(patch.decision.is_some()) + usize::from(patch.text.is_some()) + usize::from(patch.promote);
    if changes != 1 {
        return error_response(StatusCode::BAD_REQUEST, "pass exactly one of `decision`, `text` or `promote: true`");
    }
    let result = if let Some(text) = &patch.text {
        store.edit(&agent, &id, text, who)
    } else if patch.promote {
        store.promote(&agent, &id, who)
    } else {
        let decision = match patch.decision.as_deref() {
            Some("approve") => Decision::Approve,
            Some("reject") => Decision::Reject,
            Some("retire") => Decision::Retire,
            _ => return error_response(StatusCode::BAD_REQUEST, "decision must be approve, reject or retire"),
        };
        store.decide(&agent, &id, decision, who)
    };
    match result {
        Ok(lesson) => Json(lesson_json(&lesson)).into_response(),
        Err(error) => learn_error(&error),
    }
}

/// `DELETE /v1/learning/{agent}/lessons/{id}`
pub async fn delete_lesson(
    State(state): State<Arc<AppState>>,
    UrlPath((agent, id)): UrlPath<(String, String)>,
) -> Response {
    let store = match store(&state) {
        Ok(store) => store,
        Err(refused) => return refused,
    };
    match store.delete(&agent, &id, DEFAULT_ACTOR) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => learn_error(&error),
    }
}

/// `POST /v1/learning/{agent}/forget`: remove everything of a user or a run.
pub async fn forget(
    State(state): State<Arc<AppState>>,
    UrlPath(agent): UrlPath<String>,
    JsonBody(body): JsonBody<ForgetBody>,
) -> Response {
    let store = match store(&state) {
        Ok(store) => store,
        Err(refused) => return refused,
    };
    let what = match (body.user, body.run_id) {
        (Some(user), None) => Forget::User(user),
        (None, Some(run)) => Forget::Run(run),
        _ => return error_response(StatusCode::BAD_REQUEST, "pass exactly one of `user` or `run_id`"),
    };
    match store.forget(&agent, &what, actor(body.actor.as_ref())) {
        Ok(removed) => Json(json!({ "removed": removed })).into_response(),
        Err(error) => learn_error(&error),
    }
}

/// `POST /v1/learning/{agent}/distill`: turn recorded feedback into proposals.
/// Feedback comments become proposals as written. With `model`, that model is
/// also asked what the signals teach, and each suggestion becomes a proposal
/// with trust `derived`. A model that fails is reported in `skipped` and loses
/// no comment.
pub async fn distill(State(state): State<Arc<AppState>>, UrlPath(agent): UrlPath<String>, body: Bytes) -> Response {
    let store = match store(&state) {
        Ok(store) => store,
        Err(refused) => return refused,
    };
    let body: DistillBody = if body.iter().all(u8::is_ascii_whitespace) {
        DistillBody::default()
    } else {
        // An object and nothing else: serde would also read a list as the fields in order.
        let parsed = serde_json::from_slice::<Json_>(&body)
            .and_then(|value| if value.is_object() { serde_json::from_value(value) } else { Ok(None) });
        match parsed {
            Ok(Some(body)) => body,
            Ok(None) => return error_response(StatusCode::BAD_REQUEST, "the body must be a JSON object"),
            Err(error) => return error_response(StatusCode::BAD_REQUEST, &format!("invalid body: {error}")),
        }
    };
    // Resolved before the queue is read: a request that names no usable model changes nothing.
    let distiller = match body.model.as_deref().map(|spec| state.text_model(spec)) {
        Some(Ok(model)) => Some(TimedDistiller(graphrt_learn::ModelDistiller::new(Arc::new(model)))),
        Some(Err(refused)) => return refused,
        None => None,
    };
    let distiller = distiller.as_ref().map(|distiller| distiller as &dyn graphrt_learn::Distiller);
    match Learner::new(Arc::clone(store)).distill(&agent, distiller).await {
        Ok(report) => Json(json!({
            "observations": report.observations,
            "lessons": report.lessons.iter().map(lesson_json).collect::<Vec<_>>(),
            "skipped": report.skipped,
            "kept": report.kept,
        }))
        .into_response(),
        Err(error) => learn_error(&error),
    }
}

/// `POST /v1/learning/{agent}/skill-draft`: ask one of this server's models
/// to write a skill from the procedures `agent` learned (approved lessons that
/// apply to every run). Nothing is installed: the answer is a draft for a
/// person to read, and `POST /v1/skills` installs it.
pub async fn skill_draft(
    State(state): State<Arc<AppState>>,
    UrlPath(agent): UrlPath<String>,
    JsonBody(body): JsonBody<SkillDraftBody>,
) -> Response {
    let store = match store(&state) {
        Ok(store) => store,
        Err(refused) => return refused,
    };
    let model = match state.text_model(&body.model) {
        Ok(model) => model,
        Err(refused) => return refused,
    };
    let drafting = graphrt_learn::draft_skill_from_lessons(store, &agent, &model, &body.tools);
    match tokio::time::timeout(crate::serve::MODEL_ANSWER_TIMEOUT, drafting).await {
        Ok(Ok(Some(draft))) => Json(json!({ "draft": draft })).into_response(),
        Ok(Ok(None)) => {
            Json(json!({ "draft": null, "needed": graphrt_learn::skill_draft::MIN_PROCEDURES })).into_response()
        }
        // The model failed or its answer is not a skill: the caller's request was fine.
        Ok(Err(LearnError::SkillDraft(reason))) => {
            error_response(StatusCode::BAD_GATEWAY, &format!("no draft was written: {reason}"))
        }
        Ok(Err(error)) => learn_error(&error),
        Err(_) => error_response(StatusCode::GATEWAY_TIMEOUT, "the model did not answer in time"),
    }
}

/// `GET /v1/learning/{agent}/policy`
pub async fn get_policy(State(state): State<Arc<AppState>>, UrlPath(agent): UrlPath<String>) -> Response {
    let store = match store(&state) {
        Ok(store) => store,
        Err(refused) => return refused,
    };
    match store.policy(&agent) {
        Ok(policy) => Json(policy).into_response(),
        Err(error) => learn_error(&error),
    }
}

/// `PATCH /v1/learning/{agent}/policy`: turn automatic approval on or off.
pub async fn patch_policy(
    State(state): State<Arc<AppState>>,
    UrlPath(agent): UrlPath<String>,
    JsonBody(patch): JsonBody<PolicyPatch>,
) -> Response {
    let store = match store(&state) {
        Ok(store) => store,
        Err(refused) => return refused,
    };
    let updated = store.policy(&agent).and_then(|mut policy| {
        policy.auto_approve_user_feedback = patch.auto_approve_user_feedback;
        store.set_policy(&agent, policy, actor(patch.actor.as_ref())).map(|()| policy)
    });
    match updated {
        Ok(policy) => Json(policy).into_response(),
        Err(error) => learn_error(&error),
    }
}

/// `GET /v1/learning/{agent}/audit`
pub async fn audit(State(state): State<Arc<AppState>>, UrlPath(agent): UrlPath<String>) -> Response {
    let store = match store(&state) {
        Ok(store) => store,
        Err(refused) => return refused,
    };
    match store.audit(&agent) {
        Ok(entries) => Json(json!({ "entries": entries })).into_response(),
        Err(error) => learn_error(&error),
    }
}

/// Feedback on a run, as the learner records it.
#[cfg_attr(not(feature = "scores"), allow(dead_code))]
#[derive(Debug)]
pub(crate) struct FeedbackSignal<'a> {
    /// The workflow the run belongs to.
    pub workflow: &'a str,
    pub run_id: &'a str,
    pub value: Option<f64>,
    pub comment: Option<&'a str>,
    /// Who gave it.
    pub author: Option<&'a str>,
    /// The author is a verified end user who owns the run. Never set for an
    /// API key caller, whose `user` is only a label.
    pub authenticated: bool,
}

/// Record feedback for every agent the run's workflow names in
/// `learning.agent`. One appended line per agent, no model call. Nothing is
/// learned here: `distill` turns signals into proposals later.
///
/// A failure is logged and never fails the feedback request: the score was
/// stored, and learning is a side effect of it.
#[cfg_attr(not(feature = "scores"), allow(dead_code))]
pub(crate) fn capture_feedback(state: &AppState, signal: &FeedbackSignal<'_>) {
    let Some(store) = state.lesson_store() else { return };
    let agents = state.learning_agents(signal.workflow);
    if agents.is_empty() {
        return;
    }
    let learner = Learner::new(Arc::clone(store));
    for agent in agents {
        let observation = Observation {
            agent,
            user: signal.author.map(str::to_owned),
            run_id: signal.run_id.to_owned(),
            kind: ObservationKind::Feedback {
                value: signal.value,
                comment: signal.comment.map(str::to_owned),
                author: signal.author.map(str::to_owned),
                authenticated: signal.authenticated,
            },
            excerpt: None,
            at_ms: 0,
        };
        if let Err(error) = learner.observe(observation) {
            eprintln!("learning: feedback on run {} was not recorded: {error}", signal.run_id);
        }
    }
}
