//! Service-account keys: an RS256-signed JWT assertion exchanged at the OAuth 2.0
//! token endpoint (RFC 7523 JWT bearer grant) for an access token or, with a
//! `target_audience` claim, a Google-signed ID token.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use graphrt_provider_api::ProviderError;
use graphrt_provider_api::http::{AccessToken, SecretString, parse_retry_after};
use ring::signature::RsaKeyPair;
use serde_json::Value;

use super::Grant;
use super::http_date::parse_imf_fixdate;
use super::jwt::{id_token_lifetime_secs, sign_rs256};
use crate::url_encoding::encode_component;

/// Lifetime requested for an assertion (Google's maximum is one hour).
const ASSERTION_LIFETIME_SECS: i64 = 3_600;
/// `iat` is backdated a little so a slightly fast local clock is not "in the future".
const ASSERTION_BACKDATE_SECS: i64 = 10;
/// A clock correction smaller than this is not worth a retry.
const MIN_SKEW_CORRECTION_SECS: i64 = 5;
/// Longest error-response excerpt kept in a message.
const ERROR_EXCERPT_CHARS: usize = 256;
const JWT_BEARER_GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";

/// A parsed service-account key file (`"type": "service_account"`).
///
/// `Debug` never prints the private key.
pub struct ServiceAccountKey {
    client_email: String,
    private_key_id: Option<String>,
    token_uri: String,
    project_id: Option<String>,
    key_pair: RsaKeyPair,
}

impl fmt::Debug for ServiceAccountKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServiceAccountKey")
            .field("client_email", &self.client_email)
            .field("private_key_id", &self.private_key_id)
            .field("token_uri", &self.token_uri)
            .field("private_key", &"***")
            .finish()
    }
}

impl ServiceAccountKey {
    /// Parses a key file as downloaded from IAM (`gcloud iam service-accounts keys create`).
    ///
    /// @param json - The key file contents
    /// @returns The key, with its PKCS#8 (or PKCS#1) RSA private key loaded into `ring`
    /// @throws `ProviderError` (fatal) when the JSON is not a service-account key, the
    ///   `token_uri` is not HTTPS, or the private key cannot be parsed. The message
    ///   never contains key material.
    ///
    /// @example
    /// let key = ServiceAccountKey::from_json(&std::fs::read_to_string(path)?)?;
    pub fn from_json(json: &str) -> Result<Self, ProviderError> {
        let file: Value = serde_json::from_str(json)
            .map_err(|error| ProviderError::fatal(format!("invalid key file JSON: {error}")))?;
        let field = |name: &str| file.get(name).and_then(Value::as_str).map(str::to_owned);
        match field("type").as_deref() {
            Some("service_account") => {}
            Some(other) => {
                return Err(ProviderError::fatal(format!(
                    "credential type `{other}` is not supported; use a service-account key or the metadata server"
                )));
            }
            None => return Err(ProviderError::fatal("key file has no `type`")),
        }
        let client_email =
            field("client_email").ok_or_else(|| ProviderError::fatal("key file has no `client_email`"))?;
        let token_uri = field("token_uri").unwrap_or_else(|| super::DEFAULT_TOKEN_ENDPOINT.to_owned());
        if !token_uri.starts_with("https://") {
            return Err(ProviderError::fatal("key file `token_uri` must be an https:// URL"));
        }
        let pem = field("private_key").ok_or_else(|| ProviderError::fatal("key file has no `private_key`"))?;
        Ok(Self {
            client_email,
            private_key_id: field("private_key_id"),
            token_uri,
            project_id: field("project_id"),
            key_pair: parse_private_key(&pem)?,
        })
    }

    /// The service account's e-mail (its principal), safe to log.
    pub fn client_email(&self) -> &str {
        &self.client_email
    }

    /// The project the key belongs to, when the file names one.
    pub fn project_id(&self) -> Option<&str> {
        self.project_id.as_deref()
    }

    /// The token endpoint named in the key file (also the assertion's `aud`).
    pub fn token_uri(&self) -> &str {
        &self.token_uri
    }
}

