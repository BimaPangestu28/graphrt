//! Request signatures between a graphrt gateway and its agents (`Graphrt-Sig`).
//!
//! ```text
//! Graphrt-Sig: v1;kid=<kid>;ts=<unix seconds>;n=<nonce>;mac=<64 hex digits>
//! mac = HMAC-SHA256(K[kid], "v1|<kid>|<deployment>|<METHOD>|<path?query>|<ts>|<nonce>|<run_id>|<epoch>|<hex sha256(body)>")
//! ```
//!
//! The nonce (8-64 chars of `[A-Za-z0-9_-]`, unique per request) lets two
//! identical requests in the same second (two `hello` polls, a retried abort)
//! carry different signatures, so replay detection never refuses a legitimate
//! repeat.
//!
//! `deployment`, `run_id` and `epoch` come from the `Graphrt-Deployment`,
//! `Graphrt-Run-Id` and `Graphrt-Epoch` headers (empty when absent), so a
//! signature binds the request to one deployment, one run execution and one
//! body. Only the gateway signs: agents never call it (it pulls their orphans).
//! [`sign`] is the gateway's side, kept here for tests and other signers.
//!
//! A verifier accepts signatures within ±[`MAX_SKEW_SECS`] of its clock, from
//! any configured key id (two can be active during a rotation), and refuses a
//! signature it has already accepted (replay), remembering each one for as long
//! as its timestamp can stay inside the window.
//!
//! HMAC is computed with the `sha2` crate `serve` already links, so this adds no
//! dependency; keyed hasher states are prepared once per key.

use std::collections::{HashSet, VecDeque};
use std::sync::{Mutex, PoisonError};

use sha2::{Digest, Sha256};

/// Signature scheme version in `Graphrt-Sig` and the canonical string.
pub const SCHEME: &str = "v1";
/// Accepted clock difference between signer and verifier, in seconds.
pub const MAX_SKEW_SECS: u64 = 30;
/// Signature header.
pub const SIGNATURE_HEADER: &str = "graphrt-sig";
/// Deployment the request belongs to.
pub const DEPLOYMENT_HEADER: &str = "graphrt-deployment";
/// Run the request is about.
pub const RUN_ID_HEADER: &str = "graphrt-run-id";
/// Execution epoch of that run.
pub const EPOCH_HEADER: &str = "graphrt-epoch";
/// Shortest accepted key, in bytes.
pub const MIN_KEY_BYTES: usize = 32;
/// Most accepted signatures remembered for replay detection; beyond it new
/// requests are refused until old entries expire (fail closed).
const MAX_REMEMBERED: usize = 100_000;

/// One HMAC key with its id. `Debug` never shows the secret.
#[derive(Clone)]
pub struct GatewayKey {
    kid: String,
    /// SHA-256 states after absorbing the inner and outer padded key.
    inner: Sha256,
    outer: Sha256,
}

impl std::fmt::Debug for GatewayKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayKey").field("kid", &self.kid).finish_non_exhaustive()
    }
}

impl GatewayKey {
    /// A key from its id and secret bytes.
    ///
    /// @throws String - When the id is not 1-64 chars of `[A-Za-z0-9_.-]` or the
    /// secret is shorter than [`MIN_KEY_BYTES`].
    pub fn new(kid: &str, secret: &[u8]) -> Result<Self, String> {
        let kid_ok = !kid.is_empty()
            && kid.len() <= 64
            && kid.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if !kid_ok {
            return Err(format!("invalid gateway key id `{kid}` (1-64 chars of A-Z a-z 0-9 _ . -)"));
        }
        if secret.len() < MIN_KEY_BYTES {
            return Err(format!("gateway key `{kid}` is shorter than {MIN_KEY_BYTES} bytes"));
        }
        let mut block = [0u8; 64];
        if secret.len() > block.len() {
            block.iter_mut().zip(Sha256::digest(secret)).for_each(|(b, k)| *b = k);
        } else {
            block.iter_mut().zip(secret).for_each(|(b, k)| *b = *k);
        }
        let pad = |byte: u8| {
            let padded: Vec<u8> = block.iter().map(|b| b ^ byte).collect();
            let mut hasher = Sha256::new();
            hasher.update(&padded);
            hasher
        };
        Ok(Self { kid: kid.to_owned(), inner: pad(0x36), outer: pad(0x5c) })
    }

