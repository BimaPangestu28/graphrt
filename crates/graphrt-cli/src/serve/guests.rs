//! Guest sessions of the chat page (`--chat-access guests`).
//!
//! A public chat page has no sign in, but the server still needs to tell its
//! visitors apart: to keep one visitor's runs and conversations from another,
//! and to limit what each one may spend. So each browser gets a guest token:
//!
//! ```text
//! grg1.<id>.<expires>.<signature>
//! ```
//!
//! `id` is 128 random bits (hex), `expires` Unix seconds, and `signature`
//! HMAC-SHA256 over the part before it (base64url). The server keeps no state
//! per guest: the signature proves it issued the token. A guest is the user
//! `guest:<id>`: their runs are their own like any verified user's.
//!
//! The signing key is derived from the API key, so tokens survive restarts and
//! work on every replica that shares the key. Without an API key (a loopback
//! server) it is random per process.
//!
//! A token lives for [`TOKEN_LIFETIME_SECS`]. An expired one can be renewed for
//! [`RENEWAL_WINDOW_SECS`] (`POST /v1/chat/guest` with it), which keeps the id,
//! so a returning visitor still finds their conversations.
//!
//! A session costs nothing to open, so the limit per guest cannot bound the
//! spend of a public page on its own: `--chat-guest-budget` caps the runs of
//! all guests together.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};

use super::errors::error_response;
use crate::error::CliError;
use crate::user_auth::rate_limit::UserRateLimiter;

/// Prefix of guest tokens; the digit is the format version.
const TOKEN_PREFIX: &str = "grg1.";
/// Prefix of a guest's user id.
pub(crate) const SUBJECT_PREFIX: &str = "guest:";
/// How long a guest token is accepted.
pub const TOKEN_LIFETIME_SECS: u64 = 12 * 60 * 60;
/// How long after it expired a token can still be renewed, keeping its guest.
pub const RENEWAL_WINDOW_SECS: u64 = 30 * 24 * 60 * 60;
/// Separates the key derivation of guest tokens from any other use of the API key.
const KEY_CONTEXT: &[u8] = b"graphrt chat guest sessions v1";
/// The one bucket of the budget shared by all guests.
const ALL_GUESTS: &str = "";

/// Whether `token` claims to be a guest token (it may still be invalid).
pub(crate) fn is_guest_token(token: &str) -> bool {
    token.starts_with(TOKEN_PREFIX)
}

/// Whether `subject` is a guest's user id.
pub(crate) fn is_guest_subject(subject: &str) -> bool {
    subject.starts_with(SUBJECT_PREFIX)
}

/// Why a guest token was not accepted.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum GuestTokenError {
    #[error("malformed guest token")]
    Malformed,
    #[error("the guest token's signature does not verify")]
    BadSignature,
    #[error("the guest session expired")]
    Expired,
}

/// A newly issued or renewed guest token.
#[derive(Debug, PartialEq, Eq)]
pub struct IssuedToken {
    pub token: String,
    pub expires_ms: u64,
}

/// Issues and checks guest tokens, and limits what guests may start.
#[derive(Debug)]
pub struct Guests {
    key: hmac::Key,
    random: SystemRandom,
    /// Runs per minute of one guest (`--chat-guest-rate-limit`).
    per_guest: Option<UserRateLimiter>,
    /// Runs per minute of all guests together (`--chat-guest-budget`).
    budget: Option<UserRateLimiter>,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs()).unwrap_or_default()
}

