//! Webhook ingress (plan §2, §4): signature check, deterministic run id
//! `H(deployment, hook, key)` and the run record as the idempotency marker
//! (one create-only write; `202` only after it succeeded, so a retry of the
//! sender never meets a marker without a run, D2). Duplicate deliveries get
//! `{"duplicate": true}` with the existing run id (I4).
//!
//! Signature schemes match `graphrt serve --webhooks`: `github`
//! (`X-Hub-Signature-256: sha256=<hex>`), `hmac_sha256` (`X-Signature-256`),
//! `stripe` (`Stripe-Signature: t=..,v1=..`) and `graphrt`
//! (`X-Graphrt-Signature`, Stripe's format), or `none`.

use serde::Deserialize;

use crate::hex::{constant_time_eq, decode, encode, hmac_sha256, sha256_hex};

/// Timestamp tolerance of the timestamped schemes.
pub const TOLERANCE_SECS: u64 = 300;

/// Signature scheme of a hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scheme {
    /// No signature (only behind another authentication layer).
    None,
    /// GitHub.
    Github,
    /// Plain `X-Signature-256: sha256=<hex>`.
    #[default]
    HmacSha256,
    /// Stripe.
    Stripe,
    /// graphrt's own (Stripe format in `X-Graphrt-Signature`).
    Graphrt,
}

/// One hook of a deployment.
#[derive(Debug, Clone, Deserialize)]
pub struct HookConfig {
    /// Hook id in the URL `/v1/deployments/{d}/hooks/{id}`.
    pub id: String,
    /// Signature scheme.
    #[serde(default)]
    pub scheme: Scheme,
    /// Environment variable with the signing secret.
    #[serde(default)]
    pub secret_env: Option<String>,
    /// Header carrying the delivery's idempotency key (`Idempotency-Key`, `X-GitHub-Delivery`...).
    /// Without it the key is the SHA-256 of the body.
    #[serde(default)]
    pub idempotency_header: Option<String>,
    /// Input field receiving the parsed body (default `payload`).
    #[serde(default)]
    pub input: Option<String>,
    /// Completion callback URL (delivered through the outbox).
    #[serde(default)]
    pub callback_url: Option<String>,
}

/// Why a delivery was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HookError {
    /// Missing or wrong signature, or stale timestamp.
    #[error("invalid webhook signature")]
    BadSignature,
    /// The hook needs a secret that is not configured.
    #[error("webhook secret not configured")]
    NoSecret,
}

fn header<'a>(headers: &'a axum::http::HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// Verifies a delivery.
///
/// # Errors
/// See [`HookError`].
pub fn verify(
    hook: &HookConfig,
    secret: Option<&[u8]>,
    headers: &axum::http::HeaderMap,
    body: &[u8],
    now_secs: u64,
) -> Result<(), HookError> {
    if hook.scheme == Scheme::None {
        return Ok(());
    }
    let secret = secret.ok_or(HookError::NoSecret)?;
    let bad = HookError::BadSignature;
    match hook.scheme {
        Scheme::None => Ok(()),
        Scheme::Github | Scheme::HmacSha256 => {
            let name = if hook.scheme == Scheme::Github { "x-hub-signature-256" } else { "x-signature-256" };
            let value = header(headers, name).ok_or(bad.clone())?;
            let presented = decode(value.trim().strip_prefix("sha256=").ok_or(bad.clone())?).ok_or(bad.clone())?;
            let expected = hmac_sha256(secret, &[body]);
            if constant_time_eq(&expected, &presented) { Ok(()) } else { Err(bad) }
        }
        Scheme::Stripe | Scheme::Graphrt => {
            let name = if hook.scheme == Scheme::Stripe { "stripe-signature" } else { "x-graphrt-signature" };
            let value = header(headers, name).ok_or(bad.clone())?;
            let mut timestamp = None;
            let mut candidates = Vec::new();
            for part in value.split(',') {
                match part.trim().split_once('=') {
                    Some(("t", t)) => timestamp = t.parse::<u64>().ok(),
                    Some(("v1", mac)) => candidates.extend(decode(mac)),
                    _ => {}
                }
            }
            let timestamp = timestamp.ok_or(bad.clone())?;
            if timestamp.abs_diff(now_secs) > TOLERANCE_SECS {
                return Err(bad);
            }
            let expected = hmac_sha256(secret, &[timestamp.to_string().as_bytes(), b".", body]);
            if candidates.iter().any(|candidate| constant_time_eq(&expected, candidate)) { Ok(()) } else { Err(bad) }
        }
    }
}

