//! # graphrt-bundle
//!
//! A bundle (`.grt`) is one portable file holding everything a workflow needs to
//! run on any graphrt host — the CLI, `graphrt serve`, or a phone through
//! `graphrt-ffi`: the workflow document(s), WebAssembly plugins, knowledge-base
//! configuration and data, a providers file without secrets, and assets.
//!
//! A bundle is a zip archive with a `bundle.json` manifest listing the SHA-256 of
//! every file. Hosts check every hash when opening a bundle, and can require an
//! Ed25519 signature (`bundle.sig`) from a trusted key, so a bundle exported by a
//! builder platform is verifiable wherever it lands.
//!
//! ```no_run
//! use graphrt_bundle::{Bundle, OpenOptions};
//!
//! let bytes = std::fs::read("app.grt")?;
//! let bundle = Bundle::open(&bytes, &OpenOptions::default())?;
//! let workflow = bundle.workflow(None)?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! Secrets never travel in a bundle: providers files with inline keys are
//! rejected, and environment variables are only *declared* (names), never
//! stored.

mod archive;
mod builder;
mod bundle;
mod error;
mod manifest;
mod secrets;
mod sign;
#[cfg(feature = "wasm")]
mod wasm;

pub use builder::BundleBuilder;
pub use bundle::{Bundle, BundledPlugin, CompatReport, DEFAULT_MAX_KNOWLEDGE_BYTES, OpenOptions};
pub use error::BundleError;
#[cfg(feature = "wasm")]
pub use graphrt_wasm::WasmLimits;
pub use manifest::{BUNDLE_FORMAT_VERSION, BundleManifest, MANIFEST_PATH, PluginRef, Requires, SIGNATURE_PATH};
pub use secrets::check_no_inline_secrets;
pub use sign::{BundleSignature, KeyPair, generate_keypair, parse_public_key, sign_bundle};

/// Hex-encoded SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(bytes);
    hex(&digest)
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(DIGITS.get(usize::from(b >> 4)).copied().unwrap_or(b'0')));
        out.push(char::from(DIGITS.get(usize::from(b & 0x0f)).copied().unwrap_or(b'0')));
    }
    out
}

pub(crate) fn unhex(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    for pair in bytes.chunks(2) {
        let s = std::str::from_utf8(pair).ok()?;
        out.push(u8::from_str_radix(s, 16).ok()?);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let bytes = [0u8, 1, 0xab, 0xff];
        assert_eq!(hex(&bytes), "0001abff");
        assert_eq!(unhex("0001abff").unwrap(), bytes);
        assert!(unhex("abc").is_none());
        assert!(unhex("zz").is_none());
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