    /// The key id.
    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// HMAC-SHA256 of `message` under this key.
    pub fn mac(&self, message: &[u8]) -> [u8; 32] {
        let mut inner = self.inner.clone();
        inner.update(message);
        let mut outer = self.outer.clone();
        outer.update(inner.finalize());
        outer.finalize().into()
    }
}

/// Parse a key list `kid:hexsecret[,kid:hexsecret]` (the value of
/// `$GRAPHRT_GATEWAY_KEYS`). The first key signs; every key verifies.
///
/// @throws String - On an empty list, a malformed entry, a duplicate id or a short key.
///
/// @example
/// let keys = parse_keys(&format!("k1:{}", "ab".repeat(32))).unwrap();
pub fn parse_keys(text: &str) -> Result<Vec<GatewayKey>, String> {
    let mut keys: Vec<GatewayKey> = Vec::new();
    for entry in text.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let (kid, hex) =
            entry.split_once(':').ok_or_else(|| "gateway keys are `kid:hexsecret`, comma-separated".to_owned())?;
        let secret = decode_hex(hex.trim()).ok_or_else(|| format!("gateway key `{kid}` is not hex"))?;
        if keys.iter().any(|k| k.kid == kid) {
            return Err(format!("gateway key id `{kid}` appears twice"));
        }
        keys.push(GatewayKey::new(kid, &secret)?);
    }
    if keys.is_empty() {
        return Err("no gateway key configured".to_owned());
    }
    Ok(keys)
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let digit = |c: u8| (c as char).to_digit(16).and_then(|d| u8::try_from(d).ok());
    text.as_bytes().chunks(2).map(|pair| Some(digit(*pair.first()?)? << 4 | digit(*pair.get(1)?)?)).collect()
}

fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
        let _infallible = write!(out, "{b:02x}");
        out
    })
}

/// What a signature covers besides the key and timestamp.
#[derive(Clone, Copy, Debug)]
pub struct Signed<'a> {
    pub deployment: &'a str,
    pub method: &'a str,
    /// Path with query string, as sent on the request line.
    pub path: &'a str,
    pub run_id: &'a str,
    /// Decimal epoch, or empty.
    pub epoch: &'a str,
    pub body: &'a [u8],
}

fn canonical(kid: &str, ts: u64, nonce: &str, signed: &Signed<'_>) -> Vec<u8> {
    let body_hash = encode_hex(&Sha256::digest(signed.body));
    let Signed { deployment, method, path, run_id, epoch, .. } = signed;
    format!("{SCHEME}|{kid}|{deployment}|{method}|{path}|{ts}|{nonce}|{run_id}|{epoch}|{body_hash}").into_bytes()
}

fn nonce_ok(nonce: &str) -> bool {
    (8..=64).contains(&nonce.len()) && nonce.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// A nonce unique to this process and call: a hash of the process id, the
/// time and a counter (it needs to be unique, not secret).
pub fn fresh_nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let digest = Sha256::digest(format!("{}|{nanos}|{count}", std::process::id()));
    encode_hex(digest.get(..12).unwrap_or_default())
}

/// The `Graphrt-Sig` value signing `signed` with `key` at `ts` (unix seconds),
/// with a fresh nonce.
pub fn sign(key: &GatewayKey, ts: u64, signed: &Signed<'_>) -> String {
    sign_with_nonce(key, ts, &fresh_nonce(), signed)
}

/// [`sign`] with a given nonce (for tests and other signers).
pub fn sign_with_nonce(key: &GatewayKey, ts: u64, nonce: &str, signed: &Signed<'_>) -> String {
    let mac = key.mac(&canonical(&key.kid, ts, nonce, signed));
    format!("{SCHEME};kid={};ts={ts};n={nonce};mac={}", key.kid, encode_hex(&mac))
}

