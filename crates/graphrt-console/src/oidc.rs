//! OpenID Connect sign in: the authorization code flow with PKCE (S256), a
//! `state` bound to the browser and a `nonce` bound to the ID token.
//!
//! The provider is described by its discovery document, fetched on the first
//! sign in (a provider that is down at startup does not stop the server) and
//! again after a failure. The ID token's signature is checked by an
//! [`IdTokenVerifier`] the host supplies (JWKS, RS256 / ES256); the console
//! checks `iss`, `aud`, `azp`, `exp` and `nonce` itself.

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::config::{OidcConfig, check_provider_url};
use crate::error::{ConsoleError, Result};
use crate::util::{b64url, constant_time_eq, now_ms};

/// Timeout of every request to the provider.
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(10);
/// The largest discovery document or token answer read.
const MAX_ANSWER_BYTES: usize = 256 * 1024;
/// Clock skew allowed for `exp`, `nbf` and `iat`.
const LEEWAY_SECS: f64 = 60.0;

/// Claims of an ID token whose signature was checked.
pub type Claims = serde_json::Map<String, serde_json::Value>;

/// Checks the signature of ID tokens.
pub trait IdTokenVerifier: Send + Sync + std::fmt::Debug {
    /// The claims of `token` when its signature is valid for a key of the provider.
    ///
    /// @throws String - Why the token is not valid.
    fn verify<'a>(&'a self, token: &'a str) -> BoxFuture<'a, std::result::Result<Claims, String>>;
}

/// Makes the [`IdTokenVerifier`] of a provider from its `jwks_uri`.
pub trait VerifierSource: Send + Sync + std::fmt::Debug {
    /// A verifier of the keys at `jwks_uri`.
    ///
    /// @throws String - When the keys cannot be fetched.
    fn connect<'a>(&'a self, jwks_uri: &'a str)
    -> BoxFuture<'a, std::result::Result<Arc<dyn IdTokenVerifier>, String>>;
}

/// What the discovery document says.
#[derive(Clone, Debug, Deserialize)]
pub struct ProviderMetadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
    #[serde(default)]
    pub end_session_endpoint: Option<String>,
    #[serde(default)]
    pub token_endpoint_auth_methods_supported: Vec<String>,
}

#[derive(Debug)]
struct Connected {
    metadata: ProviderMetadata,
    verifier: Arc<dyn IdTokenVerifier>,
}

/// The identity provider of the console.
#[derive(Debug)]
pub struct Provider {
    config: OidcConfig,
    client_secret: Option<String>,
    redirect_uri: String,
    http: reqwest::Client,
    verifiers: Arc<dyn VerifierSource>,
    connected: Mutex<Option<Arc<Connected>>>,
}

/// What a code exchange gives.
#[derive(Debug, Deserialize)]
struct TokenAnswer {
    id_token: Option<String>,
}

