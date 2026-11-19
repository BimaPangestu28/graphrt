//! Completion callbacks: when a webhook-started run finishes, fails, aborts or
//! pauses, POST a signed JSON result to the hook's `callback.url`.
//!
//! - Requests go through the server's egress policy (`graphrt-http`): private,
//!   loopback and link-local destinations are refused unless the server runs
//!   with `--allow-private-network`, and redirects are not followed.
//! - Each request carries `X-Graphrt-Signature: t=<unix>,v1=<hex>`, the
//!   HMAC-SHA256 of `"<t>.<body>"` (the `graphrt` signature scheme), plus a
//!   stable `X-Graphrt-Delivery` / `Idempotency-Key` so receivers can drop
//!   retried duplicates.
//! - Network errors, timeouts, `408`, `429` and `5xx` are retried with
//!   exponential backoff, up to `max_attempts` in total; other statuses are final.

use std::sync::Arc;
use std::time::Duration;

use graphrt_core::Event;
use graphrt_http::{EgressPolicy, HttpClient};

use super::CallbackSpec;
use super::signature::{GRAPHRT_SIGNATURE_HEADER, sign_graphrt};
use crate::store::live::{ActiveRun, Outcome};

const DEFAULT_MAX_ATTEMPTS: u32 = 5;
const MAX_ATTEMPTS_LIMIT: u32 = 20;
const DEFAULT_BACKOFF_MS: u64 = 1_000;
const MAX_BACKOFF: Duration = Duration::from_secs(300);
const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const STATUSES: [&str; 4] = ["succeeded", "failed", "aborted", "paused"];

/// Where and how a hook's completion callbacks are delivered.
pub struct CallbackTarget {
    url: reqwest::Url,
    secret: Vec<u8>,
    on: Vec<String>,
    max_attempts: u32,
    backoff: Duration,
    timeout: Duration,
    client: HttpClient,
}

impl std::fmt::Debug for CallbackTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallbackTarget")
            .field("url", &self.url.as_str())
            .field("on", &self.on)
            .field("max_attempts", &self.max_attempts)
            .field("secret", &"***")
            .finish_non_exhaustive()
    }
}

impl CallbackTarget {
    /// Validate a hook's `callback` section.
    ///
    /// @throws String - On a missing secret, an unknown status, or a URL the
    ///   egress policy refuses (scheme, literal private address).
    pub fn from_spec(spec: CallbackSpec, vars: &[(String, String)], allow_private: bool) -> Result<Self, String> {
        let secret = super::resolve_secret(&spec.secret, vars).map_err(|e| format!("callback: {e}"))?;
        let url = reqwest::Url::parse(&spec.url).map_err(|e| format!("callback url `{}`: {e}", spec.url))?;
        let policy = EgressPolicy { allow_private, max_redirects: 0, ..EgressPolicy::default() };
        let client = HttpClient::new(policy).map_err(|e| format!("callback: {e}"))?;
        client.check_url(&url).map_err(|e| format!("callback url `{}`: {e}", spec.url))?;
        let on = spec.on.unwrap_or_else(|| STATUSES.iter().map(|s| (*s).to_owned()).collect());
        if let Some(unknown) = on.iter().find(|s| !STATUSES.contains(&s.as_str())) {
            return Err(format!("callback: unknown status `{unknown}` in `on` (expected one of {STATUSES:?})"));
        }
        let max_attempts = spec.max_attempts.unwrap_or(DEFAULT_MAX_ATTEMPTS);
        if !(1..=MAX_ATTEMPTS_LIMIT).contains(&max_attempts) {
            return Err(format!("callback: `max_attempts` must be 1-{MAX_ATTEMPTS_LIMIT}"));
        }
        Ok(Self {
            url,
            secret,
            on,
            max_attempts,
            backoff: Duration::from_millis(spec.backoff_ms.unwrap_or(DEFAULT_BACKOFF_MS)),
            timeout: Duration::from_millis(spec.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).max(1)),
            client,
        })
    }

    /// Delay before retry number `retry` (1-based): `backoff * 2^(retry-1)`, capped.
    fn delay(&self, retry: u32) -> Duration {
        let factor = 1u32.checked_shl(retry.saturating_sub(1)).unwrap_or(u32::MAX);
        self.backoff.saturating_mul(factor).min(MAX_BACKOFF)
    }
}

/// Wait for `run` to finish in the background, then deliver its callback.
pub fn spawn_delivery(target: Arc<CallbackTarget>, hook: &str, workflow: &str, run: Arc<ActiveRun>) {
    let hook = hook.to_owned();
    let workflow = workflow.to_owned();
    tokio::spawn(async move {
        let outcome = run.wait().await;
        let Some(payload) = payload(&hook, &workflow, &run.run_id, &outcome) else { return };
        let status = payload.get("status").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
        if !target.on.contains(&status) {
            return;
        }
        if let Err(error) = deliver(&target, &run.run_id, &status, &payload).await {
            eprintln!("warning: run {}: completion callback to {} failed: {error}", run.run_id, target.url);
        }
    });
}