/// Why a signature was refused. The messages name no secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignatureError {
    Missing,
    Malformed,
    UnknownKey(String),
    /// Timestamp outside the accepted window (seconds of difference).
    Expired(u64),
    BadMac,
    Replayed,
    /// Too many signatures to remember; retry shortly.
    Busy,
}

impl std::error::Error for SignatureError {}

impl SignatureError {
    /// A short, static description (no secret, no request data).
    pub fn as_str(&self) -> &'static str {
        match self {
            SignatureError::Missing => "missing Graphrt-Sig",
            SignatureError::Malformed => "malformed Graphrt-Sig",
            SignatureError::UnknownKey(_) => "unknown key id",
            SignatureError::Expired(_) => "timestamp outside the +-30 s window",
            SignatureError::BadMac => "signature mismatch",
            SignatureError::Replayed => "signature already used",
            SignatureError::Busy => "replay window full; retry",
        }
    }
}

impl std::fmt::Display for SignatureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

struct ParsedSignature<'a> {
    kid: &'a str,
    ts: u64,
    nonce: &'a str,
    mac: [u8; 32],
}

fn parse_signature(header: &str) -> Option<ParsedSignature<'_>> {
    let mut parts = header.split(';');
    if parts.next()? != SCHEME {
        return None;
    }
    let (mut kid, mut ts, mut nonce, mut mac) = (None, None, None, None);
    for part in parts {
        match part.split_once('=')? {
            ("kid", value) => kid = Some(value),
            ("ts", value) => ts = value.parse().ok(),
            ("n", value) => nonce = Some(value).filter(|n| nonce_ok(n)),
            ("mac", value) => mac = decode_hex(value).and_then(|bytes| <[u8; 32]>::try_from(bytes).ok()),
            _ => return None,
        }
    }
    Some(ParsedSignature { kid: kid?, ts: ts?, nonce: nonce?, mac: mac? })
}

/// Checks signatures against a key set and remembers accepted ones.
#[derive(Debug)]
pub struct Verifier {
    keys: Vec<GatewayKey>,
    seen: Mutex<Seen>,
}

#[derive(Debug, Default)]
struct Seen {
    macs: HashSet<[u8; 32]>,
    /// Accepted signatures by acceptance time, oldest first.
    order: VecDeque<(u64, [u8; 32])>,
}

impl Verifier {
    /// A verifier accepting any of `keys`; the first one signs outgoing requests.
    pub fn new(keys: Vec<GatewayKey>) -> Self {
        Self { keys, seen: Mutex::new(Seen::default()) }
    }

    /// The key outgoing requests are signed with.
    pub fn signing_key(&self) -> Option<&GatewayKey> {
        self.keys.first()
    }

    /// Verify `header` for `signed` at `now` (unix seconds) and remember it.
    ///
    /// @throws SignatureError - When the header is missing or malformed, the key id
    /// unknown, the timestamp outside ±[`MAX_SKEW_SECS`], the MAC wrong, or the
    /// signature already used.
    pub fn verify(&self, header: Option<&str>, signed: &Signed<'_>, now: u64) -> Result<(), SignatureError> {
        let parsed = parse_signature(header.ok_or(SignatureError::Missing)?).ok_or(SignatureError::Malformed)?;
        let key = self
            .keys
            .iter()
            .find(|k| k.kid == parsed.kid)
            .ok_or_else(|| SignatureError::UnknownKey(parsed.kid.chars().take(64).collect()))?;
        let skew = now.abs_diff(parsed.ts);
        if skew > MAX_SKEW_SECS {
            return Err(SignatureError::Expired(skew));
        }
        let expected = key.mac(&canonical(parsed.kid, parsed.ts, parsed.nonce, signed));
        if !crate::serve::constant_time_eq(&expected, &parsed.mac) {
            return Err(SignatureError::BadMac);
        }
        self.remember(parsed.mac, now)
    }

