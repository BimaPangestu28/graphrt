//! Classification of Machines API failures into [`ProviderErrorKind`]s.
//!
//! `Capacity` makes the pool try another Machine or create a new one (C9),
//! `RateLimited`/`Transient` are retried by the shared HTTP layer with jittered
//! backoff, `Auth`, `NotFound` and `Fatal` are surfaced.

use graphrt_provider_api::{ProviderError, ProviderErrorKind};

/// Phrases flyd attaches to a 409 when a host cannot fit the guest. Not an API
/// contract (flyctl matches the same strings, `fly-go/flaps/flaps_error.go`),
/// hence only a fallback to the `status` code of the body.
const HOST_CAPACITY_PHRASES: &[&str] = &[
    "insufficient CPUs available",
    "insufficient memory available",
    "insufficient IPs available",
    "insufficient resources available",
    "could not reserve resource",
];

/// Machine-readable capacity codes of the error body's `status` field.
const CAPACITY_CODES: &[&str] = &["\"insufficient_capacity\"", "\"volume_placement_capacity\""];

/// Classifies a non-2xx Machines API response.
///
/// @param status - HTTP status (u16)
/// @param body - response body, `{"error": "...", "status": "..."}` or text (&str)
/// @returns The reaction class (ProviderErrorKind)
///
/// @example
/// assert_eq!(classify_fly_response(422, r#"{"status":"insufficient_capacity"}"#), ProviderErrorKind::Capacity);
pub fn classify_fly_response(status: u16, body: &str) -> ProviderErrorKind {
    if CAPACITY_CODES.iter().any(|code| body.contains(code)) {
        return ProviderErrorKind::Capacity;
    }
    if status == 409 && is_capacity_message(body) {
        return ProviderErrorKind::Capacity;
    }
    match status {
        // 412: a lease is held by another client, or the state moved under us.
        412 => ProviderErrorKind::Transient,
        _ => ProviderError::kind_for_status(status),
    }
}

fn is_capacity_message(body: &str) -> bool {
    HOST_CAPACITY_PHRASES.iter().any(|phrase| body.contains(phrase)) || body.contains("no capacity")
}

/// Whether a failed create says a Machine of that name already exists, which
/// makes creating a deterministically named pool Machine idempotent.
pub fn is_name_taken(error: &ProviderError) -> bool {
    error.message.contains("name_taken") || error.message.contains("already exists")
}

/// A JSON body that did not match the expected shape.
pub fn decode_error(what: &str, error: &serde_json::Error) -> ProviderError {
    ProviderError::fatal(format!("Fly Machines API: unexpected {what} body: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_statuses() {
        assert_eq!(classify_fly_response(401, "{}"), ProviderErrorKind::Auth);
        assert_eq!(classify_fly_response(404, r#"{"error":"machine not found"}"#), ProviderErrorKind::NotFound);
        assert_eq!(classify_fly_response(429, ""), ProviderErrorKind::RateLimited);
        assert_eq!(classify_fly_response(503, "upstream"), ProviderErrorKind::Transient);
        assert_eq!(classify_fly_response(412, r#"{"error":"lease currently held"}"#), ProviderErrorKind::Transient);
        assert_eq!(
            classify_fly_response(409, r#"{"error":"machine is in a state that cannot be started"}"#),
            ProviderErrorKind::Transient
        );
        assert_eq!(classify_fly_response(400, r#"{"error":"invalid config"}"#), ProviderErrorKind::Fatal);
    }

    #[test]
    fn detects_capacity_by_code_and_by_flyd_phrase() {
        assert_eq!(
            classify_fly_response(422, r#"{"error":"no room","status":"insufficient_capacity"}"#),
            ProviderErrorKind::Capacity
        );
        let host_full =
            r#"{"error":"could not reserve resource for machine: insufficient memory available to fulfill request"}"#;
        assert_eq!(classify_fly_response(409, host_full), ProviderErrorKind::Capacity);
    }

    #[test]
    fn name_taken_is_recognised() {
        let error = ProviderError::fatal(
            r#"HTTP 422: {"error":"a machine with this name already exists","status":"name_taken"}"#,
        );
        assert!(is_name_taken(&error));
        assert!(!is_name_taken(&ProviderError::fatal("HTTP 400: bad")));
    }
}
