//! Client authentication on the hot path (plan §2): per-deployment API keys
//! and end-user JWTs whose `iss` / `aud` are configured per deployment, with a
//! JWKS cache. Nothing here touches the store.
//!
//! - API keys are configured as SHA-256 hex digests; the plain key is shown
//!   once by `graphrt gateway key new`. Lookup hashes the presented key, so the
//!   comparison never runs over the secret itself.
//! - JWTs: HS256 (shared secret), RS256 and ES256 (JWKS). `exp` is required,
//!   `nbf` honoured, 60 s leeway. The verified token is kept by the caller in
//!   memory only (decision 7) and forwarded to the agent as `user_jwt`.
//! - JWKS documents are fetched lazily, cached for 10 minutes, and re-fetched
//!   at most every 30 s when a token names an unknown `kid`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio::time::Instant;

/// Accepted clock difference for `exp` / `nbf`.
pub const LEEWAY_SECS: u64 = 60;
const JWKS_TTL: Duration = Duration::from_secs(600);
const JWKS_MIN_REFETCH: Duration = Duration::from_secs(30);
const MAX_TOKEN_BYTES: usize = 16 * 1024;

/// Why a request was refused. Messages name no secret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// No credentials.
    #[error("missing credentials")]
    Missing,
    /// API key unknown or for another deployment.
    #[error("invalid API key")]
    BadKey,
    /// Token malformed, bad signature, wrong issuer/audience, expired.
    #[error("invalid token: {0}")]
    BadToken(&'static str),
    /// JWKS could not be fetched.
    #[error("token keys unavailable")]
    KeysUnavailable,
}

/// JWT settings of one deployment.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct JwtConfig {
    /// Expected `iss`.
    pub issuer: String,
    /// Expected `aud` (one of the token's audiences).
    pub audience: String,
    /// JWKS URL (RS256 / ES256). `https://` only, except `http://127.0.0.1` / `localhost` for development.
    #[serde(default)]
    pub jwks_url: Option<String>,
    /// Environment variable holding an HS256 secret.
    #[serde(default)]
    pub hs256_secret_env: Option<String>,
}

/// Who is calling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// A deployment API key (service-to-service).
    ApiKey,
    /// An end user.
    User {
        /// `sub` claim.
        subject: String,
        /// `exp` claim (unix seconds): the in-memory copy of the token is dropped then.
        expires_at: u64,
    },
}

#[derive(Debug, Deserialize)]
struct Header {
    alg: String,
    #[serde(default)]
    kid: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Deserialize)]
struct Claims {
    #[serde(default)]
    iss: Option<String>,
    #[serde(default)]
    aud: Option<Audience>,
    #[serde(default)]
    sub: Option<String>,
    exp: Option<u64>,
    #[serde(default)]
    nbf: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct Jwk {
    kty: String,
    #[serde(default)]
    kid: Option<String>,
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    e: Option<String>,
    #[serde(default)]
    crv: Option<String>,
    #[serde(default)]
    x: Option<String>,
    #[serde(default)]
    y: Option<String>,
}

#[derive(Debug, Deserialize)]
struct JwkSet {
    keys: Vec<Jwk>,
}

#[derive(Debug, Default)]
struct JwksState {
    keys: Vec<Jwk>,
    fetched_at: Option<Instant>,
}

/// Cache of one JWKS URL.
#[derive(Debug)]
pub struct JwksCache {
    url: String,
    client: reqwest::Client,
    state: Mutex<JwksState>,
}

impl JwksCache {
    /// A cache for `url`.
    ///
    /// # Errors
    /// When the URL is not `https://` (or local `http://`).
    pub fn new(url: &str, client: reqwest::Client) -> Result<Self, String> {
        let local = url.starts_with("http://127.0.0.1") || url.starts_with("http://localhost") || url.starts_with("http://[::1]");
        if !url.starts_with("https://") && !local {
            return Err(format!("JWKS URL must be https: {url}"));
        }
        Ok(Self { url: url.to_owned(), client, state: Mutex::new(JwksState::default()) })
    }

