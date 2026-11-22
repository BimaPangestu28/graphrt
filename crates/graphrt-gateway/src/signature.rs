//! `Graphrt-Sig` request signatures between the gateway and its agents (plan §3.2).
//!
//! Byte-compatible with the agent side (`graphrt serve --trust-gateway`,
//! `graphrt-cli/src/gateway_agent/signature.rs`):
//!
//! ```text
//! Graphrt-Sig: v1;kid=<kid>;ts=<unix seconds>;n=<nonce>;mac=<64 hex digits>
//! mac = HMAC-SHA256(K[kid], "v1|<kid>|<deployment>|<METHOD>|<path?query>|<ts>|<nonce>|<run_id>|<epoch>|<hex sha256(body)>")
//! ```
//!
//! The gateway signs every `/internal/v1/*` request with the deployment's
//! first key and verifies the agents' orphan reports with any of its keys
//! (two are active during a rotation). Replays are refused for as long as a
//! timestamp can stay inside the ±30 s window.

use std::collections::{HashSet, VecDeque};
use std::sync::Mutex;

use sha2::{Digest, Sha256};

/// Signature scheme version.
pub const SCHEME: &str = "v1";
/// Accepted clock difference, seconds.
pub const MAX_SKEW_SECS: u64 = 30;
/// Signature header.
pub const SIGNATURE_HEADER: &str = "graphrt-sig";
/// Deployment header.
pub const DEPLOYMENT_HEADER: &str = "graphrt-deployment";
/// Run id header.
pub const RUN_ID_HEADER: &str = "graphrt-run-id";
/// Epoch header.
pub const EPOCH_HEADER: &str = "graphrt-epoch";
/// Protocol header.
pub const PROTOCOL_HEADER: &str = "graphrt-protocol";
/// Protocol version spoken.
pub const PROTOCOL_VERSION: u32 = 1;
/// Shortest accepted key.
pub const MIN_KEY_BYTES: usize = 32;
const MAX_REMEMBERED: usize = 100_000;

/// One HMAC key; `Debug` never shows the secret.
#[derive(Clone)]
pub struct GatewayKey {
    kid: String,
    inner: Sha256,
    outer: Sha256,
}

impl std::fmt::Debug for GatewayKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("GatewayKey").field("kid", &self.kid).finish_non_exhaustive()
    }
}

impl GatewayKey {
    /// A key from its id and secret.
    ///
    /// # Errors
    /// Invalid id (1-64 of `[A-Za-z0-9_.-]`) or a secret under [`MIN_KEY_BYTES`].
    pub fn new(kid: &str, secret: &[u8]) -> Result<Self, String> {
        let kid_ok = !kid.is_empty()
            && kid.len() <= 64
            && kid.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if !kid_ok {
            return Err(format!("invalid gateway key id `{kid}`"));
        }
        if secret.len() < MIN_KEY_BYTES {
            return Err(format!("gateway key `{kid}` is shorter than {MIN_KEY_BYTES} bytes"));
        }
        let mut block = [0u8; 64];
        if secret.len() > block.len() {
            block.iter_mut().zip(Sha256::digest(secret)).for_each(|(slot, byte)| *slot = byte);
        } else {
            block.iter_mut().zip(secret).for_each(|(slot, byte)| *slot = *byte);
        }
        let padded_hasher = |pad: u8| {
            let padded: Vec<u8> = block.iter().map(|byte| byte ^ pad).collect();
            let mut hasher = Sha256::new();
            hasher.update(&padded);
            hasher
        };
        Ok(Self { kid: kid.to_owned(), inner: padded_hasher(0x36), outer: padded_hasher(0x5c) })
    }

    /// The key id.
    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// HMAC-SHA256 of `message`.
    pub fn mac(&self, message: &[u8]) -> [u8; 32] {
        let mut inner = self.inner.clone();
        inner.update(message);
        let mut outer = self.outer.clone();
        outer.update(inner.finalize());
        outer.finalize().into()
    }
}

/// Parses `kid:hexsecret[,kid:hexsecret]` (first key signs).
///
/// # Errors
/// Empty list, malformed entry, duplicate id or short key.
pub fn parse_keys(text: &str) -> Result<Vec<GatewayKey>, String> {
    let mut keys: Vec<GatewayKey> = Vec::new();
    for entry in text.split(',').map(str::trim).filter(|entry| !entry.is_empty()) {
        let (kid, hex) =
            entry.split_once(':').ok_or_else(|| "gateway keys are `kid:hexsecret`, comma-separated".to_owned())?;
        let secret = crate::hex::decode(hex.trim()).ok_or_else(|| format!("gateway key `{kid}` is not hex"))?;
        if keys.iter().any(|key| key.kid == kid) {
            return Err(format!("gateway key id `{kid}` appears twice"));
        }
        keys.push(GatewayKey::new(kid, &secret)?);
    }
    if keys.is_empty() {
        return Err("no gateway key configured".to_owned());
    }
    Ok(keys)
}

