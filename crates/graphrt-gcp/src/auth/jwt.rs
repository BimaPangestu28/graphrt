//! Compact JWS helpers: RS256 signing of service-account assertions (RFC 7515 /
//! RFC 7523) and reading the lifetime of ID tokens Google issued.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use graphrt_provider_api::ProviderError;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde_json::Value;

/// Lifetime assumed for an ID token whose `exp`/`iat` cannot be read.
const DEFAULT_ID_TOKEN_LIFETIME_SECS: u64 = 3600;

/// Signs `claims` as an RS256 JWT with header `{alg, typ, kid?}`.
///
/// @param key - RSA private key of the service account
/// @param key_id - `private_key_id` of the key file, sent as `kid`
/// @param claims - JWT claims object
/// @returns The compact `header.payload.signature` string
/// @throws `ProviderError` (fatal) when signing fails
pub(crate) fn sign_rs256(key: &RsaKeyPair, key_id: Option<&str>, claims: &Value) -> Result<String, ProviderError> {
    let mut header = serde_json::json!({ "alg": "RS256", "typ": "JWT" });
    if let (Some(key_id), Some(object)) = (key_id, header.as_object_mut()) {
        object.insert("kid".to_owned(), Value::String(key_id.to_owned()));
    }
    let signing_input = format!("{}.{}", encode_segment(&header), encode_segment(claims));
    let mut signature = vec![0u8; key.public().modulus_len()];
    key.sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), signing_input.as_bytes(), &mut signature)
        .map_err(|_| ProviderError::fatal("cannot sign the service-account assertion"))?;
    Ok(format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature)))
}

fn encode_segment(value: &Value) -> String {
    URL_SAFE_NO_PAD.encode(value.to_string())
}

/// Reads the claims of a JWT **without verifying it**. Only used on ID tokens
/// that came straight from Google (metadata server or token endpoint over TLS),
/// to learn how long they live.
pub(crate) fn unverified_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Lifetime of a Google ID token, `exp - iat`, so a skewed local clock does not
/// matter (the cache counts it from the local fetch time).
pub(crate) fn id_token_lifetime_secs(token: &str) -> u64 {
    let Some(claims) = unverified_claims(token) else { return DEFAULT_ID_TOKEN_LIFETIME_SECS };
    let expires_at = claims.get("exp").and_then(Value::as_u64);
    let issued_at = claims.get("iat").and_then(Value::as_u64);
    match (expires_at, issued_at) {
        (Some(expires_at), Some(issued_at)) if expires_at > issued_at => expires_at - issued_at,
        _ => DEFAULT_ID_TOKEN_LIFETIME_SECS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token_with(claims: &Value) -> String {
        format!("{}.{}.sig", encode_segment(&serde_json::json!({"alg": "RS256"})), encode_segment(claims))
    }

    #[test]
    fn lifetime_is_exp_minus_iat() {
        let token = token_with(&serde_json::json!({"iat": 1_000, "exp": 4_600}));
        assert_eq!(id_token_lifetime_secs(&token), 3_600);
    }

    #[test]
    fn unreadable_token_uses_default_lifetime() {
        assert_eq!(id_token_lifetime_secs("not-a-jwt"), DEFAULT_ID_TOKEN_LIFETIME_SECS);
        let inverted = token_with(&serde_json::json!({"iat": 10, "exp": 5}));
        assert_eq!(id_token_lifetime_secs(&inverted), DEFAULT_ID_TOKEN_LIFETIME_SECS);
    }
}