    async fn key(&self, kid: Option<&str>) -> Result<Jwk, AuthError> {
        let mut state = self.state.lock().await;
        let stale = state.fetched_at.is_none_or(|at| at.elapsed() >= JWKS_TTL);
        let find = |keys: &[Jwk]| keys.iter().find(|key| kid.is_none_or(|kid| key.kid.as_deref() == Some(kid))).cloned();
        if !stale && let Some(key) = find(&state.keys) {
            return Ok(key);
        }
        let may_refetch = stale || state.fetched_at.is_none_or(|at| at.elapsed() >= JWKS_MIN_REFETCH);
        if may_refetch {
            match self.fetch().await {
                Ok(keys) => {
                    state.keys = keys;
                    state.fetched_at = Some(Instant::now());
                }
                Err(error) if state.keys.is_empty() => return Err(error),
                Err(_) => {}
            }
        }
        find(&state.keys).ok_or(AuthError::BadToken("unknown key id"))
    }

    async fn fetch(&self) -> Result<Vec<Jwk>, AuthError> {
        let response = self.client.get(&self.url).timeout(Duration::from_secs(5)).send().await.map_err(|_| AuthError::KeysUnavailable)?;
        if !response.status().is_success() {
            return Err(AuthError::KeysUnavailable);
        }
        let bytes = response.bytes().await.map_err(|_| AuthError::KeysUnavailable)?;
        serde_json::from_slice::<JwkSet>(&bytes).map(|set| set.keys).map_err(|_| AuthError::KeysUnavailable)
    }
}

/// Credential settings of one deployment.
#[derive(Debug, Default)]
pub struct DeploymentAuth {
    api_key_hashes: Vec<String>,
    jwt: Option<(JwtConfig, Option<Vec<u8>>, Option<Arc<JwksCache>>)>,
}

impl DeploymentAuth {
    /// From configured API key digests and optional JWT settings.
    ///
    /// # Errors
    /// When the HS256 secret variable is unset or the JWKS URL is refused.
    pub fn new(api_key_hashes: Vec<String>, jwt: Option<JwtConfig>, client: &reqwest::Client) -> Result<Self, String> {
        let jwt = match jwt {
            None => None,
            Some(config) => {
                let secret = match &config.hs256_secret_env {
                    Some(name) => Some(std::env::var(name).map_err(|_| format!("JWT secret variable {name} is not set"))?.into_bytes()),
                    None => None,
                };
                let jwks = config.jwks_url.as_deref().map(|url| JwksCache::new(url, client.clone()).map(Arc::new)).transpose()?;
                if secret.is_none() && jwks.is_none() {
                    return Err("jwt needs jwks_url or hs256_secret_env".to_owned());
                }
                Some((config, secret, jwks))
            }
        };
        Ok(Self { api_key_hashes: api_key_hashes.into_iter().map(|hash| hash.to_ascii_lowercase()).collect(), jwt })
    }

    /// Whether any credential is configured (otherwise the deployment is open,
    /// which only `--dev` allows).
    pub fn is_configured(&self) -> bool {
        !self.api_key_hashes.is_empty() || self.jwt.is_some()
    }

