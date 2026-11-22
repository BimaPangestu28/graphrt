//! End-user JWT verification and per-user rate limits for `graphrt serve`
//! (`--verify-jwt`, cargo feature `jwt`).
//!
//! A mobile or web chat app signs its users in with an identity provider
//! (Supabase Auth, Auth0, Firebase...) and calls `graphrt serve` directly with
//! the user's access token in a header mapped by `--input-header`. The app must
//! not hold the serve API key, so with `--verify-jwt INPUT`:
//!
//! - `POST /v1/workflows/{name}/runs` accepts a request whose mapped header holds a
//!   token that verifies (signature by a configured key, `exp`/`nbf`, optional
//!   `iss`/`aud`, a non-empty `sub`) *instead of* the API key. A present but
//!   invalid token is `401`, even next to a valid API key, so a bad token never
//!   reaches a tool. Without the header the API key rules apply unchanged.
//!   Only workflows whose start node declares `INPUT` accept a user token
//!   (others answer `403`), so internal workflows stay key-only.
//! - The verified `sub` becomes `sys.user_id` of the run and the `user_id` of its
//!   record and spans (the body cannot set either).
//! - `--user-rate-limit N` allows each `sub` N runs per minute (bursts of N);
//!   more answer `429` with `Retry-After`.
//! - `POST /v1/runs/{id}/feedback` (feature `scores`) accepts the token too, for
//!   runs started with a token of the same `sub` only (`user_verified` on the
//!   record; a body `user_id` from an API-key caller does not count).
//! - A user-token request sees only the answer stream, the final result and
//!   pause prompts of its run ([`events`]); `--user-events all` shows every
//!   engine event, as API-key callers get.
//! - Every other endpoint (run lists, events, scores, resume, knowledge...) still
//!   needs the API key, so users cannot read each other's runs. The server
//!   refuses to start with `--verify-jwt` unless an API key is set
//!   ([`require_api_key`]).
//!
//! Keys come from a JWKS URL (`ES256`/`RS256`, fetched at startup and kept fresh)
//! and/or an `HS256` shared secret named by `--jwt-secret-env`. Verification is
//! in memory: no network round trip per request. See `docs/serving.md`.

pub(crate) mod events;
pub mod jwks;
pub mod rate_limit;
pub mod token;

use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::args::{UserAuthArgs, UserEvents};
use crate::error::CliError;
use crate::header_inputs::{HeaderInputSpec, strip_bearer};
use crate::serve::error_response;
use jwks::{MIN_REFETCH_INTERVAL, RemoteKeySet};
use rate_limit::UserRateLimiter;
use token::{ClaimRules, TokenError, VerifiedToken, VerifyKey};

/// Fallbacks of `--jwt-jwks-url`, `--jwt-issuer` and `--jwt-audience`.
pub const JWKS_URL_ENV: &str = "GRAPHRT_JWT_JWKS_URL";
pub const ISSUER_ENV: &str = "GRAPHRT_JWT_ISSUER";
pub const AUDIENCE_ENV: &str = "GRAPHRT_JWT_AUDIENCE";

/// Clock skew tolerated for `exp` and `nbf`.
const LEEWAY_SECS: u64 = 30;

/// The result of checking a start request's user token.
#[derive(Debug, PartialEq, Eq)]
pub enum Admission {
    /// A valid token within its user's rate limit.
    User(VerifiedToken),
    /// No token in the header: the API key rules decide.
    NoToken,
}

/// Configured verification of the `--verify-jwt` input's header.
#[derive(Debug)]
pub struct UserAuth {
    /// The `--verify-jwt` input.
    input: String,
    header: HeaderName,
    /// Keys given locally (the `HS256` secret).
    local_keys: Vec<VerifyKey>,
    remote_keys: Option<Arc<RemoteKeySet>>,
    rules: ClaimRules,
    limiter: Option<UserRateLimiter>,
    /// What a user-token request sees of its run (`--user-events`).
    events: UserEvents,
}

impl UserAuth {
    /// Build from the serve flags; `None` without `--verify-jwt`.
    ///
    /// @param lookup_env - Reads an environment variable (process env, then `--env-file`).
    /// @throws CliError (usage) - On an unmapped input, no key source, an unset secret
    ///   variable or a JWKS URL that cannot be fetched.
    pub async fn from_args(
        args: &UserAuthArgs,
        header_specs: &[HeaderInputSpec],
        lookup_env: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, CliError> {
        let Some(input) = &args.verify_jwt else { return Ok(None) };
        let header = header_specs.iter().find(|s| &s.input == input).map(|s| s.header.clone()).ok_or_else(|| {
            CliError::usage(format!(
                "--verify-jwt {input}: map the input to a header with --input-header {input}=HEADER"
            ))
        })?;
        let mut local_keys = Vec::new();
        if let Some(var) = &args.jwt_secret_env {
            let secret = lookup_env(var)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| CliError::usage(format!("--jwt-secret-env {var}: the variable is not set")))?;
            local_keys.push(VerifyKey::hs256(secret.as_bytes()));
        }
        // Each flag falls back to an environment variable, for images without a shell.
        let flag_or_env =
            |flag: &Option<String>, var: &str| flag.clone().or_else(|| lookup_env(var)).filter(|v| !v.is_empty());
        let remote_keys = match flag_or_env(&args.jwt_jwks_url, JWKS_URL_ENV) {
            Some(url) => {
                let set =
                    RemoteKeySet::fetch(&url).await.map_err(|e| CliError::usage(format!("--jwt-jwks-url: {e}")))?;
                set.spawn_refresh();
                Some(set)
            }
            None => None,
        };
        if local_keys.is_empty() && remote_keys.is_none() {
            return Err(CliError::usage("--verify-jwt needs --jwt-jwks-url and/or --jwt-secret-env"));
        }
        let rules = ClaimRules {
            issuer: flag_or_env(&args.jwt_issuer, ISSUER_ENV),
            audience: flag_or_env(&args.jwt_audience, AUDIENCE_ENV),
            leeway_secs: LEEWAY_SECS,
        };
        let limiter = (args.user_rate_limit > 0).then(|| UserRateLimiter::per_minute(args.user_rate_limit));
        Ok(Some(Self {
            input: input.clone(),
            header,
            local_keys,
            remote_keys,
            rules,
            limiter,
            events: args.user_events,
        }))
    }