impl Guests {
    /// Guest sessions signed with a key derived from `api_key` (random without one).
    ///
    /// @param per_guest - Runs per minute of one guest (0 = unlimited).
    /// @param budget - Runs per minute of all guests together (0 = unlimited).
    /// @throws CliError - When the system has no randomness for a key.
    pub fn new(api_key: Option<&str>, per_guest: u32, budget: u32) -> Result<Self, CliError> {
        let random = SystemRandom::new();
        let secret = match api_key {
            // HMAC as a key derivation: the API key itself never signs a token.
            Some(api_key) => {
                hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, api_key.as_bytes()), KEY_CONTEXT).as_ref().to_vec()
            }
            None => {
                let mut secret = vec![0_u8; 32];
                random
                    .fill(&mut secret)
                    .map_err(|_| CliError::failed("cannot create guest sessions: no system randomness"))?;
                secret
            }
        };
        Ok(Self {
            key: hmac::Key::new(hmac::HMAC_SHA256, &secret),
            random,
            per_guest: (per_guest > 0).then(|| UserRateLimiter::per_minute(per_guest)),
            budget: (budget > 0).then(|| UserRateLimiter::per_minute(budget)),
        })
    }

    /// A token for a new guest.
    ///
    /// @throws GuestTokenError - Never `Expired`; `Malformed` when the system has no randomness.
    pub fn issue(&self) -> Result<IssuedToken, GuestTokenError> {
        let mut id = [0_u8; 16];
        self.random.fill(&mut id).map_err(|_| GuestTokenError::Malformed)?;
        Ok(self.sign(&crate::hex::to_hex(&id), now_secs()))
    }

    /// A fresh token for the guest of `token`, which may have expired up to
    /// [`RENEWAL_WINDOW_SECS`] ago.
    ///
    /// @throws GuestTokenError - For a token this server did not issue, or one expired for too long.
    pub fn renew(&self, token: &str) -> Result<IssuedToken, GuestTokenError> {
        self.renew_at(token, now_secs())
    }

    fn renew_at(&self, token: &str, now: u64) -> Result<IssuedToken, GuestTokenError> {
        let (id, expires) = self.parse(token)?;
        if now > expires.saturating_add(RENEWAL_WINDOW_SECS) {
            return Err(GuestTokenError::Expired);
        }
        Ok(self.sign(id, now))
    }

    fn sign(&self, id: &str, now: u64) -> IssuedToken {
        let expires = now.saturating_add(TOKEN_LIFETIME_SECS);
        let payload = format!("{TOKEN_PREFIX}{id}.{expires}");
        let signature = URL_SAFE_NO_PAD.encode(hmac::sign(&self.key, payload.as_bytes()));
        IssuedToken { token: format!("{payload}.{signature}"), expires_ms: expires.saturating_mul(1000) }
    }

    /// The guest id and expiry of a token whose signature verifies.
    fn parse<'a>(&self, token: &'a str) -> Result<(&'a str, u64), GuestTokenError> {
        let (payload, signature) = token.rsplit_once('.').ok_or(GuestTokenError::Malformed)?;
        let body = payload.strip_prefix(TOKEN_PREFIX).ok_or(GuestTokenError::Malformed)?;
        let (id, expires) = body.split_once('.').ok_or(GuestTokenError::Malformed)?;
        let well_formed = id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit());
        let expires: u64 = expires.parse().map_err(|_| GuestTokenError::Malformed)?;
        let signature = URL_SAFE_NO_PAD.decode(signature).map_err(|_| GuestTokenError::Malformed)?;
        if !well_formed {
            return Err(GuestTokenError::Malformed);
        }
        // Constant time, by `ring`.
        hmac::verify(&self.key, payload.as_bytes(), &signature).map_err(|_| GuestTokenError::BadSignature)?;
        Ok((id, expires))
    }

    /// The user id (`guest:<id>`) of a valid, unexpired token.
    ///
    /// @throws GuestTokenError - For a token this server did not issue, or an expired one.
    pub fn verify(&self, token: &str) -> Result<String, GuestTokenError> {
        self.verify_at(token, now_secs())
    }

    fn verify_at(&self, token: &str, now: u64) -> Result<String, GuestTokenError> {
        let (id, expires) = self.parse(token)?;
        if now > expires {
            return Err(GuestTokenError::Expired);
        }
        Ok(format!("{SUBJECT_PREFIX}{id}"))
    }

    /// Admit a request with a guest token: verify it and, when `metered` (the
    /// request starts or continues a run), count it against the guest's limit
    /// and the budget of all guests.
    ///
    /// @returns The guest's user id.
    /// @throws Response - `401` for an invalid token, `429` over a limit.
    #[allow(clippy::result_large_err)]
    pub(crate) fn admit(&self, token: &str, metered: bool) -> Result<String, Response> {
        let subject = self.verify(token).map_err(|e| unauthorized(&e))?;
        if metered {
            let now = Instant::now();
            // The guest's own limit first: a guest over it does not use up the shared budget.
            if let Some(limiter) = &self.per_guest
                && let Err(wait) = limiter.check(&subject, now)
            {
                return Err(too_many_requests("rate limit exceeded for this guest; retry later", wait));
            }
            if let Some(budget) = &self.budget
                && let Err(wait) = budget.check(ALL_GUESTS, now)
            {
                return Err(too_many_requests("the chat is busy; retry later", wait));
            }
        }
        Ok(subject)
    }
}