    /// Authenticates `Authorization: Bearer <token>` or `X-Api-Key: <key>`:
    /// an API key first, then a JWT.
    ///
    /// # Errors
    /// See [`AuthError`].
    pub async fn authenticate(&self, bearer: Option<&str>, api_key: Option<&str>, now_secs: u64) -> Result<Principal, AuthError> {
        let presented = api_key.or(bearer).ok_or(AuthError::Missing)?;
        let digest = crate::hex::sha256_hex(presented.as_bytes());
        if self.api_key_hashes.iter().any(|hash| crate::hex::constant_time_eq(hash.as_bytes(), digest.as_bytes())) {
            return Ok(Principal::ApiKey);
        }
        match (&self.jwt, bearer) {
            (Some((config, secret, jwks)), Some(token)) if token.contains('.') => {
                verify_jwt(token, config, secret.as_deref(), jwks.as_deref(), now_secs).await
            }
            _ => Err(AuthError::BadKey),
        }
    }
}

fn decode_part(part: &str) -> Result<Vec<u8>, AuthError> {
    URL_SAFE_NO_PAD.decode(part).map_err(|_| AuthError::BadToken("bad base64"))
}

async fn verify_jwt(token: &str, config: &JwtConfig, secret: Option<&[u8]>, jwks: Option<&JwksCache>, now_secs: u64) -> Result<Principal, AuthError> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err(AuthError::BadToken("too long"));
    }
    let mut parts = token.split('.');
    let (Some(header_part), Some(claims_part), Some(signature_part), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err(AuthError::BadToken("not a JWS"));
    };
    let header: Header = serde_json::from_slice(&decode_part(header_part)?).map_err(|_| AuthError::BadToken("bad header"))?;
    let signature = decode_part(signature_part)?;
    let signing_input = token.get(..header_part.len() + 1 + claims_part.len()).ok_or(AuthError::BadToken("not a JWS"))?;
    match header.alg.as_str() {
        "HS256" => {
            let secret = secret.ok_or(AuthError::BadToken("HS256 not configured"))?;
            let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret);
            ring::hmac::verify(&key, signing_input.as_bytes(), &signature).map_err(|_| AuthError::BadToken("bad signature"))?;
        }
        "RS256" | "ES256" => {
            let jwks = jwks.ok_or(AuthError::BadToken("JWKS not configured"))?;
            let jwk = jwks.key(header.kid.as_deref()).await?;
            verify_asymmetric(&header.alg, &jwk, signing_input.as_bytes(), &signature)?;
        }
        _ => return Err(AuthError::BadToken("unsupported alg")),
    }
    let claims: Claims = serde_json::from_slice(&decode_part(claims_part)?).map_err(|_| AuthError::BadToken("bad claims"))?;
    check_claims(&claims, config, now_secs)
}

fn verify_asymmetric(alg: &str, jwk: &Jwk, message: &[u8], signature: &[u8]) -> Result<(), AuthError> {
    use ring::signature::{ECDSA_P256_SHA256_FIXED, RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents, UnparsedPublicKey};
    let bad = || AuthError::BadToken("bad signature");
    match (alg, jwk.kty.as_str()) {
        ("RS256", "RSA") => {
            let modulus = decode_part(jwk.n.as_deref().ok_or_else(bad)?)?;
            let exponent = decode_part(jwk.e.as_deref().ok_or_else(bad)?)?;
            RsaPublicKeyComponents { n: modulus, e: exponent }.verify(&RSA_PKCS1_2048_8192_SHA256, message, signature).map_err(|_| bad())
        }
        ("ES256", "EC") if jwk.crv.as_deref() == Some("P-256") => {
            let mut point = vec![0x04];
            point.extend(decode_part(jwk.x.as_deref().ok_or_else(bad)?)?);
            point.extend(decode_part(jwk.y.as_deref().ok_or_else(bad)?)?);
            UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, point).verify(message, signature).map_err(|_| bad())
        }
        _ => Err(AuthError::BadToken("key does not match alg")),
    }
}

