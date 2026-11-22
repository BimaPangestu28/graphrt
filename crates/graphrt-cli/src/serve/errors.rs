//! JSON error responses shared by every handler.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use graphrt_core::store::StoreError;

/// `{"error": message}` with `status`.
pub(crate) fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

pub(crate) fn store_error(e: &StoreError) -> Response {
    let status = match e {
        StoreError::NotFound(_) => StatusCode::NOT_FOUND,
        StoreError::Conflict(_) | StoreError::Precondition(_) => StatusCode::CONFLICT,
        StoreError::InvalidId(_) => StatusCode::BAD_REQUEST,
        _ => {
            // I/O and corruption errors name server paths: log them, answer generically.
            eprintln!("run store error: {e}");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "run store error (details in the server log)");
        }
    };
    error_response(status, &e.to_string())
}
