//! Secrets of the served workflows: environment variables an owner sets in
//! the console, encrypted at rest, never shown again.
//!
//! Each value is sealed with AES-256-GCM under the console key (32 bytes from
//! an environment variable or a file, see [`crate::config::SecretsConfig`]),
//! with a fresh random nonce and the variable's name as associated data, so
//! a sealed value cannot be moved to another name. `secrets.json` holds the
//! sealed values and a fingerprint of the key, so a wrong key is reported at
//! startup instead of failing value by value.
//!
//! The values are given to the served workflows at startup like `--env-file`
//! entries; they win over the process environment. Names that start with
//! `GRAPHRT_` configure the server itself and are refused.

use std::collections::BTreeMap;
use std::path::Path;

use self::zeroize_lite::Zeroizing;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use serde::{Deserialize, Serialize};

use crate::error::{ConsoleError, Result};
use crate::util::{JsonFile, now_ms, random_bytes, sha256_hex};

/// The longest value, in bytes.
pub const MAX_VALUE_BYTES: usize = 64 * 1024;
/// The most secrets kept.
pub const MAX_SECRETS: usize = 500;
const AAD_PREFIX: &str = "graphrt-console-secret:";

/// One sealed secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Sealed {
    nonce: String,
    ciphertext: String,
    updated_ms: u64,
    updated_by: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct SecretsFile {
    /// SHA-256 of a value derived from the key: which key sealed these.
    #[serde(default)]
    key_fingerprint: Option<String>,
    #[serde(default)]
    secrets: BTreeMap<String, Sealed>,
}

/// A secret as listed: its name and when it changed, never its value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SecretInfo {
    pub name: String,
    pub updated_ms: u64,
    pub updated_by: String,
}

/// The console key.
pub struct SecretKey {
    key: LessSafeKey,
    fingerprint: String,
}

impl std::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretKey").field("fingerprint", &self.fingerprint).finish_non_exhaustive()
    }
}

impl SecretKey {
    /// A key from its text: 32 bytes as base64 (standard or URL-safe) or hex.
    ///
    /// @throws ConsoleError::Crypto - When the text is not 32 bytes in one of those forms.
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        let bytes = Zeroizing(
            decode_hex(text)
                .or_else(|| STANDARD.decode(text).ok())
                .or_else(|| URL_SAFE_NO_PAD.decode(text.trim_end_matches('=')).ok())
                .ok_or_else(|| ConsoleError::Crypto("the console key is not base64 or hex".into()))?,
        );
        if bytes.0.len() != 32 {
            return Err(ConsoleError::Crypto(format!(
                "the console key must be 32 bytes, not {} (e.g. `openssl rand -base64 32`)",
                bytes.0.len()
            )));
        }
        let unbound = UnboundKey::new(&AES_256_GCM, &bytes.0)
            .map_err(|_| ConsoleError::Crypto("the console key is not usable".into()))?;
        let mut tagged = b"graphrt-console-key-fingerprint:".to_vec();
        tagged.extend_from_slice(&bytes.0);
        let fingerprint = sha256_hex(&tagged);
        tagged.fill(0);
        Ok(Self { key: LessSafeKey::new(unbound), fingerprint })
    }

    fn seal(&self, name: &str, value: &str) -> Result<(String, String)> {
        let nonce_bytes = random_bytes::<NONCE_LEN>()?;
        let mut in_out = value.as_bytes().to_vec();
        let aad = format!("{AAD_PREFIX}{name}");
        self.key
            .seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce_bytes), Aad::from(aad.as_bytes()), &mut in_out)
            .map_err(|_| ConsoleError::Crypto("cannot encrypt the secret".into()))?;
        Ok((STANDARD.encode(nonce_bytes), STANDARD.encode(in_out)))
    }

    fn open(&self, name: &str, sealed: &Sealed) -> Result<String> {
        let broken = || ConsoleError::Crypto(format!("secret `{name}` cannot be decrypted with this key"));
        let nonce: [u8; NONCE_LEN] =
            STANDARD.decode(&sealed.nonce).ok().and_then(|bytes| bytes.try_into().ok()).ok_or_else(broken)?;
        let mut in_out = STANDARD.decode(&sealed.ciphertext).map_err(|_| broken())?;
        let aad = format!("{AAD_PREFIX}{name}");
        let plain = self
            .key
            .open_in_place(Nonce::assume_unique_for_key(nonce), Aad::from(aad.as_bytes()), &mut in_out)
            .map_err(|_| broken())?;
        let value = String::from_utf8(plain.to_vec()).map_err(|_| broken());
        in_out.fill(0);
        value
    }
}

