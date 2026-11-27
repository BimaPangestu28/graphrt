//! Compact JWS (JWT) parsing, signature verification and claim checks.
//!
//! Only the three algorithms identity providers such as Supabase Auth issue are
//! accepted: `HS256` (shared secret), `ES256` (P-256, the Supabase default for
//! asymmetric keys) and `RS256`. `none` and every other `alg` are refused, and a
//! key is only ever used with its own algorithm, so a token cannot downgrade an
//! asymmetric key to HMAC. All cryptography is `ring`, already linked for TLS.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::{hmac, signature};

/// Signature algorithm of a key or token header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Algorithm {
    Hs256,
    Es256,
    Rs256,
}

impl Algorithm {
    /// The algorithm named by a JOSE `alg` value; `None` for anything unsupported.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "HS256" => Some(Self::Hs256),
            "ES256" => Some(Self::Es256),
            "RS256" => Some(Self::Rs256),
            _ => None,
        }
    }
}

/// Public (or shared) key material of one verification key.
pub enum KeyMaterial {
    /// HMAC-SHA256 shared secret.
    Hmac(hmac::Key),
    /// Uncompressed P-256 point (`0x04 || x || y`).
    EcP256(Vec<u8>),
    /// RSA modulus and exponent, big-endian.
    Rsa { modulus: Vec<u8>, exponent: Vec<u8> },
}

impl std::fmt::Debug for KeyMaterial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Hmac(_) => "Hmac(******)",
            Self::EcP256(_) => "EcP256",
            Self::Rsa { .. } => "Rsa",
        })
    }
}

/// One key a token may be signed with.
#[derive(Debug)]
pub struct VerifyKey {
    /// JWK `kid`; `None` matches any token `kid` (a configured shared secret).
    pub key_id: Option<String>,
    pub algorithm: Algorithm,
    pub material: KeyMaterial,
}

impl VerifyKey {
    /// An `HS256` key from a shared secret (e.g. the Supabase legacy JWT secret).
    pub fn hs256(secret: &[u8]) -> Self {
        Self {
            key_id: None,
            algorithm: Algorithm::Hs256,
            material: KeyMaterial::Hmac(hmac::Key::new(hmac::HMAC_SHA256, secret)),
        }
    }

    fn verify(&self, message: &[u8], signature_bytes: &[u8]) -> bool {
        match &self.material {
            KeyMaterial::Hmac(key) => hmac::verify(key, message, signature_bytes).is_ok(),
            KeyMaterial::EcP256(point) => signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, point)
                .verify(message, signature_bytes)
                .is_ok(),
            KeyMaterial::Rsa { modulus, exponent } => signature::RsaPublicKeyComponents { n: modulus, e: exponent }
                .verify(&signature::RSA_PKCS1_2048_8192_SHA256, message, signature_bytes)
                .is_ok(),
        }
    }

    fn matches(&self, algorithm: Algorithm, key_id: Option<&str>) -> bool {
        self.algorithm == algorithm && (self.key_id.is_none() || key_id.is_none() || self.key_id.as_deref() == key_id)
    }
}

/// Why a token was refused. The messages are safe to return to the client.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    #[error("not a JWT")]
    Malformed,
    #[error("unsupported signing algorithm `{0}`")]
    UnsupportedAlgorithm(String),
    #[error("signed with an unknown key")]
    UnknownKey,
    #[error("invalid signature")]
    BadSignature,
    #[error("expired")]
    Expired,
    #[error("not valid yet")]
    NotYetValid,
    #[error("wrong issuer")]
    WrongIssuer,
    #[error("wrong audience")]
    WrongAudience,
    #[error("no `sub` claim")]
    MissingSubject,
}

/// Claims a token must satisfy besides its signature.
#[derive(Clone, Debug, Default)]
pub struct ClaimRules {
    /// Required `iss`, when set.
    pub issuer: Option<String>,
    /// Required `aud` (string, or one of an array), when set.
    pub audience: Option<String>,
    /// Clock skew allowed for `exp` and `nbf`, in seconds.
    pub leeway_secs: u64,
}

/// A token that passed every check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedToken {
    /// The `sub` claim: the end user's id.
    pub subject: String,
}

/// A parsed but not yet verified token.
#[derive(Debug)]
pub struct ParsedToken<'t> {
    pub algorithm: Algorithm,
    pub key_id: Option<String>,
    /// `header.payload`, the signed bytes.
    signing_input: &'t str,
    signature: Vec<u8>,
    claims: serde_json::Value,
}