/// Loads a PEM `PRIVATE KEY` (PKCS#8) or `RSA PRIVATE KEY` (PKCS#1) into `ring`.
fn parse_private_key(pem: &str) -> Result<RsaKeyPair, ProviderError> {
    let invalid = || ProviderError::fatal("key file `private_key` is not a PEM RSA private key");
    let label = if pem.contains("-----BEGIN PRIVATE KEY-----") {
        "PRIVATE KEY"
    } else if pem.contains("-----BEGIN RSA PRIVATE KEY-----") {
        "RSA PRIVATE KEY"
    } else {
        return Err(invalid());
    };
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let body = pem.split(&begin).nth(1).and_then(|rest| rest.split(&end).next()).ok_or_else(invalid)?;
    let base64_text: String = body.chars().filter(|character| !character.is_whitespace()).collect();
    let der = STANDARD.decode(base64_text).map_err(|_| invalid())?;
    let parsed = if label == "PRIVATE KEY" { RsaKeyPair::from_pkcs8(&der) } else { RsaKeyPair::from_der(&der) };
    parsed.map_err(|_| invalid())
}

/// Exchanges signed assertions for tokens. Remembers the token endpoint's clock
/// offset, learned from its `Date` header when it rejects `iat`/`exp`, so a
/// skewed local clock (C8: ±5 min) costs one extra round trip once.
pub(crate) struct ServiceAccountExchange {
    key: Arc<ServiceAccountKey>,
    http: reqwest::Client,
    token_endpoint: String,
    server_offset_secs: Arc<AtomicI64>,
}

impl fmt::Debug for ServiceAccountExchange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServiceAccountExchange")
            .field("client_email", &self.key.client_email)
            .field("token_endpoint", &self.token_endpoint)
            .finish_non_exhaustive()
    }
}

/// Outcome of one token-endpoint call.
enum ExchangeOutcome {
    Token(AccessToken),
    /// The endpoint rejected the assertion's time window; its clock is at this unix time.
    ClockRejected {
        server_now: Option<i64>,
        error: ProviderError,
    },
}

impl ServiceAccountExchange {
    pub(crate) fn new(
        key: Arc<ServiceAccountKey>,
        http: reqwest::Client,
        token_endpoint: String,
        server_offset_secs: Arc<AtomicI64>,
    ) -> Self {
        Self { key, http, token_endpoint, server_offset_secs }
    }

    /// Fetches an access token (`Grant::Access`) or an ID token (`Grant::Identity`).
    pub(crate) async fn fetch(&self, grant: &Grant) -> Result<AccessToken, ProviderError> {
        match self.exchange_once(grant).await? {
            ExchangeOutcome::Token(token) => Ok(token),
            ExchangeOutcome::ClockRejected { server_now: Some(server_now), error } => {
                let corrected = server_now - unix_now();
                let previous = self.server_offset_secs.swap(corrected, Ordering::Relaxed);
                if (corrected - previous).abs() < MIN_SKEW_CORRECTION_SECS {
                    return Err(error);
                }
                match self.exchange_once(grant).await? {
                    ExchangeOutcome::Token(token) => Ok(token),
                    ExchangeOutcome::ClockRejected { error, .. } => Err(error),
                }
            }
            ExchangeOutcome::ClockRejected { server_now: None, error } => Err(error),
        }
    }

    async fn exchange_once(&self, grant: &Grant) -> Result<ExchangeOutcome, ProviderError> {
        let assertion = self.assertion(grant)?;
        let form = format!("grant_type={}&assertion={}", encode_component(JWT_BEARER_GRANT), assertion);
        let response = self
            .http
            .post(&self.token_endpoint)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(form)
            .send()
            .await
            .map_err(|error| ProviderError::transient(format!("token endpoint unreachable: {}", error_kind(&error))))?;
        let status = response.status().as_u16();
        let server_now = response
            .headers()
            .get("date")
            .and_then(|value| value.to_str().ok())
            .and_then(parse_imf_fixdate)
            .and_then(|seconds| i64::try_from(seconds).ok());
        let retry_after =
            response.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(parse_retry_after);
        let body: Value =
            response.bytes().await.ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or(Value::Null);
        if (200..300).contains(&status) {
            return parse_token_response(grant, &body).map(ExchangeOutcome::Token);
        }
        let error = token_endpoint_error(status, &body).with_retry_after(retry_after);
        if is_clock_rejection(&body) {
            return Ok(ExchangeOutcome::ClockRejected { server_now, error });
        }
        Err(error)
    }

