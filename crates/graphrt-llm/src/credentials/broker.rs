//! Short-lived credentials from a token broker (your backend).
//!
//! ## Contract
//!
//! Request: `POST {url}` with JSON `{"provider": "<name>", ...claims}` and the
//! configured headers plus, when an app credential source is set,
//! `Authorization: Bearer <app token>` (e.g. the signed-in user's session token).
//!
//! Response (2xx, JSON, at most 64 KiB):
//!
//! ```json
//! {
//!   "token": "short-lived-secret",          // or "access_token"
//!   "expires_in": 900,                       // seconds (alias "expiresIn")
//!   "headers": { "x-tenant": "acme" },       // optional, sent with model requests
//!   "base_url": "https://proxy.example.com/v1" // optional endpoint override
//! }
//! ```
//!
//! Expiry may instead be given as `expires_at` / `expiresAt` (Unix time; values
//! above 10^11 are read as milliseconds, smaller ones as seconds) or
//! `expires_at_ms` / `expiresAtMs` (Unix milliseconds). A response without a usable
//! expiry is cached for [`TokenBrokerConfig::default_ttl`] (5 minutes), never
//! indefinitely.
//!
//! The broker URL and any returned `base_url` must be `https://` (plain http only
//! for localhost / 127.0.0.1 / 10.0.2.2 during development): an on-path attacker
//! must not be able to redirect prompts and keys.
//!
//! A 401/403 from the broker maps to [`LlmError::Auth`] (the user must sign in
//! again); other failures map like provider errors. Use
//! [`TokenBroker::cached`] so tokens are reused until shortly before expiry.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use futures::future::BoxFuture;
use serde::Deserialize;

use super::{CachedCredentials, Credential, CredentialSource, unix_now_ms};
use crate::error::LlmError;

/// Largest broker response accepted.
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// How the app authenticates to the broker.
#[derive(Clone, Default)]
#[non_exhaustive]
pub enum BrokerAuth {
    /// No app authentication (only for brokers behind other protection).
    #[default]
    None,
    /// `Authorization: Bearer <secret>` where the secret comes from this source,
    /// asked with provider name `"token_broker"` (e.g. the user's session token).
    Bearer(Arc<dyn CredentialSource>),
}

impl fmt::Debug for BrokerAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BrokerAuth::None => f.write_str("None"),
            BrokerAuth::Bearer(_) => f.write_str("Bearer(<source>)"),
        }
    }
}

/// Token broker settings.
#[derive(Clone)]
pub struct TokenBrokerConfig {
    pub url: String,
    /// Extra headers sent to the broker.
    pub headers: Vec<(String, String)>,
    /// Extra JSON fields sent with every request (e.g. app id, device id).
    pub claims: serde_json::Map<String, serde_json::Value>,
    pub auth: BrokerAuth,
    pub timeout: Duration,
    /// Lifetime assumed for tokens returned without an expiry.
    pub default_ttl: Duration,
}

/// Lifetime assumed for broker tokens that carry no expiry (5 minutes).
pub const DEFAULT_BROKER_TTL: Duration = Duration::from_secs(300);

impl TokenBrokerConfig {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: Vec::new(),
            claims: serde_json::Map::new(),
            auth: BrokerAuth::None,
            timeout: Duration::from_secs(15),
            default_ttl: DEFAULT_BROKER_TTL,
        }
    }

    /// Lifetime assumed for tokens returned without an expiry.
    pub fn default_ttl(mut self, ttl: Duration) -> Self {
        self.default_ttl = ttl;
        self
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn claim(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.claims.insert(key.into(), value);
        self
    }

    pub fn auth(mut self, auth: BrokerAuth) -> Self {
        self.auth = auth;
        self
    }
}

impl fmt::Debug for TokenBrokerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenBrokerConfig")
            .field("url", &self.url)
            .field("headers", &self.headers.iter().map(|(k, _)| k).collect::<Vec<_>>())
            .field("claims", &self.claims.keys().collect::<Vec<_>>())
            .field("auth", &self.auth)
            .finish()
    }
}

/// Fetches short-lived provider credentials from a broker endpoint.
#[derive(Clone)]
pub struct TokenBroker {
    config: TokenBrokerConfig,
    client: crate::http::LazyClient,
}

impl fmt::Debug for TokenBroker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenBroker").field("config", &self.config).finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct BrokerResponse {
    #[serde(alias = "access_token")]
    token: String,
    #[serde(default, alias = "expiresIn")]
    expires_in: Option<f64>,
    #[serde(default, alias = "expiresAt")]
    expires_at: Option<f64>,
    #[serde(default, alias = "expiresAtMs")]
    expires_at_ms: Option<u64>,
    #[serde(default)]
    headers: indexmap::IndexMap<String, String>,
    #[serde(default)]
    base_url: Option<String>,
}

impl TokenBroker {
    pub fn new(config: TokenBrokerConfig) -> Result<Self, LlmError> {
        crate::credentials::require_secure_endpoint(&config.url, "token broker url")?;
        let client = crate::http::LazyClient::new(Duration::from_secs(10));
        Ok(Self { config, client })
    }

    /// A broker wrapped in [`CachedCredentials`] (the recommended form).
    pub fn cached(config: TokenBrokerConfig) -> Result<CachedCredentials<Self>, LlmError> {
        Ok(CachedCredentials::new(Self::new(config)?))
    }