fn check_claims(claims: &Claims, config: &JwtConfig, now_secs: u64) -> Result<Principal, AuthError> {
    let expires_at = claims.exp.ok_or(AuthError::BadToken("exp required"))?;
    if expires_at.saturating_add(LEEWAY_SECS) < now_secs {
        return Err(AuthError::BadToken("expired"));
    }
    if claims.nbf.is_some_and(|nbf| nbf > now_secs.saturating_add(LEEWAY_SECS)) {
        return Err(AuthError::BadToken("not yet valid"));
    }
    if claims.iss.as_deref() != Some(config.issuer.as_str()) {
        return Err(AuthError::BadToken("wrong issuer"));
    }
    let audience_ok = match &claims.aud {
        Some(Audience::One(audience)) => *audience == config.audience,
        Some(Audience::Many(audiences)) => audiences.iter().any(|audience| *audience == config.audience),
        None => false,
    };
    if !audience_ok {
        return Err(AuthError::BadToken("wrong audience"));
    }
    let subject = claims.sub.clone().filter(|subject| !subject.is_empty()).ok_or(AuthError::BadToken("sub required"))?;
    Ok(Principal::User { subject, expires_at })
}

/// Deployment → credentials.
pub type AuthTable = HashMap<String, DeploymentAuth>;

#[cfg(test)]
mod tests {
    use super::*;

    fn hs256_token(secret: &[u8], claims: &serde_json::Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
        let input = format!("{header}.{payload}");
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret);
        let signature = URL_SAFE_NO_PAD.encode(ring::hmac::sign(&key, input.as_bytes()).as_ref());
        format!("{input}.{signature}")
    }

    fn config() -> JwtConfig {
        JwtConfig { issuer: "https://auth.example".into(), audience: "shop".into(), jwks_url: None, hs256_secret_env: None }
    }

    fn auth_with_secret(secret: &[u8]) -> DeploymentAuth {
        DeploymentAuth { api_key_hashes: vec![crate::hex::sha256_hex(b"grk_live_1")], jwt: Some((config(), Some(secret.to_vec()), None)) }
    }

    #[tokio::test]
    async fn api_keys_by_digest() {
        let auth = auth_with_secret(b"s");
        assert_eq!(auth.authenticate(None, Some("grk_live_1"), 0).await, Ok(Principal::ApiKey));
        assert_eq!(auth.authenticate(Some("grk_live_1"), None, 0).await, Ok(Principal::ApiKey));
        assert_eq!(auth.authenticate(None, Some("grk_live_2"), 0).await, Err(AuthError::BadKey));
        assert_eq!(auth.authenticate(None, None, 0).await, Err(AuthError::Missing));
    }

    #[tokio::test]
    async fn hs256_claims_are_checked() {
        let auth = auth_with_secret(b"secret");
        let good = serde_json::json!({"iss": "https://auth.example", "aud": ["x", "shop"], "sub": "u1", "exp": 2000});
        let principal = auth.authenticate(Some(&hs256_token(b"secret", &good)), None, 1000).await.unwrap();
        assert_eq!(principal, Principal::User { subject: "u1".into(), expires_at: 2000 });
        let cases = [
            (serde_json::json!({"iss": "https://auth.example", "aud": "shop", "sub": "u1", "exp": 100}), "expired"),
            (serde_json::json!({"iss": "https://evil", "aud": "shop", "sub": "u1", "exp": 2000}), "wrong issuer"),
            (serde_json::json!({"iss": "https://auth.example", "aud": "other", "sub": "u1", "exp": 2000}), "wrong audience"),
            (serde_json::json!({"iss": "https://auth.example", "aud": "shop", "sub": "u1"}), "exp required"),
        ];
        for (claims, reason) in cases {
            assert_eq!(auth.authenticate(Some(&hs256_token(b"secret", &claims)), None, 1000).await, Err(AuthError::BadToken(reason)));
        }
        let forged = hs256_token(b"wrong", &good);
        assert_eq!(auth.authenticate(Some(&forged), None, 1000).await, Err(AuthError::BadToken("bad signature")));
    }

    #[test]
    fn jwks_url_must_be_https() {
        crate::install_crypto_provider();
        assert!(JwksCache::new("http://example.com/jwks", reqwest::Client::new()).is_err());
        assert!(JwksCache::new("https://example.com/jwks", reqwest::Client::new()).is_ok());
        assert!(JwksCache::new("http://127.0.0.1:9/jwks", reqwest::Client::new()).is_ok());
    }
}