/// Split and decode `token` without verifying it.
///
/// @throws TokenError - `Malformed` or `UnsupportedAlgorithm`.
pub fn parse(token: &str) -> Result<ParsedToken<'_>, TokenError> {
    let (signing_input, signature_b64) = token.rsplit_once('.').ok_or(TokenError::Malformed)?;
    let (header_b64, payload_b64) = signing_input.split_once('.').ok_or(TokenError::Malformed)?;
    let header = decode_json(header_b64)?;
    let alg = header.get("alg").and_then(serde_json::Value::as_str).ok_or(TokenError::Malformed)?;
    let algorithm =
        Algorithm::from_name(alg).ok_or_else(|| TokenError::UnsupportedAlgorithm(alg.chars().take(16).collect()))?;
    let key_id = header.get("kid").and_then(serde_json::Value::as_str).map(str::to_owned);
    let claims = decode_json(payload_b64)?;
    let signature = URL_SAFE_NO_PAD.decode(signature_b64).map_err(|_| TokenError::Malformed)?;
    Ok(ParsedToken { algorithm, key_id, signing_input, signature, claims })
}

impl ParsedToken<'_> {
    /// The claims, not verified yet.
    pub fn claims(&self) -> &serde_json::Value {
        &self.claims
    }

    /// Whether some key in `keys` could have signed this token (same `alg`, matching `kid`).
    pub fn has_candidate(&self, keys: &[VerifyKey]) -> bool {
        keys.iter().any(|k| k.matches(self.algorithm, self.key_id.as_deref()))
    }

    /// Check the signature against `keys`, then the claims against `rules` at `now_secs`.
    ///
    /// @throws TokenError - The first failed check.
    pub fn verify(&self, keys: &[VerifyKey], rules: &ClaimRules, now_secs: u64) -> Result<VerifiedToken, TokenError> {
        let mut candidates = keys.iter().filter(|k| k.matches(self.algorithm, self.key_id.as_deref())).peekable();
        if candidates.peek().is_none() {
            return Err(TokenError::UnknownKey);
        }
        if !candidates.any(|k| k.verify(self.signing_input.as_bytes(), &self.signature)) {
            return Err(TokenError::BadSignature);
        }
        check_claims(&self.claims, rules, now_secs)
    }
}

fn decode_json(segment: &str) -> Result<serde_json::Value, TokenError> {
    let bytes = URL_SAFE_NO_PAD.decode(segment).map_err(|_| TokenError::Malformed)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| TokenError::Malformed)?;
    if value.is_object() { Ok(value) } else { Err(TokenError::Malformed) }
}

fn check_claims(claims: &serde_json::Value, rules: &ClaimRules, now_secs: u64) -> Result<VerifiedToken, TokenError> {
    // `exp` is required: a token that never expires cannot be revoked by time.
    let expires = claims.get("exp").and_then(serde_json::Value::as_f64).ok_or(TokenError::Expired)?;
    let now = now_secs as f64;
    let leeway = rules.leeway_secs as f64;
    if now >= expires + leeway {
        return Err(TokenError::Expired);
    }
    if claims.get("nbf").and_then(serde_json::Value::as_f64).is_some_and(|nbf| now + leeway < nbf) {
        return Err(TokenError::NotYetValid);
    }
    if let Some(issuer) = &rules.issuer
        && claims.get("iss").and_then(serde_json::Value::as_str) != Some(issuer.as_str())
    {
        return Err(TokenError::WrongIssuer);
    }
    if let Some(audience) = &rules.audience
        && !audience_matches(claims.get("aud"), audience)
    {
        return Err(TokenError::WrongAudience);
    }
    let subject = claims.get("sub").and_then(serde_json::Value::as_str).filter(|s| !s.is_empty());
    let subject = subject.ok_or(TokenError::MissingSubject)?;
    Ok(VerifiedToken { subject: subject.to_owned() })
}

