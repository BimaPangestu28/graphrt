//! Small helpers of the server: randomness, hashes, time and files only the
//! server's user can read.

use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::{SecureRandom as _, SystemRandom};

use super::error::{ServerError, ServerResult};

pub(crate) const MS_PER_MINUTE: u64 = 60 * 1000;
pub(crate) const MS_PER_DAY: u64 = 24 * 60 * MS_PER_MINUTE;

/// Current Unix time in milliseconds.
pub(crate) fn now_ms() -> u64 {
    graphrt_core::store::now_ms()
}

/// `N` random bytes from the operating system.
///
/// @throws ServerError::Internal - When the system has no randomness to give.
pub(crate) fn random_bytes<const N: usize>() -> ServerResult<[u8; N]> {
    let mut bytes = [0u8; N];
    SystemRandom::new().fill(&mut bytes).map_err(|_| ServerError::Internal("no system randomness".into()))?;
    Ok(bytes)
}

/// 256 random bits as base64url: a secret token.
pub(crate) fn random_token() -> ServerResult<String> {
    Ok(b64url(&random_bytes::<32>()?))
}

/// A random id with a prefix, e.g. `ws_` + 96 bits as base64url.
pub(crate) fn random_id(prefix: &str) -> ServerResult<String> {
    // base64url may start with `-` or `_`: hex keeps ids plain in paths and logs.
    Ok(format!("{prefix}{}", hex(&random_bytes::<12>()?)))
}

pub(crate) fn b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub(crate) fn b64url_decode(text: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(text).ok()
}

/// Hex SHA-256 of `bytes`: how tokens and codes are kept at rest.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _infallible = write!(out, "{byte:02x}");
    }
    out
}

/// Compare without early exit, so timing does not reveal a prefix.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= usize::from(x ^ y);
    }
    diff == 0
}

/// Whether `id` looks like an id this server made (`prefix` + 24 hex digits),
/// checked before it becomes part of a path.
pub(crate) fn is_id(id: &str, prefix: &str) -> bool {
    id.strip_prefix(prefix).is_some_and(|rest| rest.len() == 24 && rest.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Write `bytes` to `path` through a temporary file and a rename, owner only.
///
/// @throws ServerError::Io - When the file cannot be written.
pub(crate) fn replace_file(path: &Path, bytes: &[u8]) -> ServerResult<()> {
    use std::io::Write as _;
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut file = graphrt_core::private_fs::create_file(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if let Err(error) = result {
        let _cleanup = std::fs::remove_file(&tmp);
        return Err(ServerError::Io(format!("cannot write {}: {error}", path.display())));
    }
    Ok(())
}

/// Create `dir` (owner only) when it is missing.
///
/// @throws ServerError::Io - When it cannot be created.
pub(crate) fn ensure_dir(dir: &Path) -> ServerResult<()> {
    graphrt_core::private_fs::create_dir_all(dir)
        .map_err(|error| ServerError::Io(format!("cannot create {}: {error}", dir.display())))
}

/// Read a JSON file, `None` when it does not exist.
///
/// @throws ServerError::Io - When it exists but cannot be read or parsed.
pub(crate) fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> ServerResult<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| ServerError::Io(format!("{} is not valid: {error}", path.display()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ServerError::Io(format!("cannot read {}: {error}", path.display()))),
    }
}

/// Write `value` as pretty JSON through [`replace_file`].
///
/// @throws ServerError::Io - When it cannot be written.
pub(crate) fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> ServerResult<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| ServerError::Internal(error.to_string()))?;
    replace_file(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_tokens() {
        let id = random_id("ws_").unwrap();
        assert!(is_id(&id, "ws_"));
        assert!(!is_id(&id, "pr_"));
        assert!(!is_id("ws_../../etc", "ws_"));
        assert_ne!(random_token().unwrap(), random_token().unwrap());
        assert_eq!(random_token().unwrap().len(), 43);
    }

    #[test]
    fn hashes_and_comparisons() {
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"diff"));
        assert!(!constant_time_eq(b"short", b"longer"));
        assert_eq!(b64url_decode(&b64url(b"\x00\xff")).unwrap(), b"\x00\xff");
    }
}
