//! Credentials of a source: a bearer token, an API key header, basic auth, or
//! an OAuth refresh token that is exchanged for short lived access tokens.
//!
//! Configs never hold a secret: they name it (`token_env: NOTION_TOKEN`) and
//! the host resolves the name with its secret store (environment variables
//! and env files for `graphrt serve`, the OS keychain for the desktop app).
//! [`Secret`] values print as `***` in `Debug` output, so they cannot leak
//! into logs or error messages by accident.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::{ConnectError, Result};

/// A credential value. `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The value, for building a request header.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// Resolves a secret name to its value (`None` when it is not set).
pub type SecretLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// How a source authenticates, as written in its config.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthConfig {
    /// No credential (public web pages and APIs).
    #[default]
    None,
    /// `Authorization: Bearer <token>`.
    Bearer { token_env: String },
    /// A custom header, such as `X-Api-Key: <value>`.
    Header { name: String, value_env: String },
    /// HTTP basic authentication.
    Basic { username: String, password_env: String },
    /// OAuth 2.0: the refresh token is exchanged at `token_url` for access
    /// tokens, refreshed before they expire and after a 401.
    Oauth {
        token_url: String,
        client_id: String,
        /// Confidential clients only; public clients (PKCE) have no secret.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_secret_env: Option<String>,
        refresh_token_env: String,
    },
}

/// A new refresh token handed out by the token endpoint (Microsoft rotates
/// them on every refresh). Hosts that can store it should, or the next start
/// signs in with the old one, which may have been revoked.
#[derive(Clone, Debug)]
pub struct RotatedRefreshToken {
    pub refresh_token: Secret,
}

/// Called with every rotated refresh token.
pub type OnRotate = Arc<dyn Fn(RotatedRefreshToken) + Send + Sync>;

/// A resolved credential, ready to sign requests.
#[derive(Clone, Debug, Default)]
pub enum Auth {
    #[default]
    None,
    Bearer(Secret),
    Header {
        name: String,
        value: Secret,
    },
    Basic {
        username: String,
        password: Secret,
    },
    Oauth(Arc<OAuthTokens>),
}

fn required(lookup: SecretLookup<'_>, name: &str, what: &str) -> Result<Secret> {
    match lookup(name).map(|value| value.trim().to_owned()).filter(|value| !value.is_empty()) {
        Some(value) => Ok(Secret(value)),
        None => Err(ConnectError::Auth(format!("{what} is not set (secret `{name}`); connect the account again"))),
    }
}

impl AuthConfig {
    /// Resolve the secret names of this config.
    ///
    /// @throws `ConnectError::Auth` when a named secret is not set.
    pub fn resolve(&self, lookup: SecretLookup<'_>) -> Result<Auth> {
        Ok(match self {
            Self::None => Auth::None,
            Self::Bearer { token_env } => Auth::Bearer(required(lookup, token_env, "the access token")?),
            Self::Header { name, value_env } => {
                Auth::Header { name: name.clone(), value: required(lookup, value_env, "the API key")? }
            }
            Self::Basic { username, password_env } => {
                Auth::Basic { username: username.clone(), password: required(lookup, password_env, "the password")? }
            }
            Self::Oauth { token_url, client_id, client_secret_env, refresh_token_env } => {
                let client_secret = match client_secret_env {
                    Some(name) => Some(required(lookup, name, "the OAuth client secret")?),
                    None => None,
                };
                Auth::Oauth(Arc::new(OAuthTokens::new(
                    token_url.clone(),
                    client_id.clone(),
                    client_secret,
                    required(lookup, refresh_token_env, "the sign-in (refresh token)")?,
                )))
            }
        })
    }
}

/// Refreshes access tokens from a refresh token and caches them until shortly
/// before they expire.
pub struct OAuthTokens {
    token_url: String,
    client_id: String,
    client_secret: Option<Secret>,
    state: Mutex<TokenState>,
    on_rotate: Option<OnRotate>,
    /// Serializes refreshes, so parallel fetches refresh once.
    refreshing: tokio::sync::Mutex<()>,
}

struct TokenState {
    refresh_token: Secret,
    access: Option<(Secret, Instant)>,
}

impl std::fmt::Debug for OAuthTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthTokens").field("token_url", &self.token_url).field("client_id", &self.client_id).finish()
    }
}

/// Access tokens are renewed this long before they expire.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);
/// Lifetime assumed when the token endpoint does not say.
const DEFAULT_LIFETIME: Duration = Duration::from_secs(3_000);
const MAX_TOKEN_RESPONSE: usize = 64 * 1024;

impl OAuthTokens {
    pub fn new(token_url: String, client_id: String, client_secret: Option<Secret>, refresh_token: Secret) -> Self {
        Self {
            token_url,
            client_id,
            client_secret,
            state: Mutex::new(TokenState { refresh_token, access: None }),
            on_rotate: None,
            refreshing: tokio::sync::Mutex::new(()),
        }
    }

