//! Fly API tokens and the `Authorization` header built from them.
//!
//! Fly hands out two shapes of token: macaroons (`fly tokens create deploy`,
//! printed as `FlyV1 fm2_...`) and legacy personal/OAuth tokens. Macaroons go
//! out with the `FlyV1` scheme, the others with `Bearer`, which is what flyctl
//! does (`fly-go/tokens.Tokens::FlapsHeader`).
//!
//! The value is never printed: `Debug` is redacted, and nothing in this crate
//! logs headers or request bodies that carry it.

use std::fmt;

/// Environment variable flyctl and the Fly runtime use for the API token.
pub const FLY_API_TOKEN_ENV: &str = "FLY_API_TOKEN";

/// A Fly API token: run-time a deploy token scoped to one app (start, suspend,
/// stop, list), deploy-time a token that may also create apps and set secrets.
#[derive(Clone)]
pub struct FlyToken {
    authorization_value: String,
}

impl FlyToken {
    /// Wraps a token as printed by flyctl, with or without its scheme.
    ///
    /// @param raw_token - `FlyV1 fm2_...`, `fm2_...`, `Bearer xyz` or a bare personal token (&str)
    /// @returns The token, or `None` when it is empty after trimming
    ///
    /// @example
    /// let token = FlyToken::parse("FlyV1 fm2_abc").unwrap();
    /// assert_eq!(token.authorization_header(), "FlyV1 fm2_abc");
    pub fn parse(raw_token: &str) -> Option<Self> {
        let trimmed_token = raw_token.trim();
        if trimmed_token.is_empty() {
            return None;
        }
        let authorization_value = if trimmed_token.starts_with("FlyV1 ") || trimmed_token.starts_with("Bearer ") {
            trimmed_token.to_owned()
        } else if is_macaroon(trimmed_token) {
            format!("FlyV1 {trimmed_token}")
        } else {
            format!("Bearer {trimmed_token}")
        };
        Some(Self { authorization_value })
    }

    /// Reads [`FLY_API_TOKEN_ENV`].
    ///
    /// @returns The token, or `None` when the variable is unset or empty
    pub fn from_env() -> Option<Self> {
        std::env::var(FLY_API_TOKEN_ENV).ok().as_deref().and_then(Self::parse)
    }

    /// The full `Authorization` header value, scheme included.
    pub fn authorization_header(&self) -> &str {
        &self.authorization_value
    }
}

/// Where the token comes from, so a rotated token is picked up after a 401
/// without restarting the gateway.
#[derive(Clone, Debug)]
pub enum TokenOrigin {
    /// A fixed token (tests, or a token handed over by `graphrt deploy`).
    Fixed(FlyToken),
    /// An environment variable, read again on reload.
    Env(String),
    /// A file (mounted secret), read again on reload.
    File(std::path::PathBuf),
}

impl TokenOrigin {
    /// Loads the token from its origin.
    ///
    /// @returns The token, or a message naming the origin (never the value)
    pub fn load(&self) -> Result<FlyToken, String> {
        match self {
            Self::Fixed(token) => Ok(token.clone()),
            Self::Env(variable) => std::env::var(variable)
                .ok()
                .as_deref()
                .and_then(FlyToken::parse)
                .ok_or_else(|| format!("Fly API token: environment variable {variable} is unset or empty")),
            Self::File(path) => std::fs::read_to_string(path)
                .map_err(|error| format!("Fly API token: reading {}: {error}", path.display()))
                .and_then(|text| {
                    FlyToken::parse(&text).ok_or_else(|| format!("Fly API token: {} is empty", path.display()))
                }),
        }
    }
}

/// Macaroon tokens are `fm1r_`/`fm1a_`/`fm2_` prefixed, possibly a comma
/// separated list (a token and its discharges).
fn is_macaroon(token: &str) -> bool {
    token.starts_with("fm1") || token.starts_with("fm2")
}

impl fmt::Debug for FlyToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("FlyToken(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macaroon_without_scheme_gets_flyv1() {
        let token = FlyToken::parse("fm2_lJPECAAAAAAAA,fm2_second").unwrap();
        assert_eq!(token.authorization_header(), "FlyV1 fm2_lJPECAAAAAAAA,fm2_second");
    }

    #[test]
    fn printed_deploy_token_is_used_as_is() {
        let token = FlyToken::parse("  FlyV1 fm2_abc\n").unwrap();
        assert_eq!(token.authorization_header(), "FlyV1 fm2_abc");
    }

    #[test]
    fn personal_token_gets_bearer() {
        assert_eq!(FlyToken::parse("abc123").unwrap().authorization_header(), "Bearer abc123");
        assert_eq!(FlyToken::parse("Bearer abc123").unwrap().authorization_header(), "Bearer abc123");
    }

    #[test]
    fn empty_token_is_rejected() {
        assert!(FlyToken::parse("   ").is_none());
    }

    #[test]
    fn missing_env_origin_names_the_variable_only() {
        let message = TokenOrigin::Env("GRAPHRT_TEST_FLY_TOKEN_UNSET".into()).load().unwrap_err();
        assert!(message.contains("GRAPHRT_TEST_FLY_TOKEN_UNSET"));
    }

    #[test]
    fn debug_never_shows_the_token() {
        let token = FlyToken::parse("fm2_secret_value").unwrap();
        assert!(!format!("{token:?}").contains("secret_value"));
    }
}