    fn assertion(&self, grant: &Grant) -> Result<String, ProviderError> {
        let issued_at = unix_now() + self.server_offset_secs.load(Ordering::Relaxed) - ASSERTION_BACKDATE_SECS;
        let mut claims = serde_json::json!({
            "iss": self.key.client_email,
            "aud": self.key.token_uri,
            "iat": issued_at,
            "exp": issued_at + ASSERTION_LIFETIME_SECS,
        });
        if let Some(object) = claims.as_object_mut() {
            match grant {
                Grant::Access { scopes } => object.insert("scope".to_owned(), Value::String(scopes.join(" "))),
                Grant::Identity { audience } => {
                    object.insert("target_audience".to_owned(), Value::String(audience.clone()))
                }
            };
        }
        sign_rs256(&self.key.key_pair, self.key.private_key_id.as_deref(), &claims)
    }
}

fn parse_token_response(grant: &Grant, body: &Value) -> Result<AccessToken, ProviderError> {
    let field = |name: &str| body.get(name).and_then(Value::as_str);
    match grant {
        Grant::Access { .. } => {
            let token =
                field("access_token").ok_or_else(|| ProviderError::auth("token response has no access_token"))?;
            let expires_in = body.get("expires_in").and_then(Value::as_u64).unwrap_or(3_600);
            Ok(AccessToken { value: SecretString::new(token), expires_in: Duration::from_secs(expires_in) })
        }
        Grant::Identity { .. } => {
            let token = field("id_token").ok_or_else(|| ProviderError::auth("token response has no id_token"))?;
            let lifetime = id_token_lifetime_secs(token);
            Ok(AccessToken { value: SecretString::new(token), expires_in: Duration::from_secs(lifetime) })
        }
    }
}

/// OAuth error responses carry `error` and `error_description`, never secrets.
fn token_endpoint_error(status: u16, body: &Value) -> ProviderError {
    let code = body.get("error").and_then(Value::as_str).unwrap_or("unknown_error");
    let description: String =
        body.get("error_description").and_then(Value::as_str).unwrap_or("").chars().take(ERROR_EXCERPT_CHARS).collect();
    let message = format!("token endpoint HTTP {status}: {code}: {description}");
    match status {
        400 | 401 | 403 => ProviderError::auth(message),
        429 => ProviderError::rate_limited(message),
        500..=599 => ProviderError::transient(message),
        _ => ProviderError::fatal(message),
    }
}

/// Google answers `invalid_grant` with "Token must be a short-lived token ... in a
/// reasonable timeframe. Check your iat and exp values" when the clocks disagree.
fn is_clock_rejection(body: &Value) -> bool {
    let code = body.get("error").and_then(Value::as_str).unwrap_or("");
    let description = body.get("error_description").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
    code == "invalid_grant" && (description.contains("timeframe") || description.contains("iat"))
}

fn error_kind(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect error"
    } else {
        "request error"
    }
}

pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_service_account_credentials() {
        let error = ServiceAccountKey::from_json(r#"{"type":"authorized_user"}"#).unwrap_err();
        assert!(error.message.contains("authorized_user"), "{error}");
        let error = ServiceAccountKey::from_json(
            r#"{"type":"service_account","client_email":"a@b","private_key":"x","token_uri":"http://evil"}"#,
        )
        .unwrap_err();
        assert!(error.message.contains("https"), "{error}");
    }

    #[test]
    fn invalid_private_key_does_not_echo_material() {
        let error = ServiceAccountKey::from_json(
            r#"{"type":"service_account","client_email":"a@b","private_key":"-----BEGIN PRIVATE KEY-----\nU0VDUkVU\n-----END PRIVATE KEY-----\n"}"#,
        )
        .unwrap_err();
        assert!(!error.message.contains("U0VDUkVU"), "{error}");
    }

    #[test]
    fn recognises_clock_rejections() {
        let body = serde_json::json!({"error": "invalid_grant", "error_description": "Invalid JWT: Token must be a short-lived token (60 minutes) and in a reasonable timeframe. Check your iat and exp values in the JWT claim."});
        assert!(is_clock_rejection(&body));
        let body = serde_json::json!({"error": "invalid_grant", "error_description": "Invalid JWT Signature."});
        assert!(!is_clock_rejection(&body));
    }
}
