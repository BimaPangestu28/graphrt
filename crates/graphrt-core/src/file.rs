//! File access contract.
//!
//! The runtime never touches storage itself. Nodes that need file bytes (document
//! extraction, uploads, vision) ask the [`FileResolver`] registered in
//! [`crate::node::Services`] as `Arc<dyn FileResolver>`. Hosts plug in their own
//! storage (S3, blob store, database); this module provides an in-process
//! resolver for inline data URLs and a [`ChainResolver`] to combine several.

use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;

use crate::value::{File, FileTransfer};

/// Error reading or storing a file.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FileError {
    #[error("no resolver can read file `{0}`")]
    Unsupported(String),
    #[error("file `{name}` exceeds the {limit}-byte limit")]
    TooLarge { name: String, limit: u64 },
    #[error("file `{name}` not found")]
    NotFound { name: String },
    #[error("access to file `{name}` denied: {reason}")]
    Denied { name: String, reason: String },
    #[error("reading file `{name}` failed: {message}")]
    Io { name: String, message: String },
    #[error("invalid file `{name}`: {message}")]
    Invalid { name: String, message: String },
}

impl FileError {
    /// Whether retrying might succeed (transient I/O).
    pub fn is_retryable(&self) -> bool {
        matches!(self, FileError::Io { .. })
    }
}

/// File content with its detected media type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileBytes {
    pub bytes: Vec<u8>,
    /// Media type if known (from the source or the file metadata).
    pub mime_type: Option<String>,
}

/// Reads (and optionally stores) file content.
pub trait FileResolver: Send + Sync + fmt::Debug {
    /// Read the whole file, failing with [`FileError::TooLarge`] past `max_bytes`.
    /// Return [`FileError::Unsupported`] for files this resolver does not handle,
    /// so a [`ChainResolver`] can try the next one.
    fn read<'a>(&'a self, file: &'a File, max_bytes: u64) -> BoxFuture<'a, Result<FileBytes, FileError>>;

    /// Persist bytes produced by a node and return a [`File`] referencing them.
    /// Default: unsupported.
    fn store<'a>(
        &'a self,
        name: &'a str,
        mime_type: Option<&'a str>,
        bytes: Vec<u8>,
    ) -> BoxFuture<'a, Result<File, FileError>> {
        let _ = (mime_type, bytes);
        Box::pin(futures::future::ready(Err(FileError::Unsupported(name.to_owned()))))
    }
}

/// Turns an image file into a URL a model request can carry.
///
/// Nodes with `vision` ask the preparer registered in [`crate::node::Services`]
/// as `Arc<dyn ImagePreparer>` for every image they send to a model. Without
/// one, they send the file's own URL untouched. A preparer is where a host
/// checks what an image really is, bounds its size and pixel count, removes
/// metadata and downscales it (the `graphrt-image` crate provides one).
pub trait ImagePreparer: Send + Sync + fmt::Debug {
    /// A URL for `file`: `https://...` or `data:<media type>;base64,...`.
    ///
    /// [`FileError::Unsupported`] means "not mine": the node falls back to
    /// the file's own URL. Every other error fails the node, so an image that
    /// was refused is never sent anyway.
    fn prepare<'a>(&'a self, file: &'a File) -> BoxFuture<'a, Result<String, FileError>>;
}

/// Resolves `inline` files and any file whose `url` is a `data:` URL.
#[derive(Clone, Copy, Debug, Default)]
pub struct DataUrlResolver;

impl FileResolver for DataUrlResolver {
    fn read<'a>(&'a self, file: &'a File, max_bytes: u64) -> BoxFuture<'a, Result<FileBytes, FileError>> {
        let result = match file.url.as_deref() {
            Some(url) if url.starts_with("data:") => decode_data_url(&file.name, url, max_bytes),
            _ if file.transfer == FileTransfer::Inline => {
                Err(FileError::Invalid { name: file.name.clone(), message: "inline file without a data URL".into() })
            }
            _ => Err(FileError::Unsupported(file.name.clone())),
        };
        let result = result.map(|mut fb| {
            if fb.mime_type.is_none() {
                fb.mime_type.clone_from(&file.mime_type);
            }
            fb
        });
        Box::pin(futures::future::ready(result))
    }
}

/// Tries resolvers in order; the first one not returning `Unsupported` wins.
#[derive(Clone, Debug, Default)]
pub struct ChainResolver {
    resolvers: Vec<Arc<dyn FileResolver>>,
}

impl ChainResolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Chain starting with the [`DataUrlResolver`].
    pub fn with_data_urls() -> Self {
        Self::new().then(Arc::new(DataUrlResolver))
    }

    pub fn then(mut self, resolver: Arc<dyn FileResolver>) -> Self {
        self.resolvers.push(resolver);
        self
    }
}

impl FileResolver for ChainResolver {
    fn read<'a>(&'a self, file: &'a File, max_bytes: u64) -> BoxFuture<'a, Result<FileBytes, FileError>> {
        Box::pin(async move {
            for resolver in &self.resolvers {
                match resolver.read(file, max_bytes).await {
                    Err(FileError::Unsupported(_)) => continue,
                    other => return other,
                }
            }
            Err(FileError::Unsupported(file.name.clone()))
        })
    }

