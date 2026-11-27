//! Minisign signatures of a plugin catalog index.
//!
//! The graphrt-plugins release workflow signs `index.json` and `index-v2.json`
//! with minisign and publishes a detached `<index>.minisig` next to each. A
//! reader fetches the index bytes and the signature, calls
//! [`CatalogKeys::verify`] and parses the index only when that returns `Ok`.
//! The sha256 values in the verified index then cover the plugin tarballs.
//!
//! - [`CatalogKeys::built_in`] holds the keys of the graphrt-plugins catalog
//!   ([`TRUSTED_CATALOG_PUBLIC_KEYS`]). It is a list, so a key rotation can
//!   ship the old and the new key together.
//! - [`CatalogKeys::from_public_keys`] is for a catalog published by somebody
//!   else, with keys the user named.
//! - Only prehashed (`ED`) signatures are accepted, as made by current
//!   minisign. Legacy (`Ed`) signatures are refused.
//!
//! This module does no I/O: hosts fetch the bytes their own way (the `graphrt`
//! CLI and desktop apps have different download and caching rules) and share
//! the keys and the check.

use minisign_verify::{Error as MinisignError, PublicKey, Signature};

/// Base64 minisign public keys whose signatures make the graphrt-plugins
/// catalog trusted (the second line of `catalog-signing.pub` in that
/// repository). Key ids are in the comments, as `minisign -V` prints them.
pub const TRUSTED_CATALOG_PUBLIC_KEYS: &[&str] = &[
    // CB1AEE339F05C2BD: graphrt-plugins catalog key (2026).
    "RWS9wgWfM+4ayykVUw9LH3DOjwa+g/JFaREz3FWd/U8GJaa6q4WrNd9c",
];

/// Suffix of a signature file next to the file it signs.
pub const SIGNATURE_SUFFIX: &str = ".minisig";

/// Largest `.minisig` file a reader should accept (a signature is about 300 bytes).
pub const MAX_SIGNATURE_BYTES: u64 = 4 * 1024;

/// The URL (or path text) of the signature next to `signed`.
pub fn signature_location(signed: &str) -> String {
    format!("{signed}{SIGNATURE_SUFFIX}")
}

/// Why a catalog index is not trusted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CatalogSignatureError {
    /// A public key is not a minisign public key.
    #[error("`{key}` is not a minisign public key ({reason})")]
    InvalidPublicKey { key: String, reason: String },
    /// No public key was given, so nothing could be trusted.
    #[error("no catalog public key to check the signature against")]
    NoPublicKeys,
    /// The signature file is not a minisign signature.
    #[error("the plugin catalog signature is not valid: the signature cannot be read ({0})")]
    UnreadableSignature(String),
    /// The signature was made with a key that is not trusted.
    #[error("the plugin catalog signature is not valid: it was not made with a trusted catalog key")]
    UntrustedKey,
    /// A trusted key id matches but the signature does not cover these bytes:
    /// the index changed after it was signed, or the signature is forged.
    #[error("the plugin catalog signature is not valid: the signature does not match the index ({0})")]
    Mismatch(String),
}

/// What a valid signature proves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedCatalog {
    /// The base64 public key that made the signature.
    pub public_key: String,
    /// The signed comment of the signature. The graphrt-plugins catalog writes
    /// `graphrt-plugins <index name> plugins=<count> generated_at=<time>`.
    pub trusted_comment: String,
}

/// The public keys a catalog index may be signed with.
#[derive(Clone)]
pub struct CatalogKeys {
    keys: Vec<(String, PublicKey)>,
}

impl std::fmt::Debug for CatalogKeys {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_list().entries(self.keys.iter().map(|(text, _)| text)).finish()
    }
}

impl CatalogKeys {
    /// The keys of the graphrt-plugins catalog ([`TRUSTED_CATALOG_PUBLIC_KEYS`]).
    ///
    /// # Errors
    /// Only if a built-in key does not decode, which a unit test rules out.
    pub fn built_in() -> Result<Self, CatalogSignatureError> {
        Self::from_public_keys(TRUSTED_CATALOG_PUBLIC_KEYS)
    }

    /// Keys from minisign public keys: each the base64 key alone, or the text
    /// of a minisign `.pub` file (an `untrusted comment:` line, then the key).
    ///
    /// # Errors
    /// [`CatalogSignatureError::InvalidPublicKey`] for the first key that does
    /// not decode (a bad key is never skipped), and
    /// [`CatalogSignatureError::NoPublicKeys`] for an empty list.
    pub fn from_public_keys<S: AsRef<str>>(public_keys: &[S]) -> Result<Self, CatalogSignatureError> {
        if public_keys.is_empty() {
            return Err(CatalogSignatureError::NoPublicKeys);
        }
        let keys = public_keys.iter().map(|text| parse_public_key(text.as_ref())).collect::<Result<_, _>>()?;
        Ok(Self { keys })
    }

