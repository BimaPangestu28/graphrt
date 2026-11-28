//! Google user credentials: the OAuth 2.0 refresh token of a person who signed
//! in with their Google account (`"type": "authorized_user"`, what
//! `gcloud auth application-default login` writes, or what an app such as the
//! graphrt desktop app gets from its own Google sign in), exchanged at the
//! token endpoint for access tokens (RFC 6749 section 6).
//!
//! Organizations that forbid service-account keys still let their people sign
//! in, so this is how a person deploys from their own machine.

use std::fmt;
use std::time::Duration;

use graphrt_provider_api::ProviderError;
use graphrt_provider_api::http::{AccessToken, SecretString, parse_retry_after};
use serde_json::Value;

use super::Grant;
use crate::url_encoding::encode_component;

/// Longest error-response excerpt kept in a message.
const ERROR_EXCERPT_CHARS: usize = 256;

/// A person's Google sign in: OAuth client id and secret plus a refresh token.
///
/// `Debug` never prints the client secret or the refresh token.
#[derive(Clone)]
pub struct UserCredentials {
    client_id: String,
    client_secret: SecretString,
    refresh_token: SecretString,
    token_uri: String,
    quota_project_id: Option<String>,
    account: Option<String>,
}

impl fmt::Debug for UserCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UserCredentials")
            .field("client_id", &self.client_id)
            .field("account", &self.account)
            .field("quota_project_id", &self.quota_project_id)
            .field("client_secret", &"***")
            .field("refresh_token", &"***")
            .finish()
    }
}

impl UserCredentials {
    /// Credentials from the OAuth client an app signed the person in with.
    ///
    /// @param client_id - OAuth client id (`<n>-<id>.apps.googleusercontent.com`)
    /// @param client_secret - its secret (installed app clients have one too)
    /// @param refresh_token - the refresh token of the sign in
    /// @example
    /// let user = UserCredentials::new(client_id, client_secret, refresh_token);
    /// let auth = GcpAuthBuilder::user(user).build()?;
    pub fn new(
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        refresh_token: impl Into<String>,
    ) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: SecretString::new(client_secret.into()),
            refresh_token: SecretString::new(refresh_token.into()),
            token_uri: super::DEFAULT_TOKEN_ENDPOINT.to_owned(),
            quota_project_id: None,
            account: None,
        }
    }

    /// Parses an `authorized_user` credentials file
    /// (`~/.config/gcloud/application_default_credentials.json`).
    ///
    /// @throws `ProviderError` (fatal) when a field is missing or `token_uri`
    ///   is not HTTPS. The message never carries the secret or the token.
    pub fn from_json(json: &str) -> Result<Self, ProviderError> {
        let file: Value = serde_json::from_str(json)
            .map_err(|error| ProviderError::fatal(format!("invalid credentials JSON: {error}")))?;
        Self::from_value(&file)
    }

    pub(crate) fn from_value(file: &Value) -> Result<Self, ProviderError> {
        let field = |name: &str| file.get(name).and_then(Value::as_str).filter(|value| !value.is_empty());
        let required = |name: &str| {
            field(name).map(str::to_owned).ok_or_else(|| {
                ProviderError::fatal(format!(
                    "user credentials have no `{name}`: sign in again with gcloud auth application-default login"
                ))
            })
        };
        let token_uri = field("token_uri").unwrap_or(super::DEFAULT_TOKEN_ENDPOINT).to_owned();
        if !token_uri.starts_with("https://") {
            return Err(ProviderError::fatal("user credentials `token_uri` must be an https:// URL"));
        }
        Ok(Self {
            client_id: required("client_id")?,
            client_secret: SecretString::new(required("client_secret")?),
            refresh_token: SecretString::new(required("refresh_token")?),
            token_uri,
            quota_project_id: field("quota_project_id").map(str::to_owned),
            account: field("account").map(str::to_owned),
        })
    }

    /// The same credentials, exchanged at `token_uri` (tests, private endpoints).
    pub fn with_token_uri(mut self, token_uri: impl Into<String>) -> Self {
        self.token_uri = token_uri.into();
        self
    }

    /// The same credentials with the e-mail of the person (for messages only).
    pub fn with_account(mut self, account: impl Into<String>) -> Self {
        self.account = Some(account.into());
        self
    }

    /// The project the credentials file names for quota, if any.
    pub fn quota_project_id(&self) -> Option<&str> {
        self.quota_project_id.as_deref()
    }

    /// Who signed in, safe to log: the account e-mail when known, else the OAuth client id.
    pub fn principal(&self) -> String {
        match &self.account {
            Some(account) => format!("user {account}"),
            None => format!("user (OAuth client {})", self.client_id),
        }
    }

    /// The token endpoint the refresh token is exchanged at.
    pub fn token_uri(&self) -> &str {
        &self.token_uri
    }
}

/// Exchanges the refresh token for access tokens.
pub(crate) struct UserRefresh {
    credentials: UserCredentials,
    http: reqwest::Client,
}

