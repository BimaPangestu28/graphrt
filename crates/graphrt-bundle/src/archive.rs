//! Safe zip reading and deterministic zip writing.

use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};

use crate::error::BundleError;

/// Limits applied while reading an archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ArchiveLimits {
    pub max_entries: usize,
    pub max_entry_bytes: u64,
    pub max_total_bytes: u64,
    /// Maximum uncompressed / compressed ratio per entry (zip-bomb guard).
    pub max_ratio: u64,
}

/// Validate an archive path: relative, `/`-separated, no `..`, `.` or empty parts,
/// no backslashes, drive letters or control characters.
pub(crate) fn validate_path(path: &str) -> Result<(), BundleError> {
    let bad = |why: &str| Err(BundleError::Archive(format!("unsafe path `{path}`: {why}")));
    if path.is_empty() || path.len() > 512 {
        return bad("empty or too long");
    }
    if path.starts_with('/') || path.contains('\\') || path.contains(':') {
        return bad("absolute or platform-specific");
    }
    if path.chars().any(char::is_control) {
        return bad("control character");
    }
    for part in path.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return bad("empty, `.` or `..` component");
        }
    }
    Ok(())
}

/// Read every file of a zip archive into memory, enforcing `limits`.
pub(crate) fn read_zip(bytes: &[u8], limits: &ArchiveLimits) -> Result<BTreeMap<String, Vec<u8>>, BundleError> {
    let err = |e: zip::result::ZipError| BundleError::Archive(e.to_string());
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(err)?;
    if archive.len() > limits.max_entries {
        return Err(BundleError::Archive(format!(
            "{} entries exceed the limit of {}",
            archive.len(),
            limits.max_entries
        )));
    }
    let mut files = BTreeMap::new();
    let mut total: u64 = 0;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(err)?;
        let name = entry.name().map_err(err)?.into_owned();
        if entry.is_dir() {
            validate_path(name.trim_end_matches('/'))?;
            continue;
        }
        if entry.is_symlink() {
            return Err(BundleError::Archive(format!("`{name}` is a symlink")));
        }
        validate_path(&name)?;
        if entry.enclosed_name().is_none() {
            return Err(BundleError::Archive(format!("unsafe path `{name}`")));
        }
        let declared = entry.size();
        if declared > limits.max_entry_bytes {
            return Err(BundleError::Archive(format!("`{name}` is larger than {} bytes", limits.max_entry_bytes)));
        }
        let compressed = entry.compressed_size().max(1);
        if declared / compressed > limits.max_ratio {
            return Err(BundleError::Archive(format!("`{name}` has a suspicious compression ratio")));
        }
        // Never trust the declared size: read at most the limit plus one byte.
        let mut data = Vec::with_capacity(usize::try_from(declared.min(1 << 20)).unwrap_or(0));
        (&mut entry)
            .take(limits.max_entry_bytes.saturating_add(1))
            .read_to_end(&mut data)
            .map_err(|e| BundleError::Archive(format!("`{name}`: {e}")))?;
        let len = u64::try_from(data.len()).unwrap_or(u64::MAX);
        if len > limits.max_entry_bytes {
            return Err(BundleError::Archive(format!("`{name}` is larger than {} bytes", limits.max_entry_bytes)));
        }
        total = total.saturating_add(len);
        if total > limits.max_total_bytes {
            return Err(BundleError::Archive(format!(
                "bundle content exceeds the limit of {} bytes",
                limits.max_total_bytes
            )));
        }
        if files.insert(name.clone(), data).is_some() {
            return Err(BundleError::Archive(format!("duplicate entry `{name}`")));
        }
    }
    Ok(files)
}

/// Write files into a deterministic zip: sorted order (manifest first), fixed
/// timestamps and permissions, deflate compression.
pub(crate) fn write_zip(first: &[(&str, &[u8])], files: &BTreeMap<String, Vec<u8>>) -> Result<Vec<u8>, BundleError> {
    let err = |e: zip::result::ZipError| BundleError::Archive(e.to_string());
    let io = |e: std::io::Error| BundleError::Archive(e.to_string());
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(zip::DateTime::default())
        .unix_permissions(0o644);
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (path, data) in first {
        writer.start_file(*path, options).map_err(err)?;
        writer.write_all(data).map_err(io)?;
    }
    for (path, data) in files {
        validate_path(path)?;
        writer.start_file(path.as_str(), options).map_err(err)?;
        writer.write_all(data).map_err(io)?;
    }
    Ok(writer.finish().map_err(err)?.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_validation() {
        for ok in ["bundle.json", "plugins/a/b.wasm", "assets/x y.png"] {
            assert!(validate_path(ok).is_ok(), "{ok}");
        }
        for bad in ["", "/etc/passwd", "../x", "a/../b", "a//b", "a\\b", "C:/x", "./a", "a/\u{0}"] {
            assert!(validate_path(bad).is_err(), "{bad:?}");
        }
    }
}