/// Wipes a byte buffer when dropped (the key text decoded).
mod zeroize_lite {
    pub(super) struct Zeroizing(pub(super) Vec<u8>);

    impl Drop for Zeroizing {
        fn drop(&mut self) {
            self.0.fill(0);
        }
    }
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() != 64 || !text.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| text.get(i..i + 2).and_then(|pair| u8::from_str_radix(pair, 16).ok())).collect()
}

/// Whether `name` may be a secret: an environment variable name that does
/// not configure the server.
pub fn check_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 128
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit());
    if !valid {
        return Err(ConsoleError::Invalid(format!(
            "`{name}` is not an environment variable name (letters, digits and `_`, not starting with a digit)"
        )));
    }
    if name.to_ascii_uppercase().starts_with("GRAPHRT_") {
        return Err(ConsoleError::Invalid(format!(
            "`{name}`: variables that start with GRAPHRT_ configure the server and are not set from the console"
        )));
    }
    Ok(())
}

/// The secrets, in `secrets.json`.
#[derive(Debug)]
pub struct Secrets {
    file: JsonFile<SecretsFile>,
    key: Option<SecretKey>,
}

impl Secrets {
    /// The secrets kept in `dir`, sealed with `key` (without one they can be
    /// listed and removed, not set or read).
    ///
    /// @throws ConsoleError - `Io` when the file cannot be read, `Crypto` when
    ///   it was sealed with another key.
    pub fn open(dir: &Path, key: Option<SecretKey>) -> Result<Self> {
        let file: JsonFile<SecretsFile> = JsonFile::open(dir.join("secrets.json"))?;
        if let Some(key) = &key {
            let stored = file.read(|f| f.key_fingerprint.clone())?;
            if stored.as_ref().is_some_and(|stored| stored != &key.fingerprint) {
                return Err(ConsoleError::Crypto(
                    "the secrets were encrypted with another console key (check GRAPHRT_CONSOLE_KEY)".into(),
                ));
            }
        }
        Ok(Self { file, key })
    }

    /// Whether secrets can be set (a key is configured).
    pub fn has_key(&self) -> bool {
        self.key.is_some()
    }

    /// Every secret, by name.
    ///
    /// @throws ConsoleError::Io - When the file changed and cannot be read.
    pub fn list(&self) -> Result<Vec<SecretInfo>> {
        self.file.read(|f| {
            f.secrets
                .iter()
                .map(|(name, sealed)| SecretInfo {
                    name: name.clone(),
                    updated_ms: sealed.updated_ms,
                    updated_by: sealed.updated_by.clone(),
                })
                .collect()
        })
    }

    /// Set secret `name` to `value`.
    ///
    /// @param by - Who set it, for the listing.
    /// @returns Whether it existed before.
    /// @throws ConsoleError - `Invalid` name or value, `Conflict` without a key, `Io` on a failed write.
    pub fn set(&self, name: &str, value: &str, by: &str) -> Result<bool> {
        check_name(name)?;
        if value.len() > MAX_VALUE_BYTES {
            return Err(ConsoleError::Invalid(format!("a secret holds at most {MAX_VALUE_BYTES} bytes")));
        }
        let key = self.key.as_ref().ok_or_else(|| {
            ConsoleError::Conflict("no console key is configured: set GRAPHRT_CONSOLE_KEY (32 bytes)".into())
        })?;
        let (nonce, ciphertext) = key.seal(name, value)?;
        self.file.update(|f| {
            if !f.secrets.contains_key(name) && f.secrets.len() >= MAX_SECRETS {
                return Err(ConsoleError::Conflict(format!("the console keeps at most {MAX_SECRETS} secrets")));
            }
            f.key_fingerprint = Some(key.fingerprint.clone());
            let sealed = Sealed { nonce, ciphertext, updated_ms: now_ms(), updated_by: by.to_owned() };
            Ok(f.secrets.insert(name.to_owned(), sealed).is_some())
        })
    }

