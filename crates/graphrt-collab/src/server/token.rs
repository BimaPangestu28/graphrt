//! Access tokens: short lived, signed by the server, scoped to an account and
//! optionally to one workspace.
//!
//! `gca1.<payload>.<signature>`: the payload is base64url JSON
//! ([`AccessClaims`]), the signature HMAC-SHA256 over `gca1.<payload>` with
//! the server's signing key. They are checked without a lookup, so they stay
//! short (10 minutes by default). What a person may do is never read from
//! the token: every request looks up the current membership, so removing
//! someone or changing their role applies at once.

use std::path::Path;

use ring::hmac;
use serde::{Deserialize, Serialize};

use super::error::{ServerError, ServerResult};
use super::util::{b64url, b64url_decode, random_bytes};

const PREFIX: &str = "gca1.";
/// Clock skew allowed when checking expiry, in seconds.
const LEEWAY_SECS: u64 = 30;

/// What an access token says.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessClaims {
    /// The account.
    pub sub: String,
    /// The workspace it is scoped to, `None` for account wide requests
    /// (listing and creating workspaces). Sync sessions need a scoped token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ws: Option<String>,
    /// Issued at, Unix seconds.
    pub iat: u64,
    /// Expires at, Unix seconds.
    pub exp: u64,
}

/// The key that signs access tokens.
pub struct SigningKey {
    key: hmac::Key,
}

impl std::fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SigningKey(..)")
    }
}

impl SigningKey {
    /// A key from 32 or more bytes.
    pub fn from_bytes(bytes: &[u8]) -> ServerResult<Self> {
        if bytes.len() < 32 {
            return Err(ServerError::Config("the signing key needs at least 32 bytes".into()));
        }
        Ok(Self { key: hmac::Key::new(hmac::HMAC_SHA256, bytes) })
    }

    /// A key from text: 32 bytes as hex or base64.
    ///
    /// @throws ServerError::Config - When the text is not 32 bytes in one of those forms.
    pub fn parse(text: &str) -> ServerResult<Self> {
        let text = text.trim();
        let bytes = decode_hex(text)
            .or_else(|| {
                use base64::Engine as _;
                base64::engine::general_purpose::STANDARD.decode(text).ok()
            })
            .or_else(|| b64url_decode(text.trim_end_matches('=')))
            .ok_or_else(|| ServerError::Config("the signing key is not hex or base64".into()))?;
        Self::from_bytes(&bytes)
    }

    /// The key in `dir/signing.key`, made (owner only) when it is missing.
    ///
    /// @throws ServerError - When the file cannot be read or written.
    pub fn load_or_create(dir: &Path) -> ServerResult<Self> {
        let path = dir.join("signing.key");
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let bytes = random_bytes::<32>()?;
                super::util::replace_file(&path, super::util::hex(&bytes).as_bytes())?;
                Self::from_bytes(&bytes)
            }
            Err(error) => Err(ServerError::Io(format!("cannot read {}: {error}", path.display()))),
        }
    }

    /// Sign `claims`.
    pub fn sign(&self, claims: &AccessClaims) -> ServerResult<String> {
        let payload = serde_json::to_vec(claims).map_err(|error| ServerError::Internal(error.to_string()))?;
        let signed = format!("{PREFIX}{}", b64url(&payload));
        let tag = hmac::sign(&self.key, signed.as_bytes());
        Ok(format!("{signed}.{}", b64url(tag.as_ref())))
    }

    /// The claims of `token` when it is signed by this key and not expired at `now_secs`.
    ///
    /// @throws ServerError::Unauthorized - When it is malformed, signed by another key or expired.
    pub fn verify(&self, token: &str, now_secs: u64) -> ServerResult<AccessClaims> {
        let invalid = || ServerError::Unauthorized("the access token is not valid".into());
        let (signed, signature) = token.rsplit_once('.').ok_or_else(invalid)?;
        let payload = signed.strip_prefix(PREFIX).ok_or_else(invalid)?;
        let signature = b64url_decode(signature).ok_or_else(invalid)?;
        hmac::verify(&self.key, signed.as_bytes(), &signature).map_err(|_| invalid())?;
        let claims: AccessClaims =
            serde_json::from_slice(&b64url_decode(payload).ok_or_else(invalid)?).map_err(|_| invalid())?;
        if claims.exp + LEEWAY_SECS < now_secs {
            return Err(ServerError::Unauthorized("the access token expired".into()));
        }
        Ok(claims)
    }
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(exp: u64) -> AccessClaims {
        AccessClaims { sub: "acct_1".into(), ws: Some("ws_1".into()), iat: 0, exp }
    }

    #[test]
    fn signs_and_verifies() {
        let key = SigningKey::from_bytes(&[7; 32]).unwrap();
        let token = key.sign(&claims(1_000)).unwrap();
        assert!(token.starts_with("gca1."));
        assert_eq!(key.verify(&token, 900).unwrap(), claims(1_000));
    }

    #[test]
    fn refuses_expired_tampered_and_foreign_tokens() {
        let key = SigningKey::from_bytes(&[7; 32]).unwrap();
        let other = SigningKey::from_bytes(&[8; 32]).unwrap();
        let token = key.sign(&claims(1_000)).unwrap();
        assert!(key.verify(&token, 2_000).is_err());
        assert!(other.verify(&token, 900).is_err());
        let (signed, signature) = token.rsplit_once('.').unwrap();
        let forged_payload = b64url(br#"{"sub":"acct_admin","iat":0,"exp":9999999999}"#);
        assert!(key.verify(&format!("gca1.{forged_payload}.{signature}"), 900).is_err());
        assert!(key.verify(&format!("{signed}."), 900).is_err());
        assert!(key.verify("garbage", 900).is_err());
    }

    #[test]
    fn parses_keys() {
        assert!(SigningKey::parse(&"ab".repeat(32)).is_ok());
        assert!(SigningKey::parse("c2hvcnQ=").is_err());
        assert!(SigningKey::from_bytes(&[1; 16]).is_err());
        let dir = std::env::temp_dir().join(format!("graphrt-collab-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = SigningKey::load_or_create(&dir).unwrap();
        let second = SigningKey::load_or_create(&dir).unwrap();
        let token = first.sign(&claims(10)).unwrap();
        assert!(second.verify(&token, 5).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