impl Provider {
    /// The provider of `config`; nothing is fetched yet.
    ///
    /// @param redirect_uri - `<public url>/console/callback`.
    /// @throws ConsoleError::Config - When the HTTP client cannot be built.
    pub fn new(
        config: OidcConfig,
        client_secret: Option<String>,
        redirect_uri: String,
        verifiers: Arc<dyn VerifierSource>,
    ) -> Result<Self> {
        // reqwest is built without a default TLS provider: use ring, as the rest of graphrt does.
        let _already_installed = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .connect_timeout(PROVIDER_TIMEOUT)
            .timeout(PROVIDER_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("graphrt-console/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| ConsoleError::Config(format!("cannot build the HTTP client: {error}")))?;
        Ok(Self { config, client_secret, redirect_uri, http, verifiers, connected: Mutex::new(None) })
    }

    /// The issuer as configured.
    pub fn issuer(&self) -> &str {
        &self.config.issuer
    }

    async fn connected(&self) -> Result<Arc<Connected>> {
        let mut slot = self.connected.lock().await;
        if let Some(connected) = slot.as_ref() {
            return Ok(Arc::clone(connected));
        }
        let metadata = self.discover().await?;
        let verifier = self
            .verifiers
            .connect(&metadata.jwks_uri)
            .await
            .map_err(|why| ConsoleError::SignIn(format!("cannot read the provider's keys: {why}")))?;
        let connected = Arc::new(Connected { metadata, verifier });
        *slot = Some(Arc::clone(&connected));
        Ok(connected)
    }

    /// Forget the discovery document and keys (after a failed sign in).
    pub async fn reset(&self) {
        *self.connected.lock().await = None;
    }

    async fn discover(&self) -> Result<ProviderMetadata> {
        let url = format!("{}/.well-known/openid-configuration", self.config.issuer.trim_end_matches('/'));
        let body = self.read(self.http.get(&url)).await?;
        let metadata: ProviderMetadata = serde_json::from_slice(&body)
            .map_err(|error| ConsoleError::SignIn(format!("the discovery document cannot be read: {error}")))?;
        // OpenID Connect Discovery 1.0, section 4.3: the issuer must be the one asked about.
        if metadata.issuer.trim_end_matches('/') != self.config.issuer.trim_end_matches('/') {
            return Err(ConsoleError::SignIn(format!(
                "the provider names its issuer `{}`, not `{}`",
                metadata.issuer, self.config.issuer
            )));
        }
        for endpoint in [&metadata.authorization_endpoint, &metadata.token_endpoint, &metadata.jwks_uri] {
            check_provider_url(endpoint).map_err(ConsoleError::SignIn)?;
        }
        Ok(metadata)
    }

    async fn read(&self, request: reqwest::RequestBuilder) -> Result<Vec<u8>> {
        let failed = |why: String| ConsoleError::SignIn(why);
        let mut response = request.send().await.map_err(|error| failed(error.without_url().to_string()))?;
        let status = response.status();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| failed(error.without_url().to_string()))? {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_ANSWER_BYTES {
                return Err(failed(format!("the provider answered with more than {MAX_ANSWER_BYTES} bytes")));
            }
        }
        if !status.is_success() {
            // The error of a token endpoint is `{error, error_description}`: no secret in it.
            let reason = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|value| value.get("error").and_then(serde_json::Value::as_str).map(str::to_owned))
                .unwrap_or_default();
            return Err(failed(format!("the provider answered HTTP {status} {reason}").trim_end().to_owned()));
        }
        Ok(body)
    }

    /// The address to send the browser to.
    ///
    /// @throws ConsoleError::SignIn - When the provider cannot be reached.
    pub async fn authorization_url(&self, state: &str, nonce: &str, verifier: &str) -> Result<String> {
        let connected = self.connected().await?;
        let mut url = reqwest::Url::parse(&connected.metadata.authorization_endpoint)
            .map_err(|_| ConsoleError::SignIn("the authorization endpoint is not an address".into()))?;
        {
            let mut query = url.query_pairs_mut();
            for (name, value) in &self.config.auth_params {
                query.append_pair(name, value);
            }
            query
                .append_pair("response_type", "code")
                .append_pair("client_id", &self.config.client_id)
                .append_pair("redirect_uri", &self.redirect_uri)
                .append_pair("scope", &self.config.scopes.join(" "))
                .append_pair("state", state)
                .append_pair("nonce", nonce)
                .append_pair("code_challenge", &pkce_challenge(verifier))
                .append_pair("code_challenge_method", "S256");
        }
        Ok(url.into())
    }

    /// Exchange the authorization `code` and check the ID token it gives.
    ///
    /// @returns The verified claims.
    /// @throws ConsoleError::SignIn - When the exchange fails or the token is not valid.
    pub async fn exchange(&self, code: &str, verifier: &str, nonce: &str) -> Result<Claims> {
        let connected = self.connected().await?;
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("code_verifier", verifier),
        ];
        let mut request = self.http.post(&connected.metadata.token_endpoint);
        let methods = &connected.metadata.token_endpoint_auth_methods_supported;
        match &self.client_secret {
            // RFC 6749 2.3.1: every server takes HTTP Basic; the form is the alternative.
            Some(secret) if methods.iter().any(|m| m == "client_secret_post") || methods.is_empty() => {
                form.push(("client_id", self.config.client_id.as_str()));
                form.push(("client_secret", secret.as_str()));
            }
            Some(secret) => {
                request = request.basic_auth(form_encode(&self.config.client_id), Some(form_encode(secret)));
            }
            None => form.push(("client_id", self.config.client_id.as_str())),
        }
        let body = self.read(request.header("accept", "application/json").form(&form)).await?;
        let answer: TokenAnswer = serde_json::from_slice(&body)
            .map_err(|error| ConsoleError::SignIn(format!("the token answer cannot be read: {error}")))?;
        let id_token = answer.id_token.ok_or_else(|| ConsoleError::SignIn("the provider gave no ID token".into()))?;
        let claims = connected
            .verifier
            .verify(&id_token)
            .await
            .map_err(|why| ConsoleError::SignIn(format!("the ID token is not valid: {why}")))?;
        check_claims(&claims, &connected.metadata.issuer, &self.config.client_id, nonce, now_ms() / 1000)?;
        Ok(claims)
    }

    /// Where the browser signs out at the provider too, when it says.
    pub async fn end_session_url(&self) -> Option<String> {
        let connected = self.connected.lock().await.clone()?;
        connected.metadata.end_session_endpoint.clone()
    }
}