    /// Verify `spec`'s header with fixed keys only (tests and embedders).
    pub fn with_keys(spec: &HeaderInputSpec, keys: Vec<VerifyKey>, rules: ClaimRules, per_minute: u32) -> Self {
        let limiter = (per_minute > 0).then(|| UserRateLimiter::per_minute(per_minute));
        Self {
            input: spec.input.clone(),
            header: spec.header.clone(),
            local_keys: keys,
            remote_keys: None,
            rules,
            limiter,
            events: UserEvents::Answer,
        }
    }

    /// Show user-token requests `events` of their runs (default: [`UserEvents::Answer`]).
    pub fn with_events(mut self, events: UserEvents) -> Self {
        self.events = events;
        self
    }

    /// What a user-token request sees of its run.
    pub fn events(&self) -> UserEvents {
        self.events
    }

    /// The start input whose header is verified.
    pub fn input(&self) -> &str {
        &self.input
    }

    /// The header that carries the user token.
    pub fn header(&self) -> &HeaderName {
        &self.header
    }

    /// One line describing the configuration for the startup log.
    pub fn describe(&self) -> String {
        let mut sources = Vec::new();
        if !self.local_keys.is_empty() {
            sources.push("HS256 secret");
        }
        if self.remote_keys.is_some() {
            sources.push("JWKS");
        }
        let limit = self.limiter.as_ref().map_or("", |_| ", per-user rate limit on");
        let events = match self.events {
            UserEvents::Answer => "answer and result only",
            UserEvents::All => "all events",
        };
        format!("user JWTs in `{}` verified ({}){limit}; users see {events}", self.header, sources.join(" + "))
    }

    /// Check the token in `headers`, then its user's rate limit.
    ///
    /// @throws Response (401) - The header holds a token that does not verify.
    /// @throws Response (429) - The user is over `--user-rate-limit`.
    pub async fn admit(&self, headers: &HeaderMap) -> Result<Admission, Response> {
        let token = headers.get(&self.header).and_then(|v| v.to_str().ok()).map(strip_bearer).unwrap_or_default();
        if token.is_empty() {
            return Ok(Admission::NoToken);
        }
        let verified = self.verify(token).await.map_err(|e| unauthorized(&self.header, &e))?;
        if let Some(limiter) = &self.limiter
            && let Err(wait) = limiter.check(&verified.subject, Instant::now())
        {
            return Err(too_many_requests(wait.as_secs() + 1));
        }
        Ok(Admission::User(verified))
    }

    async fn verify(&self, token: &str) -> Result<VerifiedToken, TokenError> {
        let parsed = token::parse(token)?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default();
        if parsed.has_candidate(&self.local_keys) {
            return parsed.verify(&self.local_keys, &self.rules, now);
        }
        let Some(remote) = &self.remote_keys else { return Err(TokenError::UnknownKey) };
        let mut keys = remote.keys();
        // An unknown `kid` usually means the provider rotated its keys.
        if !parsed.has_candidate(&keys) && remote.refresh(MIN_REFETCH_INTERVAL).await {
            keys = remote.keys();
        }
        parsed.verify(&keys, &self.rules, now)
    }
}

/// Refuse `--verify-jwt` without an API key, on every address.
///
/// User tokens open only run starts and feedback; without a key every other
/// endpoint (run lists, events, resume, knowledge) is public. Binding loopback
/// is no protection: a reverse proxy on the same host forwards the internet to
/// a loopback port, and one that rewrites `Host` also passes the DNS-rebinding
/// check of keyless servers.
///
/// @param verify_jwt - Whether `--verify-jwt` is set.
/// @param has_api_key - Whether the API key variable holds a key.
/// @param api_key_env - Name of that variable, for the message.
/// @throws CliError (usage) - When `--verify-jwt` is set without a key.
pub fn require_api_key(verify_jwt: bool, has_api_key: bool, api_key_env: &str) -> Result<(), CliError> {
    if !verify_jwt || has_api_key {
        return Ok(());
    }
    Err(CliError::usage(format!(
        "--verify-jwt needs ${api_key_env} too: user tokens only authorize POST /v1/workflows/{{name}}/runs \
         and feedback, and without an API key every other endpoint (run lists, events, resume) would be \
         public, even on a loopback address behind a reverse proxy"
    )))
}

fn unauthorized(header: &HeaderName, error: &TokenError) -> Response {
    let mut response = error_response(StatusCode::UNAUTHORIZED, &format!("invalid user token in `{header}`: {error}"));
    response.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer error=\"invalid_token\""));
    response
}

fn too_many_requests(retry_after_secs: u64) -> Response {
    let mut response = error_response(StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded for this user; retry later");
    if let Ok(value) = HeaderValue::from_str(&retry_after_secs.to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response.into_response()
}