/// The callback body for a finished execution; `None` without a terminal event.
fn payload(hook: &str, workflow: &str, run_id: &str, outcome: &Outcome) -> Option<serde_json::Value> {
    let mut body = match outcome.terminal.as_ref()? {
        Event::RunSucceeded { outputs, usage, elapsed_ms, .. } => serde_json::json!({
            "status": "succeeded", "outputs": outputs, "answer": outcome.answer,
            "usage": usage, "elapsed_ms": elapsed_ms,
        }),
        Event::RunFailed { error, error_type, usage, elapsed_ms } => serde_json::json!({
            "status": "failed", "error": error, "error_type": error_type, "usage": usage, "elapsed_ms": elapsed_ms,
        }),
        Event::RunAborted { reason, elapsed_ms } => serde_json::json!({
            "status": "aborted", "reason": reason, "elapsed_ms": elapsed_ms,
        }),
        // The snapshot stays on the server; resume with `POST /v1/runs/{id}/resume`.
        Event::RunPaused { reasons, .. } => serde_json::json!({
            "status": "paused", "reasons": reasons, "answer": outcome.answer,
        }),
        _ => return None,
    };
    let status = body.get("status").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
    if let Some(object) = body.as_object_mut() {
        object.insert("type".into(), format!("run.{status}").into());
        object.insert("run_id".into(), run_id.into());
        object.insert("workflow".into(), workflow.into());
        object.insert("hook".into(), hook.into());
        object.insert("run".into(), format!("/v1/runs/{run_id}").into());
    }
    Some(body)
}

/// POST `payload`, retrying transient failures. Returns the last error.
async fn deliver(
    target: &CallbackTarget,
    run_id: &str,
    status: &str,
    payload: &serde_json::Value,
) -> Result<(), String> {
    let body = serde_json::to_vec(payload).map_err(|e| e.to_string())?;
    let delivery_id = format!("{run_id}.{status}");
    let client = target.client.manual_redirect_client(true).map_err(|e| e.to_string())?;
    let mut last_error = String::new();
    for attempt in 1..=target.max_attempts {
        if attempt > 1 {
            tokio::time::sleep(target.delay(attempt - 1)).await;
        }
        let signature = sign_graphrt(&target.secret, super::unix_now_secs(), &body);
        let request = client
            .post(target.url.clone())
            .timeout(target.timeout)
            .header("content-type", "application/json")
            .header("user-agent", concat!("graphrt/", env!("CARGO_PKG_VERSION")))
            .header(GRAPHRT_SIGNATURE_HEADER, signature)
            .header("x-graphrt-event", format!("run.{status}"))
            .header("x-graphrt-delivery", &delivery_id)
            .header("idempotency-key", &delivery_id)
            .header("x-graphrt-attempt", attempt.to_string())
            .body(body.clone());
        match request.send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) => {
                let code = response.status().as_u16();
                last_error = format!("HTTP {code} (attempt {attempt})");
                let retryable = code == 408 || code == 429 || response.status().is_server_error();
                if !retryable {
                    return Err(last_error);
                }
            }
            Err(error) => last_error = format!("{error} (attempt {attempt})"),
        }
    }
    Err(last_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(url: &str) -> CallbackSpec {
        CallbackSpec {
            url: url.to_owned(),
            secret: super::super::SecretSpec { secret_env: Some("CB".into()), secret_file: None },
            on: None,
            max_attempts: None,
            backoff_ms: Some(100),
            timeout_ms: None,
        }
    }

    fn vars() -> Vec<(String, String)> {
        vec![("CB".into(), "secret".into())]
    }

    #[test]
    fn egress_policy_applies_to_callback_urls() {
        let blocked = CallbackTarget::from_spec(spec("http://127.0.0.1:9/done"), &vars(), false).unwrap_err();
        assert!(blocked.contains("not a public address"), "{blocked}");
        let metadata = CallbackTarget::from_spec(spec("http://169.254.169.254/x"), &vars(), false).unwrap_err();
        assert!(metadata.contains("not a public address"), "{metadata}");
        assert!(CallbackTarget::from_spec(spec("ftp://example.com/x"), &vars(), false).is_err());
        assert!(CallbackTarget::from_spec(spec("http://127.0.0.1:9/done"), &vars(), true).is_ok());
    }

    #[test]
    fn backoff_doubles_and_is_capped() {
        let target = CallbackTarget::from_spec(spec("https://example.com/cb"), &vars(), false).unwrap();
        assert_eq!(target.delay(1), Duration::from_millis(100));
        assert_eq!(target.delay(2), Duration::from_millis(200));
        assert_eq!(target.delay(4), Duration::from_millis(800));
        assert_eq!(target.delay(40), MAX_BACKOFF);
    }

    #[test]
    fn invalid_settings_are_rejected() {
        let mut bad = spec("https://example.com/cb");
        bad.on = Some(vec!["done".into()]);
        assert!(CallbackTarget::from_spec(bad, &vars(), false).unwrap_err().contains("unknown status"));
        let mut bad = spec("https://example.com/cb");
        bad.max_attempts = Some(0);
        assert!(CallbackTarget::from_spec(bad, &vars(), false).is_err());
        assert!(CallbackTarget::from_spec(spec("https://example.com/cb"), &[], false).unwrap_err().contains("CB"));
    }
}