/// `t=<unix>,v1=<hex>` signature of an outbound callback body.
pub fn sign_callback(secret: &[u8], timestamp: u64, body: &[u8]) -> String {
    format!("t={timestamp},v1={}", encode(&hmac_sha256(secret, &[timestamp.to_string().as_bytes(), b".", body])))
}

/// The idempotency key of a delivery: the configured header, else the body hash.
pub fn delivery_key(hook: &HookConfig, headers: &axum::http::HeaderMap, body: &[u8]) -> String {
    hook.idempotency_header
        .as_deref()
        .and_then(|name| header(headers, name))
        .map(str::to_owned)
        .filter(|key| !key.is_empty())
        .unwrap_or_else(|| sha256_hex(body))
}

/// Run id of a delivery: `hook-<hex(H(deployment, hook, key))[..32]>`.
pub fn hook_run_id(deployment: &str, hook: &str, key: &str) -> String {
    let digest = sha256_hex(format!("hook|{deployment}|{hook}|{key}").as_bytes());
    format!("hook-{}", digest.get(..32).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue};

    use super::*;

    fn hook(scheme: Scheme) -> HookConfig {
        HookConfig {
            id: "gh".into(),
            scheme,
            secret_env: None,
            idempotency_header: Some("x-github-delivery".into()),
            input: None,
            callback_url: None,
        }
    }

    #[test]
    fn github_and_stripe_signatures() {
        let body = b"{\"a\":1}";
        let mut headers = HeaderMap::new();
        let mac = encode(&hmac_sha256(b"s3cret", &[body]));
        headers.insert("x-hub-signature-256", HeaderValue::from_str(&format!("sha256={mac}")).unwrap());
        assert!(verify(&hook(Scheme::Github), Some(b"s3cret"), &headers, body, 0).is_ok());
        assert_eq!(verify(&hook(Scheme::Github), Some(b"other"), &headers, body, 0), Err(HookError::BadSignature));
        let mut stripe = HeaderMap::new();
        stripe.insert("stripe-signature", HeaderValue::from_str(&sign_callback(b"whsec", 1000, body)).unwrap());
        assert!(verify(&hook(Scheme::Stripe), Some(b"whsec"), &stripe, body, 1100).is_ok());
        assert_eq!(verify(&hook(Scheme::Stripe), Some(b"whsec"), &stripe, body, 2000), Err(HookError::BadSignature));
        assert_eq!(verify(&hook(Scheme::Stripe), None, &stripe, body, 1100), Err(HookError::NoSecret));
    }

    #[test]
    fn run_ids_are_namespaced_and_stable() {
        let mut headers = HeaderMap::new();
        headers.insert("x-github-delivery", HeaderValue::from_static("d-1"));
        let key = delivery_key(&hook(Scheme::Github), &headers, b"x");
        assert_eq!(key, "d-1");
        assert_eq!(hook_run_id("shop", "gh", &key), hook_run_id("shop", "gh", &key));
        assert_ne!(hook_run_id("shop", "gh", &key), hook_run_id("blog", "gh", &key));
        assert_eq!(delivery_key(&hook(Scheme::Github), &HeaderMap::new(), b"x"), sha256_hex(b"x"));
    }

    /// `graphrt serve --deployment shop` derives the same id (its webhooks
    /// test pins the same value), so a delivery maps to one run whichever of
    /// the two receives it.
    #[test]
    fn run_ids_match_graphrt_serve() {
        assert_eq!(hook_run_id("shop", "gh", "delivery-1"), "hook-a76099f62e977ef809c457991ceb5e7a");
    }
}
