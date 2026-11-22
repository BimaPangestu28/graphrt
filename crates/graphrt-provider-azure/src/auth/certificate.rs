//! Certificate credential: a PS256 client assertion (RFC 7523) signed with the
//! certificate's RSA key, identified by the `x5t#S256` thumbprint, as the
//! Microsoft identity platform requires for certificate credentials.

use std::fmt;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{RSA_PSS_SHA256, RsaKeyPair};

use super::AuthError;

/// Lifetime of one client assertion; Entra accepts at most 10 minutes.
const ASSERTION_LIFETIME_SECS: u64 = 300;

/// An RSA private key and the thumbprint of its certificate.
#[derive(Clone)]
pub struct CertificateKey {
    key_pair: Arc<RsaKeyPair>,
    /// base64url(SHA-256(certificate DER)).
    thumbprint_sha256: String,
}

impl fmt::Debug for CertificateKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CertificateKey")
            .field("x5t#S256", &self.thumbprint_sha256)
            .field("key", &"[redacted]")
            .finish()
    }
}

impl CertificateKey {
    /// Parse a PEM bundle holding one `CERTIFICATE` and one RSA key
    /// (`PRIVATE KEY` = PKCS#8, or `RSA PRIVATE KEY` = PKCS#1), the format of
    /// `AZURE_CLIENT_CERTIFICATE_PATH`. Encrypted keys and PKCS#12 are not supported.
    ///
    /// @throws `AuthError::Config` when either block is missing or invalid.
    pub fn from_pem(pem: &str) -> Result<Self, AuthError> {
        let certificate = pem_block(pem, "CERTIFICATE")
            .ok_or_else(|| AuthError::Config("certificate PEM has no CERTIFICATE block".into()))?;
        let key_pair = if let Some(pkcs8) = pem_block(pem, "PRIVATE KEY") {
            RsaKeyPair::from_pkcs8(&pkcs8)
        } else if let Some(pkcs1) = pem_block(pem, "RSA PRIVATE KEY") {
            RsaKeyPair::from_der(&pkcs1)
        } else {
            return Err(AuthError::Config(
                "certificate PEM has no unencrypted RSA key (PRIVATE KEY or RSA PRIVATE KEY)".into(),
            ));
        }
        .map_err(|error| AuthError::Config(format!("certificate key is not a usable RSA key: {error}")))?;
        let digest = ring::digest::digest(&ring::digest::SHA256, &certificate);
        Ok(Self { key_pair: Arc::new(key_pair), thumbprint_sha256: URL_SAFE_NO_PAD.encode(digest.as_ref()) })
    }

    /// base64url SHA-256 thumbprint of the certificate (`x5t#S256`).
    pub fn thumbprint_sha256(&self) -> &str {
        &self.thumbprint_sha256
    }

    /// The RSA public key (PKCS#1 DER), for tests and diagnostics.
    pub fn public_key_der(&self) -> &[u8] {
        self.key_pair.public().as_ref()
    }

    /// A signed client assertion for `audience` (the token endpoint URL).
    ///
    /// @throws `AuthError::Config` when signing fails.
    pub fn assertion(&self, client_id: &str, audience: &str, now_unix: u64) -> Result<String, AuthError> {
        let random = SystemRandom::new();
        let header = serde_json::json!({ "alg": "PS256", "typ": "JWT", "x5t#S256": self.thumbprint_sha256 });
        let claims = serde_json::json!({
            "aud": audience,
            "iss": client_id,
            "sub": client_id,
            "jti": random_uuid(&random)?,
            "nbf": now_unix,
            "iat": now_unix,
            "exp": now_unix + ASSERTION_LIFETIME_SECS,
        });
        let signing_input =
            format!("{}.{}", URL_SAFE_NO_PAD.encode(header.to_string()), URL_SAFE_NO_PAD.encode(claims.to_string()));
        let mut signature = vec![0u8; self.key_pair.public().modulus_len()];
        self.key_pair
            .sign(&RSA_PSS_SHA256, &random, signing_input.as_bytes(), &mut signature)
            .map_err(|_| AuthError::Config("cannot sign client assertion".into()))?;
        Ok(format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature)))
    }
}

/// A random RFC 4122 version-4 UUID for `jti`.
fn random_uuid(random: &SystemRandom) -> Result<String, AuthError> {
    let mut bytes = [0u8; 16];
    random.fill(&mut bytes).map_err(|_| AuthError::Config("system random source unavailable".into()))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut uuid = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            uuid.push('-');
        }
        uuid.push_str(&format!("{byte:02x}"));
    }
    Ok(uuid)
}

/// The DER bytes of the first `-----BEGIN <label>-----` block.
fn pem_block(pem: &str, label: &str) -> Option<Vec<u8>> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = pem.find(&begin)? + begin.len();
    let length = pem.get(start..)?.find(&end)?;
    let body: String = pem.get(start..start + length)?.chars().filter(|c| !c.is_whitespace()).collect();
    STANDARD.decode(body).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIENT_PEM: &str = include_str!("../../tests/fixtures/client.pem");
    const CERT_PEM: &str = include_str!("../../tests/fixtures/cert.pem");
    const PKCS1_PEM: &str = include_str!("../../tests/fixtures/key-pkcs1.pem");

    fn decode_part(part: &str) -> serde_json::Value {
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).unwrap_or_default()).unwrap_or_default()
    }

    #[test]
    fn assertion_has_entra_header_claims_and_valid_ps256_signature() {
        let key = CertificateKey::from_pem(CLIENT_PEM).unwrap_or_else(|error| unreachable!("{error}"));
        let audience = "https://login.microsoftonline.com/tenant/oauth2/v2.0/token";
        let jwt = key.assertion("client-id", audience, 1_700_000_000).unwrap_or_else(|error| unreachable!("{error}"));
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header = decode_part(parts[0]);
        assert_eq!(header["alg"], "PS256");
        assert_eq!(header["x5t#S256"], key.thumbprint_sha256());
        let claims = decode_part(parts[1]);
        assert_eq!(claims["aud"], audience);
        assert_eq!(claims["iss"], "client-id");
        assert_eq!(claims["sub"], "client-id");
        assert_eq!(claims["exp"].as_u64(), Some(1_700_000_000 + ASSERTION_LIFETIME_SECS));
        assert_eq!(claims["jti"].as_str().map(str::len), Some(36));

        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap_or_default();
        let public_key =
            ring::signature::UnparsedPublicKey::new(&ring::signature::RSA_PSS_2048_8192_SHA256, key.public_key_der());
        let signing_input = format!("{}.{}", parts[0], parts[1]);
        assert!(public_key.verify(signing_input.as_bytes(), &signature).is_ok());
    }

    #[test]
    fn pkcs1_keys_are_accepted() {
        let bundle = format!("{CERT_PEM}{PKCS1_PEM}");
        assert!(CertificateKey::from_pem(&bundle).is_ok());
    }

    #[test]
    fn missing_key_is_a_config_error_and_debug_hides_the_key() {
        assert!(matches!(CertificateKey::from_pem(CERT_PEM), Err(AuthError::Config(_))));
        let key = CertificateKey::from_pem(CLIENT_PEM).unwrap_or_else(|error| unreachable!("{error}"));
        assert!(format!("{key:?}").contains("[redacted]"));
    }
}