    /// The base64 public keys, in the order given.
    pub fn public_keys(&self) -> impl Iterator<Item = &str> {
        self.keys.iter().map(|(text, _)| text.as_str())
    }

    /// Check that `signature_text` (the content of a `.minisig` file) signs
    /// exactly `index_bytes` with one of the keys. Call this before parsing
    /// the index.
    ///
    /// # Errors
    /// [`CatalogSignatureError`] naming why: an unreadable signature, a key
    /// that is not trusted, or a signature that does not match the bytes.
    pub fn verify(&self, index_bytes: &[u8], signature_text: &str) -> Result<VerifiedCatalog, CatalogSignatureError> {
        let signature = Signature::decode(signature_text)
            .map_err(|error| CatalogSignatureError::UnreadableSignature(error.to_string()))?;
        let mut failure = CatalogSignatureError::UntrustedKey;
        for (text, key) in &self.keys {
            match key.verify(index_bytes, &signature, false) {
                Ok(()) => {
                    return Ok(VerifiedCatalog {
                        public_key: text.clone(),
                        trusted_comment: signature.trusted_comment().to_owned(),
                    });
                }
                Err(MinisignError::UnexpectedKeyId) => {}
                Err(error) => failure = CatalogSignatureError::Mismatch(error.to_string()),
            }
        }
        Err(failure)
    }
}

/// One public key: the base64 text alone or a `.pub` file's two lines.
fn parse_public_key(text: &str) -> Result<(String, PublicKey), CatalogSignatureError> {
    let invalid = |reason: String| CatalogSignatureError::InvalidPublicKey { key: shown_key(text), reason };
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let encoded = match (lines.next(), lines.next(), lines.next()) {
        (Some(key), None, _) => key,
        (Some(comment), Some(key), None) if comment.starts_with("untrusted comment:") => key,
        _ => return Err(invalid("expected the base64 key or the two lines of a minisign .pub file".to_owned())),
    };
    let key = PublicKey::from_base64(encoded).map_err(|error| invalid(error.to_string()))?;
    Ok((encoded.to_owned(), key))
}