/// What a signature covers besides key and timestamp.
#[derive(Clone, Copy, Debug)]
pub struct Signed<'a> {
    /// Deployment name.
    pub deployment: &'a str,
    /// HTTP method.
    pub method: &'a str,
    /// Path with query.
    pub path: &'a str,
    /// Run id or empty.
    pub run_id: &'a str,
    /// Decimal epoch or empty.
    pub epoch: &'a str,
    /// Body bytes.
    pub body: &'a [u8],
}

fn canonical(kid: &str, timestamp: u64, nonce: &str, signed: &Signed<'_>) -> Vec<u8> {
    let body_hash = crate::hex::encode(&Sha256::digest(signed.body));
    let Signed { deployment, method, path, run_id, epoch, .. } = signed;
    format!("{SCHEME}|{kid}|{deployment}|{method}|{path}|{timestamp}|{nonce}|{run_id}|{epoch}|{body_hash}").into_bytes()
}

fn nonce_ok(nonce: &str) -> bool {
    (8..=64).contains(&nonce.len()) && nonce.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// A nonce unique to this process and call (unique, not secret), so two
/// identical requests in the same second still get different signatures.
pub fn fresh_nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_nanos());
    let digest = Sha256::digest(format!("{}|{nanos}|{count}", std::process::id()));
    crate::hex::encode(digest.get(..12).unwrap_or_default())
}

/// The `Graphrt-Sig` header value, with a fresh nonce.
pub fn sign(key: &GatewayKey, timestamp: u64, signed: &Signed<'_>) -> String {
    sign_with_nonce(key, timestamp, &fresh_nonce(), signed)
}

/// [`sign`] with a given nonce (tests).
pub fn sign_with_nonce(key: &GatewayKey, timestamp: u64, nonce: &str, signed: &Signed<'_>) -> String {
    let mac = key.mac(&canonical(&key.kid, timestamp, nonce, signed));
    format!("{SCHEME};kid={};ts={timestamp};n={nonce};mac={}", key.kid, crate::hex::encode(&mac))
}

/// Why a signature was refused (never names a secret).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignatureError {
    /// No header.
    #[error("missing Graphrt-Sig header")]
    Missing,
    /// Unparsable header.
    #[error("malformed Graphrt-Sig header")]
    Malformed,
    /// Unknown key id.
    #[error("unknown gateway key id")]
    UnknownKey,
    /// Outside the window.
    #[error("signature timestamp outside ±{MAX_SKEW_SECS} s")]
    Expired,
    /// Wrong MAC.
    #[error("signature does not match")]
    BadMac,
    /// Already used.
    #[error("signature was already used")]
    Replayed,
    /// Replay memory full.
    #[error("too many signed requests in the replay window")]
    Busy,
}

struct ParsedSignature<'a> {
    kid: &'a str,
    timestamp: u64,
    nonce: &'a str,
    mac: [u8; 32],
}

fn parse_signature(header: &str) -> Option<ParsedSignature<'_>> {
    let mut parts = header.split(';');
    if parts.next()? != SCHEME {
        return None;
    }
    let (mut kid, mut timestamp, mut nonce, mut mac) = (None, None, None, None);
    for part in parts {
        match part.split_once('=')? {
            ("kid", value) => kid = Some(value),
            ("ts", value) => timestamp = value.parse().ok(),
            ("n", value) => nonce = Some(value).filter(|nonce| nonce_ok(nonce)),
            ("mac", value) => mac = crate::hex::decode(value).and_then(|bytes| <[u8; 32]>::try_from(bytes).ok()),
            _ => return None,
        }
    }
    Some(ParsedSignature { kid: kid?, timestamp: timestamp?, nonce: nonce?, mac: mac? })
}

#[derive(Debug, Default)]
struct SeenSignatures {
    macs: HashSet<[u8; 32]>,
    order: VecDeque<(u64, [u8; 32])>,
}

/// Verifies signatures against a key set, with replay memory.
#[derive(Debug)]
pub struct Verifier {
    keys: Vec<GatewayKey>,
    seen: Mutex<SeenSignatures>,
}

