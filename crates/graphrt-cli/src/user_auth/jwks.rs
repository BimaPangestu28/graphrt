//! JSON Web Key Sets: parsing keys and keeping a fetched set fresh.
//!
//! The set is fetched once at startup (a failure stops the server, so a typo in
//! the URL is caught at deploy time), refreshed every [`REFRESH_INTERVAL`] in the
//! background, and refreshed early when a token names an unknown `kid` (key
//! rotation), at most once per [`MIN_REFETCH_INTERVAL`] so a flood of forged
//! `kid`s cannot hammer the identity provider. Verification itself never waits
//! on the network except for that rare early refresh.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use super::token::{Algorithm, KeyMaterial, VerifyKey};

/// Background refresh period of a fetched key set.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(600);
/// Minimum gap between two fetches triggered by unknown `kid`s.
pub const MIN_REFETCH_INTERVAL: Duration = Duration::from_secs(30);
/// Largest key set accepted, in bytes.
const MAX_JWKS_BYTES: usize = 256 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// One JWK as a verification key; `None` for keys that cannot verify JWTs here
/// (encryption keys, unsupported key types or curves, malformed members).
pub fn parse_jwk(jwk: &serde_json::Value) -> Option<VerifyKey> {
    let field = |name: &str| jwk.get(name).and_then(serde_json::Value::as_str);
    if field("use").is_some_and(|u| u != "sig") {
        return None;
    }
    let declared = field("alg");
    let key_id = field("kid").map(str::to_owned);
    let decode = |name: &str| field(name).and_then(|v| URL_SAFE_NO_PAD.decode(v).ok());
    let (algorithm, material) = match field("kty")? {
        "EC" if field("crv") == Some("P-256") => {
            let (x, y) = (decode("x")?, decode("y")?);
            if x.len() != 32 || y.len() != 32 {
                return None;
            }
            let mut point = Vec::with_capacity(65);
            point.push(0x04);
            point.extend_from_slice(&x);
            point.extend_from_slice(&y);
            (Algorithm::Es256, KeyMaterial::EcP256(point))
        }
        "RSA" => (Algorithm::Rs256, KeyMaterial::Rsa { modulus: decode("n")?, exponent: decode("e")? }),
        _ => return None,
    };
    if declared.is_some_and(|alg| Algorithm::from_name(alg) != Some(algorithm)) {
        return None;
    }
    Some(VerifyKey { key_id, algorithm, material })
}

/// The usable keys of a JWKS document (`{"keys": [...]}`).
///
/// @throws String - When the document is not a JWKS or holds no usable key.
pub fn parse_jwks(body: &[u8]) -> Result<Vec<VerifyKey>, String> {
    let document: serde_json::Value = serde_json::from_slice(body).map_err(|e| format!("not JSON: {e}"))?;
    let keys = document.get("keys").and_then(serde_json::Value::as_array).ok_or("no `keys` array")?;
    let keys: Vec<VerifyKey> = keys.iter().filter_map(parse_jwk).collect();
    if keys.is_empty() {
        return Err("no ES256 or RS256 signing key".into());
    }
    Ok(keys)
}

/// A key set fetched from a URL, swapped atomically on refresh.
#[derive(Debug)]
pub struct RemoteKeySet {
    url: String,
    client: reqwest::Client,
    keys: RwLock<Arc<Vec<VerifyKey>>>,
    /// When the last fetch (successful or not) started; serializes refreshes.
    last_fetch: tokio::sync::Mutex<Instant>,
}