/// The key text as it may be quoted in an error: one line, printable, short.
fn shown_key(text: &str) -> String {
    const MAX_CHARS: usize = 80;
    let line = text.lines().map(str::trim).rfind(|line| !line.is_empty()).unwrap_or_default();
    let mut shown: String = line.chars().take(MAX_CHARS).map(|c| if c.is_control() { '?' } else { c }).collect();
    if line.chars().count() > MAX_CHARS {
        shown.push_str("...");
    }
    shown
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use std::io::Cursor;

    use super::*;

    const INDEX: &[u8] = br#"{"schema":1,"generated_at":"2026-11-27T00:00:00Z","plugins":[]}"#;

    fn sign(key_pair: &minisign::KeyPair, bytes: &[u8]) -> String {
        minisign::sign(Some(&key_pair.pk), &key_pair.sk, Cursor::new(bytes), Some("test index.json plugins=0"), None)
            .unwrap()
            .into_string()
    }

    fn key_pair() -> minisign::KeyPair {
        minisign::KeyPair::generate_unencrypted_keypair().unwrap()
    }

    fn keys_of(key_pair: &minisign::KeyPair) -> CatalogKeys {
        CatalogKeys::from_public_keys(&[key_pair.pk.to_base64()]).unwrap()
    }

    #[test]
    fn the_built_in_keys_decode() {
        let keys = CatalogKeys::built_in().unwrap();
        assert_eq!(keys.public_keys().collect::<Vec<_>>(), TRUSTED_CATALOG_PUBLIC_KEYS);
    }

    #[test]
    fn a_valid_signature_is_accepted_with_its_trusted_comment() {
        let pair = key_pair();
        let verified = keys_of(&pair).verify(INDEX, &sign(&pair, INDEX)).unwrap();
        assert_eq!(verified.public_key, pair.pk.to_base64());
        assert_eq!(verified.trusted_comment, "test index.json plugins=0");
    }

    #[test]
    fn any_of_several_trusted_keys_may_sign() {
        let (old, new) = (key_pair(), key_pair());
        let keys = CatalogKeys::from_public_keys(&[old.pk.to_base64(), new.pk.to_base64()]).unwrap();
        assert_eq!(keys.verify(INDEX, &sign(&new, INDEX)).unwrap().public_key, new.pk.to_base64());
        assert_eq!(keys.verify(INDEX, &sign(&old, INDEX)).unwrap().public_key, old.pk.to_base64());
    }

    #[test]
    fn a_signature_from_another_key_is_refused() {
        let (trusted, other) = (key_pair(), key_pair());
        let error = keys_of(&trusted).verify(INDEX, &sign(&other, INDEX)).unwrap_err();
        assert_eq!(error, CatalogSignatureError::UntrustedKey);
        let error = CatalogKeys::built_in().unwrap().verify(INDEX, &sign(&other, INDEX)).unwrap_err();
        assert_eq!(error, CatalogSignatureError::UntrustedKey);
    }

    /// The attacker copies the trusted key id into a signature made with their
    /// own key. Built the other way round here: the trusted key gets the key id
    /// of the attacker's key (bytes 2..10 of a minisign public key).
    #[test]
    fn a_forged_key_id_is_refused() {
        let (trusted, forger) = (key_pair(), key_pair());
        let mut trusted_bytes = trusted.pk.to_bytes();
        trusted_bytes[2..10].copy_from_slice(&forger.pk.to_bytes()[2..10]);
        let trusted_key = minisign::PublicKey::from_bytes(&trusted_bytes).unwrap().to_base64();
        let keys = CatalogKeys::from_public_keys(&[trusted_key]).unwrap();
        let error = keys.verify(INDEX, &sign(&forger, INDEX)).unwrap_err();
        assert!(matches!(error, CatalogSignatureError::Mismatch(_)), "{error}");
    }

    #[test]
    fn an_index_changed_after_signing_is_refused() {
        let pair = key_pair();
        let signature = sign(&pair, INDEX);
        let mut tampered = INDEX.to_vec();
        tampered[10] ^= 1;
        let error = keys_of(&pair).verify(&tampered, &signature).unwrap_err();
        assert!(matches!(error, CatalogSignatureError::Mismatch(_)), "{error}");
        let mut longer = INDEX.to_vec();
        longer.push(b' ');
        assert!(keys_of(&pair).verify(&longer, &signature).is_err());
    }

    /// The trusted comment is signed too: a reader may show it as verified.
    #[test]
    fn a_changed_trusted_comment_is_refused() {
        let pair = key_pair();
        let signature = sign(&pair, INDEX).replace("plugins=0", "plugins=9");
        let error = keys_of(&pair).verify(INDEX, &signature).unwrap_err();
        assert!(matches!(error, CatalogSignatureError::Mismatch(_)), "{error}");
    }

    #[test]
    fn text_that_is_not_a_signature_is_refused() {
        let keys = CatalogKeys::built_in().unwrap();
        for garbage in
            ["", "not a signature", "<html>404</html>\n\n\n\n", "untrusted comment: x\nAAAA\ntrusted comment: y\nAAAA"]
        {
            let error = keys.verify(INDEX, garbage).unwrap_err();
            assert!(matches!(error, CatalogSignatureError::UnreadableSignature(_)), "{garbage:?}: {error}");
        }
    }

    #[test]
    fn public_keys_are_the_base64_text_or_a_pub_file() {
        let pair = key_pair();
        let encoded = pair.pk.to_base64();
        let pub_file = format!("untrusted comment: minisign public key ABC\n{encoded}\n");
        for text in [encoded.clone(), format!("  {encoded}\n"), pub_file] {
            let keys = CatalogKeys::from_public_keys(&[text]).unwrap();
            assert_eq!(keys.public_keys().collect::<Vec<_>>(), [encoded.as_str()]);
            assert!(keys.verify(INDEX, &sign(&pair, INDEX)).is_ok());
        }
    }

    #[test]
    fn bad_public_keys_are_errors_never_skipped() {
        let good = key_pair().pk.to_base64();
        let no_keys: &[&str] = &[];
        assert_eq!(CatalogKeys::from_public_keys(no_keys).unwrap_err(), CatalogSignatureError::NoPublicKeys);
        for bad in ["", "not base64 !", "AAAA", "comment\nline two\nline three", "wrong header\nAAAA"] {
            let error = CatalogKeys::from_public_keys(&[good.as_str(), bad]).unwrap_err();
            assert!(matches!(error, CatalogSignatureError::InvalidPublicKey { .. }), "{bad:?}: {error}");
        }
        let hostile = CatalogKeys::from_public_keys(&["\u{1b}]52;c;x\u{7}"]).unwrap_err().to_string();
        assert!(!hostile.chars().any(char::is_control), "{hostile:?}");
    }

    #[test]
    fn the_signature_sits_next_to_the_index() {
        assert_eq!(signature_location("https://x.example/index.json"), "https://x.example/index.json.minisig");
    }
}
