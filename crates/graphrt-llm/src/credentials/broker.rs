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
//!   "expires_in": 900,                       // seconds; or "expires_at" (Unix seconds)
//!                                            //   or "expires_at_ms" (Unix milliseconds)
//!   "headers": { "x-tenant": "acme" },       // optional, sent with model requests
//!   "base_url": "https://proxy.example.com/v1" // optional endpoint override
//! }
//! ```
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
}

impl TokenBrokerConfig {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: Vec::new(),
            claims: serde_json::Map::new(),
            auth: BrokerAuth::None,
            timeout: Duration::from_secs(15),
        }
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
    client: reqwest::Client,
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
    #[serde(default)]
    expires_in: Option<f64>,
    #[serde(default)]
    expires_at: Option<f64>,
    #[serde(default)]
    expires_at_ms: Option<u64>,
    #[serde(default)]
    headers: indexmap::IndexMap<String, String>,
    #[serde(default)]
    base_url: Option<String>,
}

impl TokenBroker {
    pub fn new(config: TokenBrokerConfig) -> Result<Self, LlmError> {
        if !(config.url.starts_with("https://") || config.url.starts_with("http://")) {
            return Err(LlmError::Config("token broker url must be http(s)".into()));
        }
        let client = crate::http::build_client(Duration::from_secs(10))?;
        Ok(Self { config, client })
    }

    /// A broker wrapped in [`CachedCredentials`] (the recommended form).
    pub fn cached(config: TokenBrokerConfig) -> Result<CachedCredentials<Self>, LlmError> {
        Ok(CachedCredentials::new(Self::new(config)?))
    }

    async fn fetch(&self, provider: &str) -> Result<Credential, LlmError> {
        let mut body = self.config.claims.clone();
        body.insert("provider".into(), serde_json::Value::String(provider.to_owned()));
        let mut request = self.client.post(&self.config.url).timeout(self.config.timeout).json(&body);
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
        let expires = parsed
            .expires_at_ms
            .or_else(|| parsed.expires_at.filter(|s| s.is_finite() && *s > 0.0).map(secs_to_ms))
            .or_else(|| {
                parsed
                    .expires_in
                    .filter(|s| s.is_finite() && *s >= 0.0)
                    .map(|s| unix_now_ms().saturating_add(secs_to_ms(s)))
            });
        if let Some(exp) = expires {
            credential = credential.expires_at(exp);
        }
        for (k, v) in parsed.headers {
            credential = credential.header(k, v);
        }
        if let Some(url) = parsed.base_url.filter(|u| !u.is_empty()) {
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                return Err(LlmError::Decode("token broker returned a non-http base_url".into()));
            }
            credential = credential.base_url(url);
        }
        Ok(credential)
    }
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
}