fn audience_matches(aud: Option<&serde_json::Value>, expected: &str) -> bool {
    match aud {
        Some(serde_json::Value::String(aud)) => aud == expected,
        Some(serde_json::Value::Array(auds)) => auds.iter().any(|a| a.as_str() == Some(expected)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
    use serde_json::json;

    const NOW: u64 = 1_800_000_000;

    fn encode(value: &serde_json::Value) -> String {
        URL_SAFE_NO_PAD.encode(value.to_string())
    }

    fn hs256_token(secret: &[u8], header: &serde_json::Value, claims: &serde_json::Value) -> String {
        let input = format!("{}.{}", encode(header), encode(claims));
        let tag = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, secret), input.as_bytes());
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(tag.as_ref()))
    }

    fn user_claims() -> serde_json::Value {
        json!({ "sub": "user-1", "exp": NOW + 3600, "iss": "https://p.supabase.co/auth/v1", "aud": "authenticated", "role": "authenticated" })
    }

    fn supabase_rules() -> ClaimRules {
        ClaimRules {
            issuer: Some("https://p.supabase.co/auth/v1".into()),
            audience: Some("authenticated".into()),
            leeway_secs: 30,
        }
    }

    fn verify_hs256(secret: &[u8], token: &str) -> Result<VerifiedToken, TokenError> {
        parse(token)?.verify(&[VerifyKey::hs256(secret)], &supabase_rules(), NOW)
    }

    #[test]
    fn accepts_a_valid_hs256_token() {
        let token = hs256_token(b"secret", &json!({"alg": "HS256", "typ": "JWT"}), &user_claims());
        assert_eq!(verify_hs256(b"secret", &token), Ok(VerifiedToken { subject: "user-1".into() }));
    }

    #[test]
    fn refuses_a_wrong_secret_or_tampered_payload() {
        let token = hs256_token(b"secret", &json!({"alg": "HS256"}), &user_claims());
        assert_eq!(verify_hs256(b"other", &token), Err(TokenError::BadSignature));
        let (_, rest) = token.split_once('.').unwrap();
        let (_, signature) = rest.split_once('.').unwrap();
        let forged_claims = json!({ "sub": "admin", "exp": NOW + 3600, "iss": "https://p.supabase.co/auth/v1", "aud": "authenticated" });
        let forged = format!("{}.{}.{signature}", encode(&json!({"alg": "HS256"})), encode(&forged_claims));
        assert_eq!(verify_hs256(b"secret", &forged), Err(TokenError::BadSignature));
    }

    #[test]
    fn refuses_alg_none_and_unknown_algorithms() {
        let none = format!("{}.{}.", encode(&json!({"alg": "none"})), encode(&user_claims()));
        assert_eq!(verify_hs256(b"secret", &none), Err(TokenError::UnsupportedAlgorithm("none".into())));
        let hs512 = hs256_token(b"secret", &json!({"alg": "HS512"}), &user_claims());
        assert_eq!(verify_hs256(b"secret", &hs512), Err(TokenError::UnsupportedAlgorithm("HS512".into())));
    }

    #[test]
    fn checks_expiry_issuer_audience_and_subject() {
        let header = json!({"alg": "HS256"});
        let mut claims = user_claims();
        claims["exp"] = json!(NOW - 31);
        assert_eq!(verify_hs256(b"s", &hs256_token(b"s", &header, &claims)), Err(TokenError::Expired));
        claims["exp"] = json!(NOW - 10); // within the leeway
        assert!(verify_hs256(b"s", &hs256_token(b"s", &header, &claims)).is_ok());
        let mut claims = user_claims();
        claims.as_object_mut().unwrap().remove("exp");
        assert_eq!(verify_hs256(b"s", &hs256_token(b"s", &header, &claims)), Err(TokenError::Expired));
        let mut claims = user_claims();
        claims["nbf"] = json!(NOW + 60);
        assert_eq!(verify_hs256(b"s", &hs256_token(b"s", &header, &claims)), Err(TokenError::NotYetValid));
        let mut claims = user_claims();
        claims["iss"] = json!("https://evil.example");
        assert_eq!(verify_hs256(b"s", &hs256_token(b"s", &header, &claims)), Err(TokenError::WrongIssuer));
        let mut claims = user_claims();
        claims["aud"] = json!(["other", "authenticated"]);
        assert!(verify_hs256(b"s", &hs256_token(b"s", &header, &claims)).is_ok());
        claims["aud"] = json!("anon");
        assert_eq!(verify_hs256(b"s", &hs256_token(b"s", &header, &claims)), Err(TokenError::WrongAudience));
        // The legacy anon / service_role keys are JWTs without `sub`.
        let mut claims = user_claims();
        claims.as_object_mut().unwrap().remove("sub");
        assert_eq!(verify_hs256(b"s", &hs256_token(b"s", &header, &claims)), Err(TokenError::MissingSubject));
    }

    #[test]
    fn refuses_malformed_tokens() {
        for token in ["", "abc", "a.b", "a.b.c", "e30.e30.!!", &format!("{}.W10.", encode(&json!({"alg": "HS256"})))] {
            assert!(verify_hs256(b"s", token).is_err(), "{token}");
        }
    }

    fn es256_pair() -> (EcdsaKeyPair, VerifyKey) {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
        let key = VerifyKey {
            key_id: Some("k1".into()),
            algorithm: Algorithm::Es256,
            material: KeyMaterial::EcP256(pair.public_key().as_ref().to_vec()),
        };
        (pair, key)
    }

    fn es256_token(pair: &EcdsaKeyPair, kid: &str, claims: &serde_json::Value) -> String {
        let input = format!("{}.{}", encode(&json!({"alg": "ES256", "kid": kid})), encode(claims));
        let sig = pair.sign(&SystemRandom::new(), input.as_bytes()).unwrap();
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig.as_ref()))
    }

    #[test]
    fn accepts_es256_with_the_matching_kid_only() {
        let (pair, key) = es256_pair();
        let keys = [key];
        let token = es256_token(&pair, "k1", &user_claims());
        assert_eq!(parse(&token).unwrap().verify(&keys, &supabase_rules(), NOW).unwrap().subject, "user-1");
        let other_kid = es256_token(&pair, "k2", &user_claims());
        let parsed = parse(&other_kid).unwrap();
        assert!(!parsed.has_candidate(&keys));
        assert_eq!(parsed.verify(&keys, &supabase_rules(), NOW), Err(TokenError::UnknownKey));
    }

    #[test]
    fn an_asymmetric_key_is_never_used_as_an_hmac_secret() {
        // Classic confusion attack: sign HS256 with the public key bytes.
        let (_, key) = es256_pair();
        let KeyMaterial::EcP256(point) = &key.material else { unreachable!() };
        let token = hs256_token(point, &json!({"alg": "HS256", "kid": "k1"}), &user_claims());
        assert_eq!(parse(&token).unwrap().verify(&[key], &supabase_rules(), NOW), Err(TokenError::UnknownKey));
    }

    #[test]
    fn verifies_the_rfc7515_rs256_example() {
        // RFC 7515 appendix A.2: signature check only (its `exp` is in 2011).
        let jwk = json!({
            "kty": "RSA",
            "n": "ofgWCuLjybRlzo0tZWJjNiuSfb4p4fAkd_wWJcyQoTbji9k0l8W26mPddxHmfHQp-Vaw-4qPCJrcS2mJPMEzP1Pt0Bm4d4QlL-yRT-SFd2lZS-pCgNMsD1W_YpRPEwOWvG6b32690r2jZ47soMZo9wGzjb_7OMg0LOL-bSf63kpaSHSXndS5z5rexMdbBYUsLA9e-KXBdQOS-UTo7WTBEMa2R2CapHg665xsmtdVMTBQY4uDZlxvb3qCo5ZwKh9kG4LT6_I5IhlJH7aGhyxXFvUK-DWNmoudF8NAco9_h9iaGNj8q2ethFkMLs91kzk2PAcDTW9gb54h4FRWyuXpoQ",
            "e": "AQAB"
        });
        let key = crate::user_auth::jwks::parse_jwk(&jwk).unwrap();
        let token = "eyJhbGciOiJSUzI1NiJ9.eyJpc3MiOiJqb2UiLA0KICJleHAiOjEzMDA4MTkzODAsDQogImh0dHA6Ly9leGFtcGxlLmNvbS9pc19yb290Ijp0cnVlfQ.cC4hiUPoj9Eetdgtv3hF80EGrhuB__dzERat0XF9g2VtQgr9PJbu3XOiZj5RZmh7AAuHIm4Bh-0Qc_lF5YKt_O8W2Fp5jujGbds9uJdbF9CUAr7t1dnZcAcQjbKBYNX4BAynRFdiuB--f_nZLgrnbyTyWzO75vRK5h6xBArLIARNPvkSjtQBMHlb1L07Qe7K0GarZRmB_eSN9383LcOLn6_dO--xi12jzDwusC-eOkHWEsqtFZESc6BfI7noOPqvhJ1phCnvWh6IeYI2w9QOYEUipUTI8np6LbgGY9Fs98rqVt5AXLIhWkWywlVmtVrBp0igcN_IoypGlUPQGe77Rw";
        let parsed = parse(token).unwrap();
        assert_eq!(parsed.algorithm, Algorithm::Rs256);
        let rules = ClaimRules::default();
        // The signature verifies; the claims then fail on `exp`.
        assert_eq!(parsed.verify(std::slice::from_ref(&key), &rules, NOW), Err(TokenError::Expired));
        assert_eq!(parsed.verify(&[key], &rules, 1_300_819_000), Err(TokenError::MissingSubject));
    }
}