impl Verifier {
    /// A verifier over `keys`; the first one signs.
    pub fn new(keys: Vec<GatewayKey>) -> Self {
        Self { keys, seen: Mutex::new(SeenSignatures::default()) }
    }

    /// The signing key.
    pub fn signing_key(&self) -> Option<&GatewayKey> {
        self.keys.first()
    }

    /// Verifies `header` for `signed` at `now` (unix seconds).
    ///
    /// # Errors
    /// See [`SignatureError`].
    pub fn verify(&self, header: Option<&str>, signed: &Signed<'_>, now: u64) -> Result<(), SignatureError> {
        let parsed = parse_signature(header.ok_or(SignatureError::Missing)?).ok_or(SignatureError::Malformed)?;
        let key = self.keys.iter().find(|key| key.kid == parsed.kid).ok_or(SignatureError::UnknownKey)?;
        if parsed.timestamp.abs_diff(now) > MAX_SKEW_SECS {
            return Err(SignatureError::Expired);
        }
        let expected = key.mac(&canonical(&key.kid, parsed.timestamp, parsed.nonce, signed));
        if !crate::hex::constant_time_eq(&expected, &parsed.mac) {
            return Err(SignatureError::BadMac);
        }
        let mut seen = self.seen.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        while seen.order.front().is_some_and(|(accepted, _)| accepted.saturating_add(2 * MAX_SKEW_SECS) < now) {
            if let Some((_, mac)) = seen.order.pop_front() {
                seen.macs.remove(&mac);
            }
        }
        if seen.macs.contains(&parsed.mac) {
            return Err(SignatureError::Replayed);
        }
        if seen.macs.len() >= MAX_REMEMBERED {
            return Err(SignatureError::Busy);
        }
        seen.macs.insert(parsed.mac);
        seen.order.push_back((now, parsed.mac));
        Ok(())
    }
}

/// Current unix seconds.
pub fn unix_now() -> u64 {
    graphrt_core::store::now_ms() / 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> GatewayKey {
        GatewayKey::new("k1", &[7u8; 32]).unwrap()
    }

    fn signed(body: &[u8]) -> Signed<'_> {
        Signed { deployment: "shop", method: "POST", path: "/internal/v1/runs", run_id: "r1", epoch: "1", body }
    }

    #[test]
    fn hmac_matches_rfc4231_case_6() {
        // RFC 4231 test case 6: a key longer than the block is hashed first.
        let long = GatewayKey::new("k", &[0xaa; 131]).unwrap();
        let mac = long.mac(b"Test Using Larger Than Block-Size Key - Hash Key First");
        assert_eq!(crate::hex::encode(&mac), "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54");
    }

    #[test]
    fn sign_then_verify_and_refuse_replay() {
        let verifier = Verifier::new(vec![key()]);
        let header = sign(&key(), 1000, &signed(b"{}"));
        assert!(verifier.verify(Some(&header), &signed(b"{}"), 1010).is_ok());
        assert_eq!(verifier.verify(Some(&header), &signed(b"{}"), 1010), Err(SignatureError::Replayed));
        let other = sign(&key(), 1001, &signed(b"{}"));
        assert_eq!(verifier.verify(Some(&other), &signed(b"{\"x\":1}"), 1001), Err(SignatureError::BadMac));
        assert_eq!(verifier.verify(Some(&other), &signed(b"{}"), 1100), Err(SignatureError::Expired));
        assert_eq!(verifier.verify(None, &signed(b"{}"), 1001), Err(SignatureError::Missing));
        let fixed = sign_with_nonce(&key(), 1000, "abcdefgh", &signed(b"{}"));
        assert!(fixed.contains(";n=abcdefgh;"));
        assert_ne!(sign(&key(), 1000, &signed(b"{}")), sign(&key(), 1000, &signed(b"{}")), "fresh nonce per call");
        assert_eq!(
            verifier.verify(Some("v1;kid=k1;ts=1000;mac=00"), &signed(b"{}"), 1000),
            Err(SignatureError::Malformed),
            "nonce required"
        );
    }

    #[test]
    fn parses_key_lists() {
        let keys = parse_keys(&format!("a:{},b:{}", "11".repeat(32), "22".repeat(32))).unwrap();
        assert_eq!(keys.iter().map(GatewayKey::kid).collect::<Vec<_>>(), vec!["a", "b"]);
        assert!(parse_keys("a:12").is_err());
        assert!(parse_keys("").is_err());
    }
}
