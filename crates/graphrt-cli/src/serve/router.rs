//! The HTTP router: routes, their authentication layers and the body limits.

use std::sync::Arc;

use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};

use super::auth::{authorize_start, require_auth};
use super::events::run_events;
use super::resume::resume_run;
use super::runs::{abort_run, delete_run, get_run, list_runs};
use super::start::start_run;
use super::state::AppState;
use super::workflows::{list_nodes, list_workflows, validate_workflow};

/// Default upload limit for knowledge base files (`--max-upload-bytes`).
pub const DEFAULT_MAX_UPLOAD_BYTES: usize = 16 * 1024 * 1024;

/// The HTTP router.
pub fn router(state: Arc<AppState>, max_body_bytes: usize) -> Router {
    router_with_uploads(state, max_body_bytes, DEFAULT_MAX_UPLOAD_BYTES)
}

/// The HTTP router, with a separate body limit for knowledge base file uploads.
pub fn router_with_uploads(state: Arc<AppState>, max_body_bytes: usize, max_upload_bytes: usize) -> Router {
    let key_only =
        key_only_routes(max_upload_bytes).layer(middleware::from_fn_with_state(Arc::clone(&state), require_auth));
    let counted = key_only.merge(self_authenticating_routes(&state));
    // Opt-in usage telemetry counts API requests (in memory); health checks are not counted.
    #[cfg(feature = "telemetry")]
    let counted = counted.layer(middleware::from_fn(crate::telemetry::count_requests));
    // Gateway-signed routes authenticate themselves (HMAC, `--trust-gateway`).
    #[cfg(feature = "trust-gateway")]
    let counted = if state.gateway_agent.is_some() { counted.merge(crate::gateway_agent::router()) } else { counted };
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readiness))
        .merge(counted)
        .layer(DefaultBodyLimit::max(max_body_bytes))
        // Outermost: early rejections leave the body unread; drain it so keep-alive clients can reuse the connection.
        .layer(middleware::from_fn_with_state(
            crate::body_drain::DrainLimit(max_body_bytes),
            crate::body_drain::drain_unread_body,
        ))
        .with_state(state)
}

/// `GET /readyz`: `200 ready`, or `503 draining` once a shutdown drain began, so
/// a load balancer stops routing new runs here. `/healthz` stays `200` (liveness).
async fn readiness(State(state): State<Arc<AppState>>) -> Response {
    if state.is_draining() {
        (StatusCode::SERVICE_UNAVAILABLE, "draining").into_response()
    } else {
        "ready".into_response()
    }
}

/// Routes that take the API key only; the caller adds the `require_auth` layer.
fn key_only_routes(max_upload_bytes: usize) -> Router<Arc<AppState>> {
    let api = Router::new()
        .route("/v1/workflows", get(list_workflows))
        .route("/v1/runs", get(list_runs))
        .route("/v1/runs/{run_id}", get(get_run).merge(delete(delete_run)))
        .route("/v1/runs/{run_id}/events", get(run_events))
        .route("/v1/runs/{run_id}/abort", post(abort_run))
        .route("/v1/runs/{run_id}/resume", post(resume_run))
        .route("/v1/nodes", get(list_nodes))
        .route("/v1/validate", post(validate_workflow));
    #[cfg(feature = "rag")]
    let api = with_knowledge_routes(api, max_upload_bytes);
    #[cfg(not(feature = "rag"))]
    let _ = max_upload_bytes;
    #[cfg(feature = "scores")]
    let api = api.route("/v1/runs/{run_id}/scores", get(crate::scores::get_scores));
    #[cfg(feature = "skills")]
    let api = with_skill_routes(api);
    #[cfg(feature = "learning")]
    let api = with_learning_routes(api);
    api.route("/v1/hooks", get(crate::webhooks::list_hooks))
}

/// The knowledge base API under `/v1/knowledge`; file uploads get their own
/// body limit (`--max-upload-bytes`).
#[cfg(feature = "rag")]
fn with_knowledge_routes(api: Router<Arc<AppState>>, max_upload_bytes: usize) -> Router<Arc<AppState>> {
    use crate::knowledge_api as k;
    api.route("/v1/knowledge/datasets", get(k::list_datasets).post(k::create_dataset))
        .route("/v1/knowledge/datasets/{dataset}", delete(k::delete_dataset))
        .route("/v1/knowledge/resources", get(k::list_resources))
        .route("/v1/knowledge/datasets/{dataset}/documents", get(k::list_documents).post(k::put_text_document))
        .route(
            "/v1/knowledge/datasets/{dataset}/documents/{document}",
            get(k::get_document).merge(delete(k::delete_document)).merge(axum::routing::patch(k::patch_document)),
        )
        .route(
            "/v1/knowledge/datasets/{dataset}/documents/{document}/file",
            axum::routing::put(k::put_file_document).layer(DefaultBodyLimit::max(max_upload_bytes)),
        )
        .route("/v1/knowledge/retrieve", post(k::retrieve))
}

/// The skill store API under `/v1/skills` (owner actions: API key only).
#[cfg(feature = "skills")]
fn with_skill_routes(api: Router<Arc<AppState>>) -> Router<Arc<AppState>> {
    use crate::skills_api as s;
    api.route("/v1/skills", get(s::list_skills).post(s::install_skill))
        .route("/v1/skills/drafts", post(s::check_draft))
        .route(
            "/v1/skills/{name}",
            get(s::get_skill).merge(axum::routing::patch(s::patch_skill)).merge(delete(s::delete_skill)),
        )
}

/// The lesson store API under `/v1/learning` (owner actions: API key only).
#[cfg(feature = "learning")]
fn with_learning_routes(api: Router<Arc<AppState>>) -> Router<Arc<AppState>> {
    use crate::learning_api as l;
    api.route("/v1/learning/{agent}/lessons", get(l::list_lessons).post(l::add_lesson))
        .route(
            "/v1/learning/{agent}/lessons/{id}",
            axum::routing::patch(l::patch_lesson).merge(delete(l::delete_lesson)),
        )
        .route("/v1/learning/{agent}/forget", post(l::forget))
        .route("/v1/learning/{agent}/distill", post(l::distill))
        .route("/v1/learning/{agent}/policy", get(l::get_policy).merge(axum::routing::patch(l::patch_policy)))
        .route("/v1/learning/{agent}/audit", get(l::audit))
}

/// Routes that authenticate on their own rather than by the API key alone.
fn self_authenticating_routes(state: &Arc<AppState>) -> Router<Arc<AppState>> {
    // Run starts accept a verified end-user JWT instead of the key (`--verify-jwt`).
    let start = Router::new()
        .route("/v1/workflows/{name}/runs", post(start_run))
        .layer(middleware::from_fn_with_state(Arc::clone(state), authorize_start));
    // Webhook deliveries authenticate themselves (signature, else the bearer key).
    let hooks = Router::new().route("/v1/hooks/{hook}", post(crate::webhooks::receive));
    let routes = start.merge(hooks);
    // Feedback authorizes itself: a verified end-user JWT for the user's own runs, else the key.
    #[cfg(feature = "scores")]
    let routes = routes.route("/v1/runs/{run_id}/feedback", post(crate::scores::post_feedback));
    // The workspace routes exist only with `--workspace-api`, and authorize themselves:
    // the API key, or a verified end user inside their own directory.
    #[cfg(feature = "files")]
    let routes = if state.file_api.is_some() { routes.merge(super::files_api::routes()) } else { routes };
    routes
}
