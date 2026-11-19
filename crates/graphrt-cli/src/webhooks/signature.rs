//! HMAC-SHA256 webhook signatures: verifying inbound deliveries (GitHub, Stripe,
//! Slack and generic styles) and signing outbound completion callbacks.
//!
//! HMAC (RFC 2104) is built on the `sha2` crate already in the dependency tree
//! instead of pulling in another crate; it is checked against RFC 4231 vectors.

use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::hex::{from_hex, to_hex};
use crate::serve::constant_time_eq;

/// SHA-256 block size in bytes.
const BLOCK_BYTES: usize = 64;

/// Default allowed clock skew for timestamped schemes (Stripe, Slack, graphrt).
pub const DEFAULT_TOLERANCE_SECS: u64 = 300;

/// Header carrying graphrt's own signature (outbound callbacks, `scheme: graphrt`).
pub const GRAPHRT_SIGNATURE_HEADER: &str = "x-graphrt-signature";

/// HMAC-SHA256 of the concatenation of `message_parts` under `key`.
///
/// @param key - Secret key; keys longer than a block are hashed first (RFC 2104).
/// @param message_parts - Message pieces, MAC'd as if concatenated.
/// @returns The 32-byte MAC.
pub fn hmac_sha256(key: &[u8], message_parts: &[&[u8]]) -> [u8; 32] {
    let hashed_key;
    let key = if key.len() > BLOCK_BYTES {
        hashed_key = Sha256::digest(key);
        hashed_key.as_slice()
    } else {
        key
    };
    let mut inner_pad = [0x36u8; BLOCK_BYTES];
    let mut outer_pad = [0x5cu8; BLOCK_BYTES];
    for ((inner, outer), key_byte) in inner_pad.iter_mut().zip(outer_pad.iter_mut()).zip(key) {
        *inner ^= key_byte;
        *outer ^= key_byte;
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    for part in message_parts {
        inner.update(part);
    }
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner_digest);
    outer.finalize().into()
}

/// Signature conventions of common webhook senders.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SignatureScheme {
    /// Hex HMAC of the raw body in one header (`header`, `prefix` configurable).
    #[default]
    HmacSha256,
    /// GitHub: `X-Hub-Signature-256: sha256=<hex hmac(body)>`.
    Github,
    /// Stripe: `Stripe-Signature: t=<unix>,v1=<hex hmac("<t>.<body>")>`.
    Stripe,
    /// Slack: `X-Slack-Signature: v0=<hex hmac("v0:<ts>:<body>")>` with
    /// `X-Slack-Request-Timestamp: <ts>`.
    Slack,
    /// graphrt's callback format: Stripe's, in `X-Graphrt-Signature`.
    Graphrt,
}

/// Why a delivery's signature was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignatureError {
    /// The named header is absent.
    Missing(String),
    /// The named header cannot be parsed.
    Malformed(String),
    /// The signed timestamp is outside the replay window.
    Expired,
    /// The MAC is wrong.
    Mismatch,
}

impl std::fmt::Display for SignatureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(header) => write!(f, "missing signature header `{header}`"),
            Self::Malformed(header) => write!(f, "malformed signature header `{header}`"),
            Self::Expired => f.write_str("signature timestamp is outside the allowed window"),
            Self::Mismatch => f.write_str("signature does not match"),
        }
    }
}

impl std::error::Error for SignatureError {}

/// Verifies deliveries of one webhook.
#[derive(Clone)]
pub struct Verifier {
    scheme: SignatureScheme,
    secret: Vec<u8>,
    header: String,
    prefix: String,
    tolerance_secs: u64,
}

impl std::fmt::Debug for Verifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Verifier")
            .field("scheme", &self.scheme)
            .field("header", &self.header)
            .field("secret", &"***")
            .finish_non_exhaustive()
    }
}

impl Verifier {
    /// A verifier for `scheme`. `header` and `prefix` override the scheme's defaults
    /// (they matter for `hmac-sha256`; the other schemes fix their own format).
    pub fn new(scheme: SignatureScheme, secret: Vec<u8>, header: Option<String>, prefix: Option<String>) -> Self {
        let (default_header, default_prefix) = match scheme {
            SignatureScheme::HmacSha256 => ("x-signature-256", "sha256="),
            SignatureScheme::Github => ("x-hub-signature-256", "sha256="),
            SignatureScheme::Stripe => ("stripe-signature", ""),
            SignatureScheme::Slack => ("x-slack-signature", "v0="),
            SignatureScheme::Graphrt => (GRAPHRT_SIGNATURE_HEADER, ""),
        };
        Self {
            scheme,
            secret,
            header: header.unwrap_or_else(|| default_header.to_owned()),
            prefix: prefix.unwrap_or_else(|| default_prefix.to_owned()),
            tolerance_secs: DEFAULT_TOLERANCE_SECS,
        }
    }