    fn remember(&self, mac: [u8; 32], now: u64) -> Result<(), SignatureError> {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        // A signature stays acceptable while now - skew <= ts <= accepted + skew.
        let horizon = now.saturating_sub(2 * MAX_SKEW_SECS + 1);
        while let Some(&(accepted, old)) = seen.order.front() {
            if accepted >= horizon {
                break;
            }
            seen.order.pop_front();
            seen.macs.remove(&old);
        }
        if seen.macs.contains(&mac) {
            return Err(SignatureError::Replayed);
        }
        if seen.macs.len() >= MAX_REMEMBERED {
            return Err(SignatureError::Busy);
        }
        seen.macs.insert(mac);
        seen.order.push_back((now, mac));
        Ok(())
    }
}

/// Current Unix time in seconds.
pub fn unix_now() -> u64 {
    graphrt_core::store::now_ms() / 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(kid: &str) -> GatewayKey {
        GatewayKey::new(kid, &[7u8; 32]).unwrap()
    }

    fn request<'a>(body: &'a [u8]) -> Signed<'a> {
        Signed { deployment: "shop", method: "POST", path: "/internal/v1/runs", run_id: "run-1", epoch: "3", body }
    }

    #[test]
    fn hmac_matches_rfc_4231_test_case_2() {
        // Key "Jefe" is too short for gateway use, so build the states directly.
        let jefe = {
            let mut block = [0u8; 64];
            block.iter_mut().zip(b"Jefe").for_each(|(b, k)| *b = *k);
            let pad = |byte: u8| {
                let mut h = Sha256::new();
                h.update(block.iter().map(|b| b ^ byte).collect::<Vec<u8>>());
                h
            };
            GatewayKey { kid: "jefe".into(), inner: pad(0x36), outer: pad(0x5c) }
        };
        assert_eq!(
            encode_hex(&jefe.mac(b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn long_keys_are_hashed_first_like_rfc_4231_test_case_6() {
        let long = GatewayKey::new("k", &[0xaa; 131]).unwrap();
        assert_eq!(
            encode_hex(&long.mac(b"Test Using Larger Than Block-Size Key - Hash Key First")),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn a_signed_request_verifies_once() {
        let verifier = Verifier::new(vec![key("k1")]);
        let header = sign_with_nonce(&key("k1"), 1_000, "nonce-001", &request(b"{}"));
        assert!(header.starts_with("v1;kid=k1;ts=1000;n=nonce-001;mac="), "{header}");
        assert_eq!(verifier.verify(Some(&header), &request(b"{}"), 1_010), Ok(()));
        assert_eq!(verifier.verify(Some(&header), &request(b"{}"), 1_011), Err(SignatureError::Replayed));
        // The same request signed again (another nonce) is a new request.
        let again = sign(&key("k1"), 1_000, &request(b"{}"));
        assert_eq!(verifier.verify(Some(&again), &request(b"{}"), 1_011), Ok(()));
        assert_ne!(fresh_nonce(), fresh_nonce());
    }

    #[test]
    fn every_signed_part_is_bound() {
        let verifier = Verifier::new(vec![key("k1")]);
        let header = sign(&key("k1"), 1_000, &request(b"{}"));
        let base = request(b"{}");
        let changed = [
            Signed { body: b"{ }", ..base },
            Signed { deployment: "other", ..base },
            Signed { method: "GET", ..base },
            Signed { path: "/internal/v1/runs?x=1", ..base },
            Signed { run_id: "run-2", ..base },
            Signed { epoch: "4", ..base },
        ];
        for signed in changed {
            assert_eq!(verifier.verify(Some(&header), &signed, 1_000), Err(SignatureError::BadMac), "{signed:?}");
        }
    }

    #[test]
    fn refuses_old_unknown_and_malformed_signatures() {
        let verifier = Verifier::new(vec![key("k1"), key("k2")]);
        let old = sign(&key("k1"), 1_000, &request(b""));
        assert_eq!(verifier.verify(Some(&old), &request(b""), 1_031), Err(SignatureError::Expired(31)));
        let future = sign(&key("k1"), 1_100, &request(b""));
        assert_eq!(verifier.verify(Some(&future), &request(b""), 1_000), Err(SignatureError::Expired(100)));
        // Rotation: the second key verifies too.
        let rotated = sign(&key("k2"), 1_000, &request(b""));
        assert_eq!(verifier.verify(Some(&rotated), &request(b""), 1_000), Ok(()));
        let unknown = sign(&key("k3"), 1_000, &request(b""));
        assert_eq!(verifier.verify(Some(&unknown), &request(b""), 1_000), Err(SignatureError::UnknownKey("k3".into())));
        let wrong_secret = sign(&GatewayKey::new("k1", &[8u8; 32]).unwrap(), 1_000, &request(b""));
        assert_eq!(verifier.verify(Some(&wrong_secret), &request(b""), 1_000), Err(SignatureError::BadMac));
        assert_eq!(verifier.verify(None, &request(b""), 1_000), Err(SignatureError::Missing));
        for bad in [
            "",
            "v2;kid=k1;ts=1;mac=00",
            "v1;kid=k1;ts=x;mac=00",
            "v1;kid=k1;ts=1",
            "v1;kid=k1;ts=1;mac=zz",
            &format!("v1;kid=k1;ts=1000;mac={}", "00".repeat(32)),
            &format!("v1;kid=k1;ts=1000;n=short;mac={}", "00".repeat(32)),
        ] {
            assert_eq!(verifier.verify(Some(bad), &request(b""), 1_000), Err(SignatureError::Malformed), "{bad}");
        }
    }

    #[test]
    fn remembered_signatures_expire_after_the_window() {
        let verifier = Verifier::new(vec![key("k1")]);
        let header = sign(&key("k1"), 1_000, &request(b""));
        assert_eq!(verifier.verify(Some(&header), &request(b""), 1_000), Ok(()));
        // Still inside its window: a replay.
        assert_eq!(verifier.verify(Some(&header), &request(b""), 1_030), Err(SignatureError::Replayed));
        // A later request prunes it; the old signature is then refused as expired anyway.
        let later = sign(&key("k1"), 1_100, &request(b""));
        assert_eq!(verifier.verify(Some(&later), &request(b""), 1_100), Ok(()));
        assert_eq!(verifier.seen.lock().unwrap().macs.len(), 1);
        assert_eq!(verifier.verify(Some(&header), &request(b""), 1_100), Err(SignatureError::Expired(100)));
    }

    #[test]
    fn key_lists_are_parsed_and_checked() {
        let hex = "ab".repeat(32);
        let keys = parse_keys(&format!("new:{hex}, old:{hex}")).unwrap();
        assert_eq!(keys.iter().map(GatewayKey::kid).collect::<Vec<_>>(), ["new", "old"]);
        assert!(!format!("{:?}", keys[0]).contains("ab"), "Debug hides the secret");
        for bad in [
            "",
            "nocolon",
            "k:zz",
            &format!("k:{}", "ab".repeat(31)),
            &format!("k:{hex},k:{hex}"),
            &format!("b d:{hex}"),
        ] {
            assert!(parse_keys(bad).is_err(), "{bad}");
        }
    }

    /// `cargo test --release -p graphrt-cli --lib -- --ignored hmac_verify_speed --nocapture`
    #[test]
    #[ignore = "timing; run in release"]
    #[allow(clippy::print_stdout)]
    fn hmac_verify_speed() {
        let verifier = Verifier::new(vec![key("k1")]);
        let body = vec![b'x'; 1024];
        let rounds = 20_000u64;
        let headers: Vec<String> = (0..rounds)
            .map(|i| sign(&key("k1"), 1_000, &Signed { run_id: &format!("run-{i}"), ..request(&body) }))
            .collect();
        let ids: Vec<String> = (0..rounds).map(|i| format!("run-{i}")).collect();
        let started = std::time::Instant::now();
        for (header, id) in headers.iter().zip(&ids) {
            verifier.verify(Some(header), &Signed { run_id: id, ..request(&body) }, 1_000).unwrap();
        }
        let per_verify = started.elapsed() / u32::try_from(rounds).unwrap();
        println!("HMAC verify of a 1 KiB body: {per_verify:?} per request");
        assert!(per_verify < std::time::Duration::from_micros(5), "{per_verify:?}");
    }
}