    async fn fetch(&self, provider: &str) -> Result<Credential, LlmError> {
        let mut body = self.config.claims.clone();
        body.insert("provider".into(), serde_json::Value::String(provider.to_owned()));
        let mut request = self.client.get()?.post(&self.config.url).timeout(self.config.timeout).json(&body);
        for (k, v) in &self.config.headers {
            request = request.header(k.as_str(), v.as_str());
        }
        if let BrokerAuth::Bearer(source) = &self.config.auth {
            let app = source.credential("token_broker").await?;
            request = request.bearer_auth(app.secret);
        }
        let response =
            request.send().await.map_err(|e| LlmError::Network(format!("token broker: {}", e.without_url())))?;
        let status = response.status().as_u16();
        let text = read_capped(response).await?;
        if !(200..300).contains(&status) {
            if status == 401 || status == 403 {
                if let BrokerAuth::Bearer(source) = &self.config.auth {
                    source.invalidate("token_broker");
                }
                return Err(LlmError::Auth(format!("token broker rejected the app credential (HTTP {status})")));
            }
            return Err(LlmError::from_status(status, &text, None));
        }
        let parsed: BrokerResponse =
            serde_json::from_str(&text).map_err(|e| LlmError::Decode(format!("token broker response: {e}")))?;
        if parsed.token.is_empty() {
            return Err(LlmError::Decode("token broker returned an empty token".into()));
        }
        let mut credential = Credential::new(parsed.token);
        let now = unix_now_ms();
        let expires = expiry_ms(parsed.expires_at_ms, parsed.expires_at, parsed.expires_in, now).unwrap_or_else(|| {
            now.saturating_add(u64::try_from(self.config.default_ttl.as_millis()).unwrap_or(u64::MAX))
        });
        credential = credential.expires_at(expires);
        for (k, v) in parsed.headers {
            credential = credential.header(k, v);
        }
        if let Some(url) = parsed.base_url.filter(|u| !u.is_empty()) {
            if !crate::credentials::is_secure_endpoint(&url) {
                return Err(LlmError::Decode("token broker returned a base_url that is not https".into()));
            }
            credential = credential.base_url(url);
        }
        Ok(credential)
    }
}

/// Unix milliseconds above this are treated as milliseconds in `expires_at`
/// (10^11 seconds is the year 5138; 10^11 milliseconds is 1973).
const MS_THRESHOLD: f64 = 1e11;

/// The absolute expiry from the response fields, or `None` when none is usable.
fn expiry_ms(at_ms: Option<u64>, at: Option<f64>, in_secs: Option<f64>, now: u64) -> Option<u64> {
    if let Some(ms) = at_ms.filter(|ms| *ms > 0) {
        return Some(ms);
    }
    if let Some(at) = at.filter(|v| v.is_finite() && *v > 0.0) {
        return Some(if at > MS_THRESHOLD { secs_to_ms(at / 1000.0) } else { secs_to_ms(at) });
    }
    in_secs.filter(|s| s.is_finite() && *s >= 0.0).map(|s| now.saturating_add(secs_to_ms(s)))
}

fn secs_to_ms(secs: f64) -> u64 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ms = (secs * 1000.0).min(u64::MAX as f64) as u64;
    ms
}

async fn read_capped(response: reqwest::Response) -> Result<String, LlmError> {
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| LlmError::Network(format!("token broker: {}", e.without_url())))?;
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(LlmError::Decode("token broker response exceeds 64 KiB".into()));
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|_| LlmError::Decode("token broker response is not UTF-8".into()))
}

impl CredentialSource for TokenBroker {
    fn credential<'a>(&'a self, provider: &'a str) -> BoxFuture<'a, Result<Credential, LlmError>> {
        Box::pin(self.fetch(provider))
    }

    /// Tokens minted for one app session must not outlive it: when the app
    /// credential source changes (sign-out, new user), cached tokens are dropped.
    fn generation(&self) -> u64 {
        match &self.config.auth {
            BrokerAuth::Bearer(source) => source.generation(),
            BrokerAuth::None => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_parsing() {
        let now = 1_700_000_000_000;
        assert_eq!(expiry_ms(Some(5), None, None, now), Some(5));
        // Seconds vs milliseconds in `expires_at`.
        assert_eq!(expiry_ms(None, Some(1_700_000_900.0), None, now), Some(1_700_000_900_000));
        assert_eq!(expiry_ms(None, Some(1_700_000_900_000.0), None, now), Some(1_700_000_900_000));
        assert_eq!(expiry_ms(None, None, Some(60.0), now), Some(now + 60_000));
        // Unusable values yield no expiry (the caller applies the default TTL).
        assert_eq!(expiry_ms(None, Some(f64::NAN), Some(-1.0), now), None);
        assert_eq!(expiry_ms(Some(0), Some(0.0), None, now), None);
    }

    #[test]
    fn camel_case_fields_and_insecure_urls() {
        let parsed: BrokerResponse = serde_json::from_str(r#"{"token":"t","expiresIn":30}"#).unwrap();
        assert_eq!(parsed.expires_in, Some(30.0));
        assert!(TokenBroker::new(TokenBrokerConfig::new("http://broker.example.com/token")).is_err());
        assert!(TokenBroker::new(TokenBrokerConfig::new("http://127.0.0.1:9/token")).is_ok());
        assert!(TokenBroker::new(TokenBrokerConfig::new("https://broker.example.com/token")).is_ok());
    }
}
