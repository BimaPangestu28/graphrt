//! Ed25519 signatures over the bundle manifest.
//!
//! The manifest records the SHA-256 of every file, so signing the exact
//! `bundle.json` bytes covers the whole bundle.

use std::fmt;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::error::BundleError;
use crate::{hex, unhex};

/// Contents of `bundle.sig`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleSignature {
    /// Always `ed25519`.
    pub algorithm: String,
    /// Hex-encoded 32-byte public key.
    pub public_key: String,
    /// Hex-encoded 64-byte signature over the exact `bundle.json` bytes.
    pub signature: String,
}

/// A generated signing key pair, hex encoded.
#[derive(Clone)]
pub struct KeyPair {
    /// 32-byte secret seed. Keep it out of source control.
    pub secret_key: String,
    pub public_key: String,
}

impl fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyPair").field("secret_key", &"<redacted>").field("public_key", &self.public_key).finish()
    }
}

/// Generate a new Ed25519 key pair from the operating system's random source.
pub fn generate_keypair() -> Result<KeyPair, BundleError> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| BundleError::Signature(format!("no random source: {e}")))?;
    let signing = SigningKey::from_bytes(&seed);
    Ok(KeyPair { secret_key: hex(&seed), public_key: hex(signing.verifying_key().as_bytes()) })
}

/// Parse a hex public key.
pub fn parse_public_key(text: &str) -> Result<VerifyingKey, BundleError> {
    let bytes = unhex(text).ok_or_else(|| BundleError::Signature("public key is not hex".into()))?;
    let array: [u8; 32] = bytes.try_into().map_err(|_| BundleError::Signature("public key must be 32 bytes".into()))?;
    VerifyingKey::from_bytes(&array).map_err(|e| BundleError::Signature(format!("invalid public key: {e}")))
}

pub(crate) fn parse_secret_key(text: &str) -> Result<SigningKey, BundleError> {
    let bytes = unhex(text).ok_or_else(|| BundleError::Signature("secret key is not hex".into()))?;
    let array: [u8; 32] = bytes.try_into().map_err(|_| BundleError::Signature("secret key must be 32 bytes".into()))?;
    Ok(SigningKey::from_bytes(&array))
}

/// Sign manifest bytes.
pub(crate) fn sign_manifest(manifest: &[u8], secret_key: &str) -> Result<BundleSignature, BundleError> {
    let key = parse_secret_key(secret_key)?;
    let signature = key.sign(manifest);
    Ok(BundleSignature {
        algorithm: "ed25519".into(),
        public_key: hex(key.verifying_key().as_bytes()),
        signature: hex(&signature.to_bytes()),
    })
}

/// Verify `sig` over manifest bytes; returns the signer's public key (hex).
pub(crate) fn verify_manifest(manifest: &[u8], sig: &BundleSignature) -> Result<String, BundleError> {
    if sig.algorithm != "ed25519" {
        return Err(BundleError::Signature(format!("unsupported algorithm `{}`", sig.algorithm)));
    }
    let key = parse_public_key(&sig.public_key)?;
    let bytes = unhex(&sig.signature).ok_or_else(|| BundleError::Signature("signature is not hex".into()))?;
    let array: [u8; 64] = bytes.try_into().map_err(|_| BundleError::Signature("signature must be 64 bytes".into()))?;
    // Strict verification also rejects small-order keys and non-canonical signatures.
    key.verify_strict(manifest, &Signature::from_bytes(&array))
        .map_err(|_| BundleError::Signature("signature does not match the manifest".into()))?;
    Ok(hex(key.as_bytes()))
}

/// Return a copy of `bundle` (archive bytes) signed with `secret_key` (hex seed).
/// Any previous signature is replaced. The bundle is fully verified first.
pub fn sign_bundle(bundle: &[u8], secret_key: &str) -> Result<Vec<u8>, BundleError> {
    let opened = crate::Bundle::open(bundle, &crate::OpenOptions::default())?;
    let signature = sign_manifest(opened.manifest_bytes(), secret_key)?;
    let sig_json = serde_json::to_vec_pretty(&signature).map_err(|e| BundleError::Signature(e.to_string()))?;
    let first: [(&str, &[u8]); 2] =
        [(crate::MANIFEST_PATH, opened.manifest_bytes()), (crate::SIGNATURE_PATH, sig_json.as_slice())];
    crate::archive::write_zip(&first, opened.raw_files())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify() {
        let pair = generate_keypair().unwrap();
        let sig = sign_manifest(b"manifest", &pair.secret_key).unwrap();
        assert_eq!(verify_manifest(b"manifest", &sig).unwrap(), pair.public_key);
        assert!(verify_manifest(b"tampered", &sig).is_err());
        assert!(!format!("{pair:?}").contains(&pair.secret_key));
    }
}