impl fmt::Debug for UserRefresh {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("UserRefresh").field("credentials", &self.credentials).finish_non_exhaustive()
    }
}

impl UserRefresh {
    pub(crate) fn new(credentials: UserCredentials, http: reqwest::Client) -> Self {
        Self { credentials, http }
    }

    pub(crate) fn quota_project_id(&self) -> Option<&str> {
        self.credentials.quota_project_id()
    }

    /// An access token. A person's sign in cannot mint ID tokens for an
    /// arbitrary audience (only a service account can), so `Grant::Identity` fails.
    pub(crate) async fn fetch(&self, grant: &Grant) -> Result<AccessToken, ProviderError> {
        if let Grant::Identity { audience } = grant {
            return Err(ProviderError::auth(format!(
                "a Google user sign in cannot mint ID tokens for {audience}: use a service account (key file or the \
                 metadata server) where the gateway calls private Cloud Run services"
            )));
        }
        let credentials = &self.credentials;
        let form = format!(
            "grant_type=refresh_token&client_id={}&client_secret={}&refresh_token={}",
            encode_component(&credentials.client_id),
            encode_component(credentials.client_secret.expose()),
            encode_component(credentials.refresh_token.expose()),
        );
        let response = self
            .http
            .post(&credentials.token_uri)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(form)
            .send()
            .await
            .map_err(|error| {
                ProviderError::transient(format!(
                    "Google sign in endpoint unreachable: {}",
                    if error.is_timeout() { "timeout" } else { "connect error" }
                ))
            })?;
        let status = response.status().as_u16();
        let retry_after =
            response.headers().get("retry-after").and_then(|value| value.to_str().ok()).and_then(parse_retry_after);
        let body: Value =
            response.bytes().await.ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or(Value::Null);
        if (200..300).contains(&status) {
            let token = body
                .get("access_token")
                .and_then(Value::as_str)
                .ok_or_else(|| ProviderError::auth("token response has no access_token"))?;
            let expires_in = body.get("expires_in").and_then(Value::as_u64).unwrap_or(3_600);
            return Ok(AccessToken { value: SecretString::new(token), expires_in: Duration::from_secs(expires_in) });
        }
        Err(refresh_error(status, &body, &credentials.principal()).with_retry_after(retry_after))
    }
}

/// `invalid_grant` means the sign in expired or was revoked (password change,
/// session length policy of the organization, removed access): only a new sign in helps.
fn refresh_error(status: u16, body: &Value, principal: &str) -> ProviderError {
    let code = body.get("error").and_then(Value::as_str).unwrap_or("unknown_error");
    let description: String =
        body.get("error_description").and_then(Value::as_str).unwrap_or("").chars().take(ERROR_EXCERPT_CHARS).collect();
    if code == "invalid_grant" {
        return ProviderError::auth(format!(
            "the Google sign in of {principal} expired or was revoked ({description}): sign in again \
             (gcloud auth application-default login, or sign in to Google again in the app)"
        ));
    }
    let message = format!("Google sign in endpoint HTTP {status}: {code}: {description}");
    match status {
        400 | 401 | 403 => ProviderError::auth(message),
        429 => ProviderError::rate_limited(message),
        500..=599 => ProviderError::transient(message),
        _ => ProviderError::fatal(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = r#"{"type":"authorized_user","client_id":"123-abc.apps.googleusercontent.com",
        "client_secret":"d-secret-value","refresh_token":"1//refresh-value","quota_project_id":"my-project",
        "account":"someone@example.com"}"#;

    #[test]
    fn parses_an_application_default_credentials_file() {
        let user = UserCredentials::from_json(FILE).unwrap();
        assert_eq!(user.quota_project_id(), Some("my-project"));
        assert_eq!(user.principal(), "user someone@example.com");
        assert_eq!(user.token_uri(), super::super::DEFAULT_TOKEN_ENDPOINT);
        let debug = format!("{user:?}");
        assert!(!debug.contains("d-secret-value") && !debug.contains("refresh-value"), "{debug}");
    }

    #[test]
    fn a_missing_field_says_how_to_sign_in_again() {
        let error = UserCredentials::from_json(r#"{"type":"authorized_user","client_id":"c","client_secret":"s"}"#)
            .unwrap_err();
        assert!(error.message.contains("refresh_token") && error.message.contains("application-default login"));
        let error = UserCredentials::from_json(
            r#"{"client_id":"c","client_secret":"s","refresh_token":"r","token_uri":"http://evil"}"#,
        )
        .unwrap_err();
        assert!(error.message.contains("https"), "{error}");
    }

    #[test]
    fn an_expired_sign_in_asks_for_a_new_one_without_the_token() {
        let body =
            serde_json::json!({"error": "invalid_grant", "error_description": "Token has been expired or revoked."});
        let error = refresh_error(400, &body, "user someone@example.com");
        assert_eq!(error.kind, graphrt_provider_api::ProviderErrorKind::Auth);
        assert!(error.message.contains("sign in again") && error.message.contains("someone@example.com"));
    }
}