    /// Accept timestamps at most this many seconds away from now (timestamped schemes).
    pub fn with_tolerance(mut self, tolerance_secs: u64) -> Self {
        self.tolerance_secs = tolerance_secs;
        self
    }

    pub fn scheme(&self) -> SignatureScheme {
        self.scheme
    }

    /// Check a delivery's signature against its raw body.
    ///
    /// @param headers - Request headers.
    /// @param body - The raw request body, exactly as received.
    /// @param now_secs - Current Unix time, for the replay window of timestamped schemes.
    /// @throws SignatureError - When the header is missing, malformed, too old, or wrong.
    pub fn verify(&self, headers: &HeaderMap, body: &[u8], now_secs: u64) -> Result<(), SignatureError> {
        let value = header_text(headers, &self.header)?;
        match self.scheme {
            SignatureScheme::HmacSha256 | SignatureScheme::Github => {
                let expected = hmac_sha256(&self.secret, &[body]);
                self.check_prefixed_hex(value, &expected)
            }
            SignatureScheme::Slack => {
                let timestamp = header_text(headers, "x-slack-request-timestamp")?;
                self.check_timestamp(timestamp, "x-slack-request-timestamp", now_secs)?;
                let expected = hmac_sha256(&self.secret, &[b"v0:", timestamp.as_bytes(), b":", body]);
                self.check_prefixed_hex(value, &expected)
            }
            SignatureScheme::Stripe | SignatureScheme::Graphrt => self.check_timestamped(value, body, now_secs),
        }
    }

    fn check_prefixed_hex(&self, value: &str, expected: &[u8; 32]) -> Result<(), SignatureError> {
        let hex = value.trim().strip_prefix(self.prefix.as_str()).ok_or_else(|| self.malformed())?;
        let provided = from_hex(hex).ok_or_else(|| self.malformed())?;
        if constant_time_eq(&provided, expected) { Ok(()) } else { Err(SignatureError::Mismatch) }
    }

    /// `t=<unix>,v1=<hex>[,v1=<hex>...]`; any `v1` may match (secret rotation).
    fn check_timestamped(&self, value: &str, body: &[u8], now_secs: u64) -> Result<(), SignatureError> {
        let mut timestamp = None;
        let mut candidates = Vec::new();
        for part in value.split(',') {
            match part.trim().split_once('=') {
                Some(("t", t)) => timestamp = Some(t),
                Some(("v1", hex)) => candidates.push(from_hex(hex).ok_or_else(|| self.malformed())?),
                _ => {}
            }
        }
        let timestamp = timestamp.ok_or_else(|| self.malformed())?;
        if candidates.is_empty() {
            return Err(self.malformed());
        }
        self.check_timestamp(timestamp, &self.header, now_secs)?;
        let expected = hmac_sha256(&self.secret, &[timestamp.as_bytes(), b".", body]);
        // Compare every candidate so timing does not reveal which one matched.
        let matched = candidates.iter().fold(false, |found, candidate| constant_time_eq(candidate, &expected) | found);
        if matched { Ok(()) } else { Err(SignatureError::Mismatch) }
    }

    fn check_timestamp(&self, timestamp: &str, header: &str, now_secs: u64) -> Result<(), SignatureError> {
        let seconds: u64 =
            timestamp.trim().parse().map_err(|_| SignatureError::Malformed(header.to_ascii_lowercase()))?;
        if now_secs.abs_diff(seconds) > self.tolerance_secs { Err(SignatureError::Expired) } else { Ok(()) }
    }