    /// Remove secret `name`.
    ///
    /// @returns Whether there was one.
    /// @throws ConsoleError::Io - When the file cannot be written.
    pub fn remove(&self, name: &str) -> Result<bool> {
        self.file.update(|f| Ok(f.secrets.remove(name).is_some()))
    }

    /// Every value, decrypted: what the served workflows get at startup.
    ///
    /// @throws ConsoleError::Crypto - When there are secrets but no key, or one cannot be decrypted.
    pub fn values(&self) -> Result<Vec<(String, String)>> {
        let sealed = self.file.read(|f| f.secrets.clone())?;
        if sealed.is_empty() {
            return Ok(Vec::new());
        }
        let key = self.key.as_ref().ok_or_else(|| {
            ConsoleError::Crypto("there are console secrets but no console key (GRAPHRT_CONSOLE_KEY)".into())
        })?;
        sealed.iter().map(|(name, sealed)| Ok((name.clone(), key.open(name, sealed)?))).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

    fn dir(name: &str) -> std::path::PathBuf {
        graphrt_core::private_fs::create_fresh_temp_dir(name).unwrap()
    }

    #[test]
    fn values_are_sealed_at_rest_and_never_listed() {
        let dir = dir("graphrt-console-secrets");
        let secrets = Secrets::open(&dir, Some(SecretKey::parse(KEY).unwrap())).unwrap();
        assert!(!secrets.set("OPENAI_API_KEY", "sk-very-secret", "alice").unwrap());
        assert!(secrets.set("OPENAI_API_KEY", "sk-rotated", "bob").unwrap());
        let text = std::fs::read_to_string(dir.join("secrets.json")).unwrap();
        assert!(!text.contains("sk-"), "{text}");
        let listed = serde_json::to_string(&secrets.list().unwrap()).unwrap();
        assert!(listed.contains("OPENAI_API_KEY") && listed.contains("bob") && !listed.contains("sk-"));
        let reopened = Secrets::open(&dir, Some(SecretKey::parse(KEY).unwrap())).unwrap();
        assert_eq!(reopened.values().unwrap(), [("OPENAI_API_KEY".to_owned(), "sk-rotated".to_owned())]);
        assert!(reopened.remove("OPENAI_API_KEY").unwrap());
        assert!(reopened.values().unwrap().is_empty());
        let _cleanup = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn another_key_is_refused_at_open() {
        let dir = dir("graphrt-console-secrets-key");
        let secrets = Secrets::open(&dir, Some(SecretKey::parse(KEY).unwrap())).unwrap();
        secrets.set("TOKEN", "x", "alice").unwrap();
        let other = SecretKey::parse(&"11".repeat(32)).unwrap();
        assert!(matches!(Secrets::open(&dir, Some(other)), Err(ConsoleError::Crypto(_))));
        let keyless = Secrets::open(&dir, None).unwrap();
        assert_eq!(keyless.list().unwrap().len(), 1);
        assert!(matches!(keyless.values(), Err(ConsoleError::Crypto(_))));
        assert!(matches!(keyless.set("TOKEN", "y", "a"), Err(ConsoleError::Conflict(_))));
        let _cleanup = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_sealed_value_cannot_move_to_another_name() {
        let key = SecretKey::parse(KEY).unwrap();
        let (nonce, ciphertext) = key.seal("A", "value").unwrap();
        let sealed = Sealed { nonce, ciphertext, updated_ms: 0, updated_by: String::new() };
        assert_eq!(key.open("A", &sealed).unwrap(), "value");
        assert!(key.open("B", &sealed).is_err());
    }

    #[test]
    fn keys_and_names_are_checked() {
        assert!(SecretKey::parse("short").is_err());
        assert!(SecretKey::parse(&"ab".repeat(32)).is_ok());
        assert!(SecretKey::parse(KEY.trim_end_matches('=')).is_ok());
        for bad in ["", "1A", "A-B", "GRAPHRT_API_KEY", "graphrt_x"] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
        assert!(check_name("OPENAI_API_KEY").is_ok());
    }
}
