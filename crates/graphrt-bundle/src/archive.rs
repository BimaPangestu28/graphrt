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
    /// Limit of one entry under `knowledge/` and of all of them together
    /// (passage stores grow with the documents; counted apart from the rest).
    pub max_knowledge_bytes: u64,
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

/// Windows device names: a file named after one (with any extension) opens the
/// device instead of creating a file.
const WINDOWS_DEVICE_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2",
    "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Refuse paths that do not extract to distinct files on every platform:
/// Windows device names (`CON`, `nul.txt`, `com1.json`...) in any component,
/// and paths that differ only in case (`Plugin.wasm` and `plugin.wasm`), which
/// overwrite each other on case-insensitive file systems (Windows, macOS by
/// default) so that a verified file could be replaced by another.
///
/// @throws BundleError::Archive - Naming the first offending path.
pub(crate) fn check_extractable<'a>(paths: impl IntoIterator<Item = &'a str>) -> Result<(), BundleError> {
    let mut seen = std::collections::HashSet::new();
    for path in paths {
        if let Some(part) = path.split('/').find(|part| is_windows_device_name(part)) {
            return Err(BundleError::Archive(format!("unsafe path `{path}`: `{part}` is a Windows device name")));
        }
        if !seen.insert(path.to_lowercase()) {
            return Err(BundleError::Archive(format!(
                "unsafe path `{path}`: another file has the same name in a different case"
            )));
        }
    }
    Ok(())
}

fn is_windows_device_name(component: &str) -> bool {
    // Windows ignores the extension and trailing spaces and dots: `NUL.txt`, `con .json`.
    let stem = component.split('.').next().unwrap_or(component).trim_end_matches([' ', '.']);
    WINDOWS_DEVICE_NAMES.iter().any(|device| device.eq_ignore_ascii_case(stem))
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
    let mut knowledge_total: u64 = 0;
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
        let is_knowledge = name.starts_with("knowledge/");
        let entry_limit = if is_knowledge { limits.max_knowledge_bytes } else { limits.max_entry_bytes };
        let declared = entry.size();
        if declared > entry_limit {
            return Err(BundleError::Archive(format!("`{name}` is larger than {entry_limit} bytes")));
        }
        let compressed = entry.compressed_size().max(1);
        if declared / compressed > limits.max_ratio {
            return Err(BundleError::Archive(format!("`{name}` has a suspicious compression ratio")));
        }
        // Never trust the declared size: read at most the limit plus one byte.
        let mut data = Vec::with_capacity(usize::try_from(declared.min(1 << 20)).unwrap_or(0));
        (&mut entry)
            .take(entry_limit.saturating_add(1))
            .read_to_end(&mut data)
            .map_err(|e| BundleError::Archive(format!("`{name}`: {e}")))?;
        let len = u64::try_from(data.len()).unwrap_or(u64::MAX);
        if len > entry_limit {
            return Err(BundleError::Archive(format!("`{name}` is larger than {entry_limit} bytes")));
        }
        if is_knowledge {
            knowledge_total = knowledge_total.saturating_add(len);
            if knowledge_total > limits.max_knowledge_bytes {
                return Err(BundleError::Archive(format!(
                    "the bundle's knowledge exceeds the limit of {} bytes",
                    limits.max_knowledge_bytes
                )));
            }
        } else {
            total = total.saturating_add(len);
            if total > limits.max_total_bytes {
                return Err(BundleError::Archive(format!(
                    "bundle content exceeds the limit of {} bytes",
                    limits.max_total_bytes
                )));
            }
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

    #[test]
    fn device_names_and_case_collisions_are_not_extractable() {
        assert!(
            check_extractable(["bundle.json", "workflows/console.yaml", "assets/com10.txt", "nullable.md"]).is_ok()
        );
        for device in ["CON", "nul.txt", "workflows/Aux.yaml", "com1.json", "LPT9", "prn .md", "lpt3/x.yaml"] {
            let err = check_extractable(["bundle.json", device]).unwrap_err().to_string();
            assert!(err.contains("Windows device name"), "{device}: {err}");
        }
        let err = check_extractable(["plugins/A.wasm", "plugins/a.wasm"]).unwrap_err().to_string();
        assert!(err.contains("different case"), "{err}");
    }
}