fn unauthorized(error: &GuestTokenError) -> Response {
    let mut response = error_response(StatusCode::UNAUTHORIZED, &error.to_string());
    response.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer error=\"invalid_token\""));
    response
}

fn too_many_requests(message: &str, wait: Duration) -> Response {
    let mut response = error_response(StatusCode::TOO_MANY_REQUESTS, message);
    if let Ok(value) = HeaderValue::from_str(&(wait.as_secs() + 1).to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn guests() -> Guests {
        Guests::new(Some("server-key"), 0, 0).unwrap()
    }

    #[test]
    fn an_issued_token_verifies_as_its_guest() {
        let guests = guests();
        let issued = guests.issue().unwrap();
        assert!(is_guest_token(&issued.token));
        let subject = guests.verify(&issued.token).unwrap();
        assert!(is_guest_subject(&subject));
        assert_eq!(subject.len(), SUBJECT_PREFIX.len() + 32);
        assert_ne!(guests.issue().unwrap().token, issued.token, "every guest is new");
    }

    #[test]
    fn tokens_survive_a_restart_with_the_same_api_key_only() {
        let token = guests().issue().unwrap().token;
        assert!(guests().verify(&token).is_ok(), "same key, another process");
        let other = Guests::new(Some("another-key"), 0, 0).unwrap();
        assert_eq!(other.verify(&token), Err(GuestTokenError::BadSignature));
        let keyless = Guests::new(None, 0, 0).unwrap();
        assert_eq!(keyless.verify(&token), Err(GuestTokenError::BadSignature));
    }

    #[test]
    fn a_changed_token_is_refused() {
        let guests = guests();
        let token = guests.issue().unwrap().token;
        let parts: Vec<&str> = token.split('.').collect();
        // Another guest id, a later expiry, a cut signature: none verify.
        let other_id = format!("{}.{}.{}.{}", parts[0], "0".repeat(32), parts[2], parts[3]);
        assert_eq!(guests.verify(&other_id), Err(GuestTokenError::BadSignature));
        let later = format!("{}.{}.{}.{}", parts[0], parts[1], u64::MAX, parts[3]);
        assert_eq!(guests.verify(&later), Err(GuestTokenError::BadSignature));
        assert!(guests.verify(&token[..token.len() - 2]).is_err());
        for malformed in ["grg1.", "grg1.abc.1.sig", "grg1.zz.1", "grg2.x.1.y", ""] {
            assert!(guests.verify(malformed).is_err(), "{malformed}");
        }
    }

    #[test]
    fn an_expired_token_is_refused_but_renews_as_the_same_guest() {
        let guests = guests();
        let issued = guests.sign(&"ab".repeat(16), 1_000);
        let expired_at = 1_000 + TOKEN_LIFETIME_SECS + 1;
        assert_eq!(guests.verify_at(&issued.token, expired_at), Err(GuestTokenError::Expired));
        let renewed = guests.renew_at(&issued.token, expired_at).unwrap();
        assert_eq!(guests.verify_at(&renewed.token, expired_at).unwrap(), format!("guest:{}", "ab".repeat(16)));
        let too_late = expired_at + RENEWAL_WINDOW_SECS;
        assert_eq!(guests.renew_at(&issued.token, too_late), Err(GuestTokenError::Expired));
    }

    #[test]
    fn runs_are_limited_per_guest_and_for_all_guests_together() {
        let guests = Guests::new(Some("k"), 2, 3).unwrap();
        let (first, second) = (guests.issue().unwrap().token, guests.issue().unwrap().token);
        assert!(guests.admit(&first, true).is_ok());
        assert!(guests.admit(&first, true).is_ok());
        let refused = guests.admit(&first, true).unwrap_err();
        assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(refused.headers().contains_key(header::RETRY_AFTER));
        // Reading is never limited, and the refused request used none of the budget.
        assert!(guests.admit(&first, false).is_ok());
        assert!(guests.admit(&second, true).is_ok());
        let over_budget = guests.admit(&second, true).unwrap_err();
        assert_eq!(over_budget.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(guests.admit("grg1.nope", true).unwrap_err().status(), StatusCode::UNAUTHORIZED);
    }
}