/// `application/x-www-form-urlencoded` of one value (client credentials in Basic auth).
fn form_encode(value: &str) -> String {
    reqwest::Url::parse_with_params("x:", [("v", value)])
        .ok()
        .and_then(|url| url.query().map(|q| q.trim_start_matches("v=").to_owned()))
        .unwrap_or_else(|| value.to_owned())
}

/// The S256 challenge of a PKCE `verifier` (RFC 7636).
pub fn pkce_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    b64url(&Sha256::digest(verifier.as_bytes()))
}

/// The checks of OpenID Connect Core 1.0, section 3.1.3.7, besides the signature.
///
/// @throws ConsoleError::SignIn - On the first claim that does not hold.
pub fn check_claims(claims: &Claims, issuer: &str, client_id: &str, nonce: &str, now_secs: u64) -> Result<()> {
    let refused = |why: &str| Err(ConsoleError::SignIn(format!("the ID token {why}")));
    let text = |name: &str| claims.get(name).and_then(serde_json::Value::as_str);
    if text("iss").map(|iss| iss.trim_end_matches('/')) != Some(issuer.trim_end_matches('/')) {
        return refused("names another issuer");
    }
    let audiences: Vec<&str> = match claims.get("aud") {
        Some(serde_json::Value::String(aud)) => vec![aud.as_str()],
        Some(serde_json::Value::Array(auds)) => auds.iter().filter_map(serde_json::Value::as_str).collect(),
        _ => Vec::new(),
    };
    if !audiences.contains(&client_id) {
        return refused("is not for this client");
    }
    if audiences.len() > 1 && text("azp") != Some(client_id) {
        return refused("was issued to another party (azp)");
    }
    let now = now_secs as f64;
    let number = |name: &str| claims.get(name).and_then(serde_json::Value::as_f64);
    match number("exp") {
        Some(exp) if now < exp + LEEWAY_SECS => {}
        _ => return refused("expired"),
    }
    if number("nbf").is_some_and(|nbf| now + LEEWAY_SECS < nbf)
        || number("iat").is_some_and(|iat| now + LEEWAY_SECS < iat)
    {
        return refused("is not valid yet");
    }
    match text("nonce") {
        Some(found) if constant_time_eq(found.as_bytes(), nonce.as_bytes()) => Ok(()),
        _ => refused("does not carry this sign in's nonce"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: u64 = 1_800_000_000;

    fn claims(value: serde_json::Value) -> Claims {
        value.as_object().cloned().unwrap()
    }

    fn good() -> serde_json::Value {
        json!({"iss": "https://idp.example", "aud": "graphrt", "sub": "1", "exp": NOW + 300, "iat": NOW, "nonce": "n1"})
    }

    #[test]
    fn a_valid_token_passes() {
        assert!(check_claims(&claims(good()), "https://idp.example/", "graphrt", "n1", NOW).is_ok());
    }

    #[test]
    fn each_claim_is_checked() {
        let cases = [
            ("iss", json!("https://evil.example")),
            ("aud", json!("other")),
            ("exp", json!(NOW - 120)),
            ("nonce", json!("n2")),
            ("iat", json!(NOW + 3600)),
        ];
        for (name, value) in cases {
            let mut token = good();
            token[name] = value;
            assert!(check_claims(&claims(token), "https://idp.example", "graphrt", "n1", NOW).is_err(), "{name}");
        }
        let mut no_nonce = good();
        no_nonce.as_object_mut().unwrap().remove("nonce");
        assert!(check_claims(&claims(no_nonce), "https://idp.example", "graphrt", "n1", NOW).is_err());
    }

    #[test]
    fn several_audiences_need_azp() {
        let mut token = good();
        token["aud"] = json!(["graphrt", "other"]);
        assert!(check_claims(&claims(token.clone()), "https://idp.example", "graphrt", "n1", NOW).is_err());
        token["azp"] = json!("graphrt");
        assert!(check_claims(&claims(token), "https://idp.example", "graphrt", "n1", NOW).is_ok());
    }

    #[test]
    fn pkce_challenge_matches_rfc_7636() {
        // Appendix B of RFC 7636.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn basic_auth_credentials_are_form_encoded() {
        assert_eq!(form_encode("a b+c/d"), "a+b%2Bc%2Fd");
        assert_eq!(form_encode("plain"), "plain");
    }
}
