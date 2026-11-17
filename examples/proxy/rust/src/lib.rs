//! A thin LLM proxy and token broker for apps that run agents on user devices.
//!
//! ```text
//!  app ──(1) POST /token, Authorization: Bearer <user session>──▶ broker ──▶ {token, expires_in, base_url}
//!  app ──(2) POST /v1/chat/completions, Authorization: Bearer <token>──▶ proxy ──(+ provider key)──▶ provider
//! ```
//!
//! - Tokens are HS256 JWTs signed with `PROXY_JWT_SECRET`, valid for `TOKEN_TTL_SECS`.
//! - The proxy checks the token, the path and the model allow-list, then
//!   forwards the request with the provider key and streams the response back.
//! - The provider key never leaves the server.
//!
//! The `/token` endpoint here authenticates users with a single demo token
//! (`DEMO_USER_TOKEN`). Replace `authenticate_user` with your real session check.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// How the upstream provider expects its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpstreamAuth {
    /// `Authorization: Bearer <key>` (OpenAI-compatible providers).
    Bearer,
    /// `x-api-key: <key>` (Anthropic).
    XApiKey,
}

/// Proxy settings.
#[derive(Clone)]
pub struct Config {
    pub jwt_secret: Vec<u8>,
    pub upstream_base_url: String,
    pub upstream_key: String,
    pub upstream_auth: UpstreamAuth,
    /// Allowed `model` values; empty allows any model.
    pub allowed_models: Vec<String>,
    /// Allowed request paths (relative to `/v1`).
    pub allowed_paths: Vec<String>,
    pub token_ttl: Duration,
    /// Demo user credential accepted by `/token`.
    pub demo_user_token: Option<String>,
    /// Base URL handed to apps in token responses (this proxy's public URL + `/v1`).
    pub public_base_url: Option<String>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("upstream_base_url", &self.upstream_base_url)
            .field("allowed_models", &self.allowed_models)
            .finish_non_exhaustive()
    }
}

impl Config {
    /// Read settings from environment variables.
    pub fn from_env() -> Result<Self, String> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let jwt_secret = var("PROXY_JWT_SECRET").ok_or("PROXY_JWT_SECRET is required (32+ random bytes)")?;
        if jwt_secret.len() < 32 {
            return Err("PROXY_JWT_SECRET must be at least 32 characters".into());
        }
        let upstream_key = var("UPSTREAM_API_KEY").ok_or("UPSTREAM_API_KEY is required")?;
        let upstream_auth = match var("UPSTREAM_AUTH").as_deref() {
            None | Some("bearer") => UpstreamAuth::Bearer,
            Some("x-api-key") => UpstreamAuth::XApiKey,
            Some(other) => return Err(format!("UPSTREAM_AUTH must be `bearer` or `x-api-key`, got `{other}`")),
        };
        let list = |k: &str, default: &str| -> Vec<String> {
            var(k)
                .unwrap_or_else(|| default.to_owned())
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        };
        let ttl: u64 = var("TOKEN_TTL_SECS").and_then(|v| v.parse().ok()).unwrap_or(900);
        Ok(Self {
            jwt_secret: jwt_secret.into_bytes(),
            upstream_base_url: var("UPSTREAM_BASE_URL").unwrap_or_else(|| "https://api.openai.com/v1".into()),
            upstream_key,
            upstream_auth,
            allowed_models: list("ALLOWED_MODELS", ""),
            allowed_paths: list("ALLOWED_PATHS", "/chat/completions,/messages,/embeddings"),
            token_ttl: Duration::from_secs(ttl),
            demo_user_token: var("DEMO_USER_TOKEN"),
            public_base_url: var("PUBLIC_BASE_URL"),
        })
    }
}

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    client: reqwest::Client,
}

/// Build the router: `POST /token` and `POST /v1/{*path}`.
pub fn router(config: Config) -> Result<Router, String> {
    let _already = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let state = AppState { config: Arc::new(config), client };
    Ok(Router::new().route("/token", post(issue_token)).route("/v1/{*path}", post(forward)).with_state(state))
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Sign an HS256 JWT with `sub` and `exp`.
pub fn sign_token(secret: &[u8], subject: &str, expires_at: u64) -> Result<String, String> {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = serde_json::json!({"sub": subject, "exp": expires_at});
    let payload = URL_SAFE_NO_PAD.encode(payload.to_string());
    let signing_input = format!("{header}.{payload}");
    let mut mac = HmacSha256::new_from_slice(secret).map_err(|e| e.to_string())?;
    mac.update(signing_input.as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    Ok(format!("{signing_input}.{signature}"))
}

/// Verify an HS256 JWT and return its subject. Rejects other algorithms,
/// bad signatures and expired tokens.
pub fn verify_token(secret: &[u8], token: &str, now: u64) -> Result<String, &'static str> {
    let mut parts = token.split('.');
    let (Some(h), Some(p), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err("malformed token");
    };
    let header: serde_json::Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(h).map_err(|_| "malformed token")?)
        .map_err(|_| "malformed token")?;
    if header.get("alg").and_then(|a| a.as_str()) != Some("HS256") {
        return Err("unsupported token algorithm");
    }
    let signature = URL_SAFE_NO_PAD.decode(s).map_err(|_| "malformed token")?;
    let mut mac = HmacSha256::new_from_slice(secret).map_err(|_| "server misconfigured")?;
    mac.update(format!("{h}.{p}").as_bytes());
    // Constant-time comparison.
    mac.verify_slice(&signature).map_err(|_| "invalid token signature")?;
    let payload: serde_json::Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(p).map_err(|_| "malformed token")?)
        .map_err(|_| "malformed token")?;
    let exp = payload.get("exp").and_then(serde_json::Value::as_u64).ok_or("token has no expiry")?;
    if exp <= now {
        return Err("token expired");
    }
    Ok(payload.get("sub").and_then(|s| s.as_str()).unwrap_or_default().to_owned())
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ")
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(serde_json::json!({"error": {"message": message}}))).into_response()
}