    /// Start with an access token that is still valid for `valid_for`.
    pub fn with_access_token(self, token: Secret, valid_for: Duration) -> Self {
        self.lock().access = Some((token, Instant::now() + valid_for));
        self
    }

    /// Call `on_rotate` whenever the token endpoint hands out a new refresh token.
    pub fn on_rotate(mut self, on_rotate: OnRotate) -> Self {
        self.on_rotate = Some(on_rotate);
        self
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TokenState> {
        // Every update replaces whole values, so a poisoned state is still consistent.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn cached(&self) -> Option<Secret> {
        let state = self.lock();
        state
            .access
            .as_ref()
            .filter(|(_, until)| Instant::now() + EXPIRY_MARGIN < *until)
            .map(|(token, _)| token.clone())
    }

    /// Forget the cached access token (after the service answered 401).
    pub fn invalidate(&self) {
        self.lock().access = None;
    }

    /// A valid access token, refreshed when needed.
    ///
    /// @throws `ConnectError::Auth` when the refresh token was refused.
    pub async fn access_token(&self, http: &reqwest::Client) -> Result<Secret> {
        if let Some(token) = self.cached() {
            return Ok(token);
        }
        let _turn = self.refreshing.lock().await;
        if let Some(token) = self.cached() {
            return Ok(token);
        }
        let refresh_token = self.lock().refresh_token.clone();
        let form = {
            let mut form = url::form_urlencoded::Serializer::new(String::new());
            form.append_pair("grant_type", "refresh_token");
            form.append_pair("refresh_token", refresh_token.expose());
            form.append_pair("client_id", &self.client_id);
            if let Some(secret) = &self.client_secret {
                form.append_pair("client_secret", secret.expose());
            }
            form.finish()
        };
        let response = http
            .post(&self.token_url)
            .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(reqwest::header::ACCEPT, "application/json")
            .body(form)
            .send()
            .await
            .map_err(|e| ConnectError::Network(format!("could not reach the sign-in server: {}", e.without_url())))?;
        let status = response.status().as_u16();
        let body = crate::http::read_limited(response, MAX_TOKEN_RESPONSE).await?;
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
        if !(200..300).contains(&status) {
            // `error` is a fixed code (`invalid_grant`), never a token.
            let code = json.get("error").and_then(serde_json::Value::as_str).unwrap_or("refused");
            return Err(if status == 400 || status == 401 {
                ConnectError::Auth(format!("the sign-in expired or was revoked ({code}); connect the account again"))
            } else {
                ConnectError::Http {
                    status,
                    message: format!("the sign-in server answered with HTTP {status} ({code})"),
                }
            });
        }
        let access = json
            .get("access_token")
            .and_then(serde_json::Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| ConnectError::Decode("the sign-in server returned no access token".into()))?;
        let lifetime = json
            .get("expires_in")
            .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .map_or(DEFAULT_LIFETIME, Duration::from_secs);
        let rotated = json
            .get("refresh_token")
            .and_then(serde_json::Value::as_str)
            .filter(|token| !token.is_empty() && *token != refresh_token.expose())
            .map(Secret::new);
        let token = Secret::new(access);
        {
            let mut state = self.lock();
            state.access = Some((token.clone(), Instant::now() + lifetime));
            if let Some(new_refresh) = &rotated {
                state.refresh_token = new_refresh.clone();
            }
        }
        if let (Some(new_refresh), Some(on_rotate)) = (rotated, &self.on_rotate) {
            on_rotate(RotatedRefreshToken { refresh_token: new_refresh });
        }
        Ok(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_never_print() {
        let auth = AuthConfig::Bearer { token_env: "T".into() }.resolve(&|_| Some("s3cr3t\n".into())).unwrap();
        let text = format!("{auth:?}");
        assert!(!text.contains("s3cr3t"), "{text}");
        let Auth::Bearer(secret) = auth else { panic!("bearer") };
        assert_eq!(secret.expose(), "s3cr3t", "trailing newline of a key file is dropped");
    }

    #[test]
    fn missing_secret_names_it_without_a_value() {
        let error = AuthConfig::Header { name: "X-Api-Key".into(), value_env: "SHOP_KEY".into() }
            .resolve(&|_| None)
            .unwrap_err();
        assert_eq!(error.kind(), "auth");
        assert!(error.to_string().contains("SHOP_KEY"));
    }

    #[test]
    fn config_round_trips() {
        let json = serde_json::json!({
            "type": "oauth", "token_url": "https://oauth2.googleapis.com/token",
            "client_id": "app", "refresh_token_env": "DRIVE_REFRESH"
        });
        let config: AuthConfig = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(serde_json::to_value(&config).unwrap(), json);
    }
}
