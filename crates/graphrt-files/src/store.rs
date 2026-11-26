//! The storage contract behind a workspace.
//!
//! [`FileStore`] is the only thing that touches storage. The crate ships
//! [`crate::DirStore`] (one directory on disk) and [`crate::MemoryStore`];
//! hosts add object storage or a database by implementing the trait.
//! Limits, quotas and type checks live one level up in [`crate::Workspace`],
//! so every backend gets them.

use std::fmt;

use futures::future::BoxFuture;

use crate::path::WorkspacePath;

/// How many bytes [`FileStore::sha256`] reads per step by default.
const HASH_CHUNK_BYTES: u64 = 1024 * 1024;

/// Storage failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    #[error("`{0}` does not exist")]
    NotFound(String),
    #[error("`{0}` already exists")]
    AlreadyExists(String),
    #[error("`{0}` is not a regular file")]
    NotAFile(String),
    #[error("access to `{path}` denied: {reason}")]
    Denied { path: String, reason: String },
    #[error("storage error on `{path}`: {message}")]
    Io { path: String, message: String },
}

impl StoreError {
    pub(crate) fn denied(path: &WorkspacePath, reason: &str) -> Self {
        StoreError::Denied { path: path.to_string(), reason: reason.to_owned() }
    }

    pub(crate) fn io(path: &str, error: impl fmt::Display) -> Self {
        StoreError::Io { path: path.to_owned(), message: error.to_string() }
    }
}

/// One stored file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: WorkspacePath,
    /// Size in bytes.
    pub size: u64,
    /// Last modification, in milliseconds since the Unix epoch, when the store knows it.
    pub modified_ms: Option<u64>,
}

/// Files below a directory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    /// Files in path order.
    pub entries: Vec<Entry>,
    /// Whether more files exist than `max_entries` allowed.
    pub truncated: bool,
}

/// A place files are kept.
///
/// Implementations must never resolve a [`WorkspacePath`] to anything outside
/// their own root, and must not follow links that lead out of it.
pub trait FileStore: Send + Sync + fmt::Debug {
    /// The file at `path`, or `None` when there is none.
    fn stat<'a>(&'a self, path: &'a WorkspacePath) -> BoxFuture<'a, Result<Option<Entry>, StoreError>>;

    /// Up to `max_bytes` bytes of the file, starting at byte `offset`.
    /// Reading past the end returns what is there (possibly nothing).
    fn read<'a>(
        &'a self,
        path: &'a WorkspacePath,
        offset: u64,
        max_bytes: u64,
    ) -> BoxFuture<'a, Result<Vec<u8>, StoreError>>;

    /// Store `bytes` at `path`, creating directories as needed. The file is
    /// either fully written or not there at all. Without `overwrite`, an
    /// existing file fails with [`StoreError::AlreadyExists`].
    fn write<'a>(
        &'a self,
        path: &'a WorkspacePath,
        bytes: Vec<u8>,
        overwrite: bool,
    ) -> BoxFuture<'a, Result<Entry, StoreError>>;

    /// Remove the file at `path`.
    fn delete<'a>(&'a self, path: &'a WorkspacePath) -> BoxFuture<'a, Result<(), StoreError>>;

    /// Files below `dir` (the whole store when `None`), at most `max_entries`.
    fn list<'a>(
        &'a self,
        dir: Option<&'a WorkspacePath>,
        max_entries: usize,
    ) -> BoxFuture<'a, Result<Listing, StoreError>>;

    /// SHA-256 of the file. The default reads it in steps of 1 MiB, so a large
    /// file is never held in memory at once.
    fn sha256<'a>(&'a self, path: &'a WorkspacePath) -> BoxFuture<'a, Result<[u8; 32], StoreError>> {
        Box::pin(async move {
            use sha2::Digest;
            let mut hasher = sha2::Sha256::new();
            let mut offset = 0u64;
            loop {
                let chunk = self.read(path, offset, HASH_CHUNK_BYTES).await?;
                if chunk.is_empty() {
                    break;
                }
                offset = offset.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
                hasher = crate::blocking(move || {
                    hasher.update(&chunk);
                    hasher
                })
                .await
                .map_err(|message| StoreError::io(path.as_str(), message))?;
            }
            Ok(hasher.finalize().into())
        })
    }
}

/// Lowercase hex of a digest.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS.get(usize::from(byte >> 4)).copied().unwrap_or(b'0')));
        out.push(char::from(DIGITS.get(usize::from(byte & 0x0f)).copied().unwrap_or(b'0')));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_is_lowercase_and_padded() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
        assert_eq!(hex(&[]), "");
    }
}