/// Replace with your real session check (e.g. verify your auth provider's ID token).
fn authenticate_user(config: &Config, headers: &HeaderMap) -> Option<String> {
    let presented = bearer(headers)?;
    let expected = config.demo_user_token.as_deref()?;
    // Compare via HMAC to keep the comparison constant-time.
    let tag = |v: &str| {
        let mut mac = HmacSha256::new_from_slice(&config.jwt_secret).ok()?;
        mac.update(v.as_bytes());
        Some(mac.finalize().into_bytes())
    };
    (tag(presented)? == tag(expected)?).then(|| "demo-user".to_owned())
}

async fn issue_token(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(user) = authenticate_user(&state.config, &headers) else {
        return error(StatusCode::UNAUTHORIZED, "sign in required");
    };
    let ttl = state.config.token_ttl.as_secs();
    match sign_token(&state.config.jwt_secret, &user, now_secs() + ttl) {
        Ok(token) => {
            let mut body = serde_json::json!({"token": token, "expires_in": ttl});
            if let Some(url) = &state.config.public_base_url {
                body["base_url"] = serde_json::Value::String(url.clone());
            }
            axum::Json(body).into_response()
        }
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "cannot sign token"),
    }
}

async fn forward(
    State(state): State<AppState>,
    axum::extract::Path(path): axum::extract::Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let config = &state.config;
    let Some(token) = bearer(&headers) else {
        return error(StatusCode::UNAUTHORIZED, "missing token");
    };
    let user = match verify_token(&config.jwt_secret, token, now_secs()) {
        Ok(user) => user,
        Err(reason) => return error(StatusCode::UNAUTHORIZED, reason),
    };
    let path = format!("/{path}");
    if !config.allowed_paths.iter().any(|p| p == &path) {
        return error(StatusCode::NOT_FOUND, "path not allowed");
    }
    let json: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return error(StatusCode::BAD_REQUEST, "body must be JSON"),
    };
    let model = json.get("model").and_then(|m| m.as_str()).unwrap_or_default();
    if !config.allowed_models.is_empty() && !config.allowed_models.iter().any(|m| m == model) {
        return error(StatusCode::BAD_REQUEST, "model not allowed");
    }
    // Per-user rate limiting / quotas belong here (keyed by `user`).
    let _ = user;

    let url = format!("{}{path}", config.upstream_base_url.trim_end_matches('/'));
    let mut request = state.client.post(url).header(header::CONTENT_TYPE, "application/json").body(body);
    request = match config.upstream_auth {
        UpstreamAuth::Bearer => request.bearer_auth(&config.upstream_key),
        UpstreamAuth::XApiKey => request.header("x-api-key", &config.upstream_key),
    };
    // Forward only provider headers the app may set; never its Authorization.
    for name in ["anthropic-version", "anthropic-beta", "accept"] {
        if let Some(v) = headers.get(name) {
            request = request.header(name, v);
        }
    }
    let upstream = match request.send().await {
        Ok(r) => r,
        Err(e) => return error(StatusCode::BAD_GATEWAY, &format!("upstream unreachable: {}", e.without_url())),
    };
    let status = StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    // A rejected provider key is the proxy's problem, not the app's token: report 502.
    let status = if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        StatusCode::BAD_GATEWAY
    } else {
        status
    };
    let content_type = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_owned();
    let stream = upstream.bytes_stream();
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    if let Ok(value) = content_type.parse() {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

    #[test]
    fn tokens_roundtrip_and_expire() {
        let t = sign_token(SECRET, "u1", 2_000).unwrap();
        assert_eq!(verify_token(SECRET, &t, 1_000).unwrap(), "u1");
        assert_eq!(verify_token(SECRET, &t, 2_000), Err("token expired"));
        assert_eq!(verify_token(b"another-secret-another-secret-xx", &t, 1_000), Err("invalid token signature"));
        let mut tampered = t.clone();
        tampered.push('x');
        assert!(verify_token(SECRET, &tampered, 1_000).is_err());
        let none_alg = format!("{}.{}.", URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#), URL_SAFE_NO_PAD.encode(b"{}"));
        assert_eq!(verify_token(SECRET, &none_alg, 1_000), Err("unsupported token algorithm"));
    }
}
