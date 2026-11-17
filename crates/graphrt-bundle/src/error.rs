//! Errors.

/// Error building, opening or verifying a bundle.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BundleError {
    /// The archive is not a valid zip or violates a safety limit.
    #[error("invalid bundle archive: {0}")]
    Archive(String),
    /// The manifest is missing or malformed.
    #[error("invalid bundle manifest: {0}")]
    Manifest(String),
    /// A file's content does not match its recorded SHA-256.
    #[error("bundle integrity check failed: {0}")]
    Integrity(String),
    /// Signature missing, malformed, invalid or from an untrusted key.
    #[error("bundle signature: {0}")]
    Signature(String),
    /// A workflow inside the bundle is invalid.
    #[error("bundle workflow `{name}`: {message}")]
    Workflow { name: String, message: String },
    /// A file would leak a secret (inline API key, token, password...).
    #[error("{0}")]
    Secret(String),
    /// Building the bundle failed (bad input file, missing workflow...).
    #[error("bundle build: {0}")]
    Build(String),
    /// A bundled plugin could not be loaded.
    #[error("bundled plugin `{plugin}`: {message}")]
    Plugin { plugin: String, message: String },
    /// Filesystem error.
    #[error("{path}: {message}")]
    Io { path: String, message: String },
}

impl BundleError {
    pub(crate) fn io(path: &std::path::Path, e: &std::io::Error) -> Self {
        Self::Io { path: path.display().to_string(), message: e.to_string() }
    }
}