impl RemoteKeySet {
    /// Fetch `url` once and return the live set.
    ///
    /// @throws String - When the URL is not https (except loopback) or the first fetch fails.
    pub async fn fetch(url: &str) -> Result<Arc<Self>, String> {
        check_url(url)?;
        let _already_installed = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .connect_timeout(FETCH_TIMEOUT)
            .timeout(FETCH_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("graphrt/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| format!("cannot build HTTP client: {e}"))?;
        let keys = download(&client, url).await?;
        Ok(Arc::new(Self {
            url: url.to_owned(),
            client,
            keys: RwLock::new(Arc::new(keys)),
            last_fetch: tokio::sync::Mutex::new(Instant::now()),
        }))
    }

    /// The current keys.
    pub fn keys(&self) -> Arc<Vec<VerifyKey>> {
        self.keys.read().map(|k| Arc::clone(&k)).unwrap_or_else(|poisoned| Arc::clone(&poisoned.into_inner()))
    }

    /// Re-fetch unless a fetch started less than `min_gap` ago; keeps the old keys on failure.
    ///
    /// @returns Whether a fetch ran and replaced the keys.
    pub async fn refresh(&self, min_gap: Duration) -> bool {
        let mut last = self.last_fetch.lock().await;
        if last.elapsed() < min_gap {
            return false;
        }
        *last = Instant::now();
        match download(&self.client, &self.url).await {
            Ok(keys) => {
                let mut slot = self.keys.write().unwrap_or_else(std::sync::PoisonError::into_inner);
                *slot = Arc::new(keys);
                true
            }
            Err(e) => {
                eprintln!(
                    "warning: cannot refresh the JWT key set ({}): {e}; keeping the previous keys",
                    graphrt_core::urls::redact_userinfo(&self.url)
                );
                false
            }
        }
    }

    /// Refresh every [`REFRESH_INTERVAL`] for as long as the set is in use.
    pub fn spawn_refresh(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(REFRESH_INTERVAL);
            ticker.tick().await; // the first tick is immediate; the set was just fetched
            loop {
                ticker.tick().await;
                let Some(set) = weak.upgrade() else { return };
                set.refresh(MIN_REFETCH_INTERVAL).await;
            }
        });
    }
}

fn check_url(url: &str) -> Result<(), String> {
    let shown = graphrt_core::urls::redact_userinfo(url);
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid JWKS URL `{shown}`: {e}"))?;
    let loopback = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if parsed.scheme() == "https" || (parsed.scheme() == "http" && loopback) {
        Ok(())
    } else {
        Err(format!("JWKS URL `{shown}` must be https (http only for localhost)"))
    }
}

async fn download(client: &reqwest::Client, url: &str) -> Result<Vec<VerifyKey>, String> {
    let mut response = client.get(url).send().await.map_err(|e| e.without_url().to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.without_url().to_string())? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_JWKS_BYTES {
            return Err(format!("larger than {MAX_JWKS_BYTES} bytes"));
        }
    }
    parse_jwks(&body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const X: &str = "f83OJ3D2xF1Bg8vub9tLe1gHMzV76e8Tus9uPHvRVEU";
    const Y: &str = "x_FEzRu9m36HLN_tue659LNpXW6pCyStikYjKIWI5a0";

    #[test]
    fn parses_ec_and_rsa_signing_keys() {
        let doc = json!({ "keys": [
            { "kty": "EC", "crv": "P-256", "x": X, "y": Y, "kid": "a", "alg": "ES256", "use": "sig" },
            { "kty": "RSA", "n": "AQAB", "e": "AQAB", "kid": "b" },
            { "kty": "EC", "crv": "P-384", "x": X, "y": Y },
            { "kty": "oct", "k": "c2VjcmV0" },
            { "kty": "EC", "crv": "P-256", "x": X, "y": Y, "use": "enc" },
            { "kty": "EC", "crv": "P-256", "x": X, "y": Y, "alg": "RS256" },
            { "kty": "EC", "crv": "P-256", "x": "AQAB", "y": Y }
        ]});
        let keys = parse_jwks(doc.to_string().as_bytes()).unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!((keys[0].key_id.as_deref(), keys[0].algorithm), (Some("a"), Algorithm::Es256));
        assert_eq!((keys[1].key_id.as_deref(), keys[1].algorithm), (Some("b"), Algorithm::Rs256));
    }

    #[test]
    fn refuses_documents_without_usable_keys() {
        assert!(parse_jwks(b"nope").is_err());
        assert!(parse_jwks(br#"{"keys": []}"#).is_err());
        assert!(parse_jwks(br#"{"keys": [{"kty": "oct", "k": "AA"}]}"#).is_err());
    }

    #[test]
    fn jwks_urls_must_be_https_outside_loopback() {
        assert!(check_url("https://p.supabase.co/auth/v1/.well-known/jwks.json").is_ok());
        assert!(check_url("http://127.0.0.1:54321/auth/v1/.well-known/jwks.json").is_ok());
        assert!(check_url("http://p.supabase.co/jwks").is_err());
        assert!(check_url("file:///etc/passwd").is_err());
    }
}