    fn store<'a>(
        &'a self,
        name: &'a str,
        mime_type: Option<&'a str>,
        bytes: Vec<u8>,
    ) -> BoxFuture<'a, Result<File, FileError>> {
        Box::pin(async move {
            // Only the first resolver that supports storage is used; bytes cannot be
            // cloned cheaply, so resolvers are probed in order with the same buffer.
            let mut bytes = Some(bytes);
            for resolver in &self.resolvers {
                let Some(b) = bytes.take() else { break };
                match resolver.store(name, mime_type, b.clone()).await {
                    Err(FileError::Unsupported(_)) => bytes = Some(b),
                    other => return other,
                }
            }
            Err(FileError::Unsupported(name.to_owned()))
        })
    }
}

/// Decode a `data:[<mime>][;base64],<payload>` URL.
pub fn decode_data_url(name: &str, url: &str, max_bytes: u64) -> Result<FileBytes, FileError> {
    let invalid = |m: &str| FileError::Invalid { name: name.to_owned(), message: m.to_owned() };
    let rest = url.strip_prefix("data:").ok_or_else(|| invalid("not a data URL"))?;
    let (meta, payload) = rest.split_once(',').ok_or_else(|| invalid("data URL without ','"))?;
    // The `;base64` flag is case-insensitive (RFC 2397 media-type parameters).
    let is_base64 = meta.len() >= 7 && meta.get(meta.len() - 7..).is_some_and(|s| s.eq_ignore_ascii_case(";base64"));
    let meta = if is_base64 { meta.get(..meta.len() - 7).unwrap_or_default() } else { meta };
    let mime = meta.split(';').next().filter(|m| !m.is_empty()).map(str::to_owned);
    let too_large = || FileError::TooLarge { name: name.to_owned(), limit: max_bytes };
    let bytes = if is_base64 {
        // Reject before decoding: base64 yields 3 bytes per 4 characters.
        let estimate = u64::try_from(payload.len() / 4 * 3).unwrap_or(u64::MAX);
        if estimate > max_bytes.saturating_add(3) {
            return Err(too_large());
        }
        base64_decode(payload).ok_or_else(|| invalid("invalid base64 payload"))?
    } else {
        percent_decode(payload)
    };
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
        return Err(FileError::TooLarge { name: name.to_owned(), limit: max_bytes });
    }
    Ok(FileBytes { bytes, mime_type: mime })
}

/// Standard and URL-safe base64 decoder (padding optional, whitespace ignored).
pub fn base64_decode(input: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    let mut padding = false;
    for c in input.bytes() {
        if c.is_ascii_whitespace() {
            continue;
        }
        if c == b'=' {
            padding = true;
            continue;
        }
        // Data after padding means a corrupted or concatenated payload.
        if padding {
            return None;
        }
        buf = (buf << 6) | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buf >> bits) & 0xff).ok()?);
        }
    }
    // A single leftover character cannot encode a byte.
    if bits >= 6 {
        return None;
    }
    Some(out)
}

/// Standard base64 encoder with padding.
pub fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk.first().copied().unwrap_or(0),
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                let idx = usize::try_from((n >> (18 - 6 * i)) & 63).unwrap_or(0);
                out.push(char::from(TABLE.get(idx).copied().unwrap_or(b'A')));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn percent_decode(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            let hex = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok());
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip() {
        for s in ["", "a", "ab", "abc", "hello world", "\u{0}\u{1}\u{ff}"] {
            let enc = base64_encode(s.as_bytes());
            assert_eq!(base64_decode(&enc).unwrap(), s.as_bytes(), "{s:?}");
        }
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
    }

    #[test]
    fn data_url_decoding() {
        let fb = decode_data_url("f", "data:text/plain;base64,aGVsbG8=", 100).unwrap();
        assert_eq!(fb.bytes, b"hello");
        assert_eq!(fb.mime_type.as_deref(), Some("text/plain"));
        let fb = decode_data_url("f", "data:,a%20b", 100).unwrap();
        assert_eq!(fb.bytes, b"a b");
        // `;BASE64` is the same flag.
        let fb = decode_data_url("f", "data:text/plain;BASE64,aGVsbG8=", 100).unwrap();
        assert_eq!(fb.bytes, b"hello");
        // Oversized payloads are rejected before decoding.
        let big = format!("data:;base64,{}", "A".repeat(4000));
        assert!(matches!(decode_data_url("f", &big, 100), Err(FileError::TooLarge { .. })));
    }

    #[test]
    fn base64_rejects_trailing_garbage() {
        assert_eq!(base64_decode("aGk="), Some(b"hi".to_vec()));
        assert_eq!(base64_decode("aGk=aGk="), None);
        assert_eq!(base64_decode("aGVsbG8"), Some(b"hello".to_vec()));
        assert_eq!(base64_decode("a"), None);
        assert!(matches!(decode_data_url("f", "data:,abc", 2), Err(FileError::TooLarge { .. })));
    }

    #[test]
    fn chain_falls_through_unsupported() {
        let chain = ChainResolver::with_data_urls();
        let remote = File { name: "x".into(), url: Some("https://e.com/x".into()), ..File::default() };
        let r = futures::executor::block_on(chain.read(&remote, 10));
        assert!(matches!(r, Err(FileError::Unsupported(_))));
        let inline = File { name: "y".into(), url: Some("data:,hi".into()), ..File::default() };
        let r = futures::executor::block_on(chain.read(&inline, 10)).unwrap();
        assert_eq!(r.bytes, b"hi");
    }
}