    fn malformed(&self) -> SignatureError {
        SignatureError::Malformed(self.header.to_ascii_lowercase())
    }
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, SignatureError> {
    headers
        .get(name)
        .ok_or_else(|| SignatureError::Missing(name.to_ascii_lowercase()))?
        .to_str()
        .map_err(|_| SignatureError::Malformed(name.to_ascii_lowercase()))
}

/// graphrt's signature header value for an outbound body: `t=<unix>,v1=<hex>`.
pub fn sign_graphrt(secret: &[u8], timestamp_secs: u64, body: &[u8]) -> String {
    let timestamp = timestamp_secs.to_string();
    let mac = hmac_sha256(secret, &[timestamp.as_bytes(), b".", body]);
    format!("t={timestamp},v1={}", to_hex(&mac))
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        let case1 = hmac_sha256(&[0x0b; 20], &[b"Hi There"]);
        assert_eq!(to_hex(&case1), "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7");
        let case2 = hmac_sha256(b"Jefe", &[b"what do ya ", b"want for nothing?"]);
        assert_eq!(to_hex(&case2), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
        // Key longer than one block: hashed first.
        let case6 = hmac_sha256(&[0xaa; 131], &[b"Test Using Larger Than Block-Size Key - Hash Key First"]);
        assert_eq!(to_hex(&case6), "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54");
    }

    #[test]
    fn github_style() {
        let verifier = Verifier::new(SignatureScheme::Github, b"s3cret".to_vec(), None, None);
        let body = br#"{"zen":"hi"}"#;
        let good = format!("sha256={}", to_hex(&hmac_sha256(b"s3cret", &[body])));
        assert_eq!(verifier.verify(&headers(&[("x-hub-signature-256", &good)]), body, 0), Ok(()));
        let wrong = format!("sha256={}", to_hex(&hmac_sha256(b"other", &[body])));
        assert_eq!(
            verifier.verify(&headers(&[("x-hub-signature-256", &wrong)]), body, 0),
            Err(SignatureError::Mismatch)
        );
        assert!(matches!(verifier.verify(&HeaderMap::new(), body, 0), Err(SignatureError::Missing(_))));
        assert!(matches!(
            verifier.verify(&headers(&[("x-hub-signature-256", "md5=00")]), body, 0),
            Err(SignatureError::Malformed(_))
        ));
        // A tampered body fails.
        assert_eq!(
            verifier.verify(&headers(&[("x-hub-signature-256", &good)]), b"{}", 0),
            Err(SignatureError::Mismatch)
        );
    }

    #[test]
    fn stripe_style_with_replay_window() {
        let verifier = Verifier::new(SignatureScheme::Stripe, b"whsec".to_vec(), None, None);
        let body = br#"{"id":"evt_1"}"#;
        let sig = hmac_sha256(b"whsec", &[b"1000.", body]);
        let value = format!("t=1000,v1={},v1=00", to_hex(&sig));
        assert_eq!(verifier.verify(&headers(&[("stripe-signature", &value)]), body, 1100), Ok(()));
        assert_eq!(
            verifier.verify(&headers(&[("stripe-signature", &value)]), body, 2000),
            Err(SignatureError::Expired)
        );
        let only_v0 = format!("t=1000,v0={}", to_hex(&sig));
        assert!(matches!(
            verifier.verify(&headers(&[("stripe-signature", &only_v0)]), body, 1000),
            Err(SignatureError::Malformed(_))
        ));
    }

    #[test]
    fn slack_style() {
        let verifier = Verifier::new(SignatureScheme::Slack, b"slk".to_vec(), None, None);
        let body = b"{\"type\":\"event_callback\"}";
        let sig = format!("v0={}", to_hex(&hmac_sha256(b"slk", &[b"v0:500:", body])));
        let ok = headers(&[("x-slack-signature", &sig), ("x-slack-request-timestamp", "500")]);
        assert_eq!(verifier.verify(&ok, body, 510), Ok(()));
        let no_ts = headers(&[("x-slack-signature", &sig)]);
        assert!(matches!(verifier.verify(&no_ts, body, 510), Err(SignatureError::Missing(_))));
    }

    #[test]
    fn graphrt_signatures_verify_with_graphrt_scheme() {
        let body = br#"{"type":"run.succeeded"}"#;
        let value = sign_graphrt(b"cb-secret", 42, body);
        let verifier = Verifier::new(SignatureScheme::Graphrt, b"cb-secret".to_vec(), None, None);
        assert_eq!(verifier.verify(&headers(&[(GRAPHRT_SIGNATURE_HEADER, &value)]), body, 42), Ok(()));
    }

    #[test]
    fn debug_hides_the_secret() {
        let verifier = Verifier::new(SignatureScheme::Github, b"topsecret".to_vec(), None, None);
        assert!(!format!("{verifier:?}").contains("topsecret"));
    }
}
