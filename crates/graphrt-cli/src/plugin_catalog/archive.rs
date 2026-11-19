//! Safe extraction of plugin release tarballs (`.tar.gz`).
//!
//! Release tarballs hold regular files and directories only (graphrt-plugins'
//! `tar-archive.mjs` refuses everything else when it writes them), so this
//! reader accepts exactly that and refuses the rest instead of trying to make
//! it safe:
//!
//! - absolute paths, `..` components, backslashes and NUL bytes;
//! - symlinks and hard links (wherever they point), device files and FIFOs;
//! - duplicate paths (a later entry may not overwrite an earlier one);
//! - more than [`ExtractLimits::max_total_bytes`] of file contents or
//!   [`ExtractLimits::max_entries`] entries (zip bombs);
//! - headers with a bad checksum, base-256 sizes or truncated contents.
//!
//! Supported headers: ustar (with `prefix`), PAX `path` records and GNU long
//! names (`L`). PAX global headers (`g`) are skipped.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

const BLOCK_SIZE: usize = 512;
/// PAX and GNU long-name records are short; anything larger is refused.
const MAX_METADATA_BYTES: u64 = 64 * 1024;

/// Caps on what one tarball may extract.
#[derive(Clone, Copy, Debug)]
pub struct ExtractLimits {
    /// Total bytes of file contents.
    pub max_total_bytes: u64,
    /// Number of entries (files and directories).
    pub max_entries: usize,
}

/// Extract the gzip-compressed tar `archive` into the existing, empty `destination`.
///
/// # Errors
/// A message naming the offending entry for every refusal listed in the module
/// documentation, and for I/O failures. The caller removes `destination`.
pub fn extract_tar_gz(archive: &[u8], destination: &Path, limits: ExtractLimits) -> Result<(), String> {
    let decoder = flate2::read::GzDecoder::new(archive);
    let mut reader = TarReader { input: decoder, limits, entry_count: 0, content_bytes: 0 };
    while let Some(entry) = reader.next_entry()? {
        write_entry(&mut reader, &entry, destination)?;
    }
    Ok(())
}

/// One tar member, after long-name records are applied.
#[derive(Debug)]
struct TarEntry {
    path: String,
    kind: EntryKind,
    size: u64,
    mode: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryKind {
    File,
    Directory,
}

struct TarReader<R> {
    input: R,
    limits: ExtractLimits,
    entry_count: usize,
    content_bytes: u64,
}

impl<R: Read> TarReader<R> {
    /// The next file or directory entry, `None` at the end-of-archive marker.
    fn next_entry(&mut self) -> Result<Option<TarEntry>, String> {
        let mut long_path: Option<String> = None;
        loop {
            let Some(header) = self.read_header()? else {
                return Ok(None);
            };
            let size = parse_size(&header)?;
            let type_flag = header.get(156).copied().unwrap_or(0);
            match type_flag {
                b'x' => long_path = parse_pax_path(&self.read_metadata(size)?).or(long_path),
                b'L' => long_path = Some(nul_terminated(&self.read_metadata(size)?)),
                b'g' => {
                    self.read_metadata(size)?;
                }
                _ => {
                    let path = long_path.take().unwrap_or_else(|| header_path(&header));
                    let kind = entry_kind(type_flag, &path)?;
                    self.count_entry(&path, kind, size)?;
                    return Ok(Some(TarEntry { path, kind, size, mode: parse_octal(header_field(&header, 100, 8))? }));
                }
            }
        }
    }

    fn read_header(&mut self) -> Result<Option<[u8; BLOCK_SIZE]>, String> {
        let mut header = [0u8; BLOCK_SIZE];
        self.input.read_exact(&mut header).map_err(|error| format!("truncated or corrupt tarball: {error}"))?;
        if header.iter().all(|&byte| byte == 0) {
            return Ok(None);
        }
        verify_header_checksum(&header)?;
        Ok(Some(header))
    }

    fn read_metadata(&mut self, size: u64) -> Result<Vec<u8>, String> {
        if size > MAX_METADATA_BYTES {
            return Err(format!("tar metadata record of {size} bytes is too large"));
        }
        // Metadata records count as entries, so endless headers cannot loop forever.
        self.entry_count += 1;
        if self.entry_count > self.limits.max_entries {
            return Err(format!("tarball has more than {} entries", self.limits.max_entries));
        }
        let mut record = Vec::new();
        self.copy_contents(size, &mut record)?;
        Ok(record)
    }

    fn count_entry(&mut self, path: &str, kind: EntryKind, size: u64) -> Result<(), String> {
        self.entry_count += 1;
        if self.entry_count > self.limits.max_entries {
            return Err(format!("tarball has more than {} entries", self.limits.max_entries));
        }
        if kind == EntryKind::Directory && size != 0 {
            return Err(format!("tar entry `{path}`: a directory cannot have contents"));
        }
        self.content_bytes = self.content_bytes.saturating_add(size);
        if self.content_bytes > self.limits.max_total_bytes {
            return Err(format!("tarball contents exceed the {}-byte extraction limit", self.limits.max_total_bytes));
        }
        Ok(())
    }

    /// Copy exactly `size` content bytes into `sink`, then skip the block padding.
    fn copy_contents(&mut self, size: u64, sink: &mut impl std::io::Write) -> Result<(), String> {
        let copied = std::io::copy(&mut (&mut self.input).take(size), sink).map_err(|error| error.to_string())?;
        if copied != size {
            return Err("truncated tarball: an entry is shorter than its header says".into());
        }
        let padding = (BLOCK_SIZE as u64 - size % BLOCK_SIZE as u64) % BLOCK_SIZE as u64;
        let skipped =
            std::io::copy(&mut (&mut self.input).take(padding), &mut std::io::sink()).map_err(|e| e.to_string())?;
        if skipped != padding {
            return Err("truncated tarball: missing block padding".into());
        }
        Ok(())
    }
}

fn entry_kind(type_flag: u8, path: &str) -> Result<EntryKind, String> {
    match type_flag {
        b'0' | 0 => Ok(EntryKind::File),
        b'5' => Ok(EntryKind::Directory),
        b'1' => Err(format!("tar entry `{path}` is a hard link, which plugin tarballs may not contain")),
        b'2' => Err(format!("tar entry `{path}` is a symlink, which plugin tarballs may not contain")),
        b'3' | b'4' | b'6' => Err(format!("tar entry `{path}` is a device file or FIFO, which is not allowed")),
        other => Err(format!("tar entry `{path}` has unsupported type `{}`", char::from(other))),
    }
}

fn write_entry<R: Read>(reader: &mut TarReader<R>, entry: &TarEntry, destination: &Path) -> Result<(), String> {
    let relative = safe_relative_path(&entry.path)?;
    let target = destination.join(&relative);
    let io_error = |error: std::io::Error| format!("cannot extract `{}`: {error}", entry.path);
    if entry.kind == EntryKind::Directory {
        return std::fs::create_dir_all(&target).map_err(io_error);
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(io_error)?;
    }
    // `create_new` refuses duplicates, and never follows a symlink at `target`.
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&target).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            format!("tar entry `{}` appears twice", entry.path)
        } else {
            io_error(error)
        }
    })?;
    reader.copy_contents(entry.size, &mut file)?;
    set_file_mode(&file, entry.mode).map_err(io_error)
}

/// Files keep only their executable bit: `0755` or `0644`.
#[cfg(unix)]
fn set_file_mode(file: &std::fs::File, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let normalized = if mode & 0o111 != 0 { 0o755 } else { 0o644 };
    file.set_permissions(std::fs::Permissions::from_mode(normalized))
}

#[cfg(not(unix))]
fn set_file_mode(_file: &std::fs::File, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

/// A relative path made only of normal components.
///
/// # Errors
/// For empty and absolute paths, `..`, backslashes, NUL bytes and drive prefixes.
pub fn safe_relative_path(path: &str) -> Result<PathBuf, String> {
    let refuse = |reason: &str| Err(format!("tar entry `{path}` is refused: {reason}"));
    if path.contains('\\') || path.contains('\0') {
        return refuse("backslashes and NUL bytes are not allowed");
    }
    let mut relative = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            Component::ParentDir => return refuse("`..` is not allowed"),
            Component::RootDir | Component::Prefix(_) => return refuse("absolute paths are not allowed"),
        }
    }
    if relative.as_os_str().is_empty() {
        return refuse("empty path");
    }
    Ok(relative)
}

fn header_field(header: &[u8; BLOCK_SIZE], offset: usize, length: usize) -> &[u8] {
    header.get(offset..offset + length).unwrap_or_default()
}

fn nul_terminated(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&byte| byte == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(bytes.get(..end).unwrap_or_default()).into_owned()
}

/// `prefix/name` for ustar headers, `name` otherwise.
fn header_path(header: &[u8; BLOCK_SIZE]) -> String {
    let name = nul_terminated(header_field(header, 0, 100));
    let is_ustar = header_field(header, 257, 5) == b"ustar";
    let prefix = if is_ustar { nul_terminated(header_field(header, 345, 155)) } else { String::new() };
    if prefix.is_empty() { name } else { format!("{prefix}/{name}") }
}

fn parse_octal(field: &[u8]) -> Result<u32, String> {
    let value = parse_octal_u64(field)?;
    u32::try_from(value).map_err(|_| "tar header mode is out of range".to_owned())
}

fn parse_octal_u64(field: &[u8]) -> Result<u64, String> {
    let text = nul_terminated(field);
    let digits = text.trim_matches(|c: char| c == ' ' || c == '\0');
    if digits.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(digits, 8).map_err(|_| format!("invalid octal number `{digits}` in a tar header"))
}

fn parse_size(header: &[u8; BLOCK_SIZE]) -> Result<u64, String> {
    let field = header_field(header, 124, 12);
    if field.first().is_some_and(|&byte| byte & 0x80 != 0) {
        return Err("tar entries with base-256 sizes are not supported".into());
    }
    parse_octal_u64(field)
}

/// The header checksum: the byte sum with the checksum field read as spaces.
fn verify_header_checksum(header: &[u8; BLOCK_SIZE]) -> Result<(), String> {
    let expected = parse_octal_u64(header_field(header, 148, 8))?;
    let actual: u64 = header
        .iter()
        .enumerate()
        .map(|(position, &byte)| if (148..156).contains(&position) { u64::from(b' ') } else { u64::from(byte) })
        .sum();
    if expected != actual {
        return Err("corrupt tarball: tar header checksum mismatch".into());
    }
    Ok(())
}

/// The `path` value of a PAX extended header (`<length> path=<value>\n` records).
fn parse_pax_path(record_bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(record_bytes);
    text.lines().find_map(|line| {
        let (_length, key_value) = line.split_once(' ')?;
        key_value.strip_prefix("path=").map(str::to_owned)
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    const LIMITS: ExtractLimits = ExtractLimits { max_total_bytes: 1024 * 1024, max_entries: 100 };

    fn scratch(test_name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("graphrt-archive-test-{}-{test_name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_octal(field: &mut [u8], value: u64) {
        let text = format!("{value:0width$o}\0", width = field.len() - 1);
        field.copy_from_slice(text.as_bytes());
    }

    fn header(path: &str, type_flag: u8, size: usize, mode: u32) -> [u8; BLOCK_SIZE] {
        let mut header = [0u8; BLOCK_SIZE];
        header[..path.len()].copy_from_slice(path.as_bytes());
        write_octal(&mut header[100..108], u64::from(mode));
        write_octal(&mut header[124..136], size as u64);
        header[156] = type_flag;
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        header[148..156].copy_from_slice(b"        ");
        let checksum: u32 = header.iter().map(|&byte| u32::from(byte)).sum();
        header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
        header
    }

    fn tar(entries: &[(&str, u8, &[u8])]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (path, type_flag, content) in entries {
            bytes.extend_from_slice(&header(path, *type_flag, content.len(), 0o644));
            bytes.extend_from_slice(content);
            bytes.resize(bytes.len().div_ceil(BLOCK_SIZE) * BLOCK_SIZE, 0);
        }
        bytes.extend_from_slice(&[0u8; 2 * BLOCK_SIZE]);
        bytes
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn extract(test_name: &str, entries: &[(&str, u8, &[u8])], limits: ExtractLimits) -> (PathBuf, Result<(), String>) {
        let destination = scratch(test_name);
        let result = extract_tar_gz(&gzip(&tar(entries)), &destination, limits);
        (destination, result)
    }

    #[test]
    fn extracts_files_and_directories() {
        let (destination, result) = extract(
            "ok",
            &[("demo", b'5', b""), ("demo/a.txt", b'0', b"alpha"), ("demo/deep/b.txt", 0, b"beta")],
            LIMITS,
        );
        result.unwrap();
        assert_eq!(std::fs::read_to_string(destination.join("demo/a.txt")).unwrap(), "alpha");
        assert_eq!(std::fs::read_to_string(destination.join("demo/deep/b.txt")).unwrap(), "beta");
    }

    #[test]
    fn applies_pax_and_gnu_long_names() {
        let long_name = format!("demo/{}/file.txt", "d".repeat(120));
        let pax_record = format!("{} path={long_name}\n", long_name.len() + 7);
        let gnu_name = format!("demo/{}/gnu.txt", "g".repeat(120));
        let (destination, result) = extract(
            "long-names",
            &[
                ("pax", b'x', pax_record.as_bytes()),
                ("ignored-name", b'0', b"pax"),
                ("././@LongLink", b'L', format!("{gnu_name}\0").as_bytes()),
                ("ignored-too", b'0', b"gnu"),
            ],
            LIMITS,
        );
        result.unwrap();
        assert_eq!(std::fs::read_to_string(destination.join(&long_name)).unwrap(), "pax");
        assert_eq!(std::fs::read_to_string(destination.join(&gnu_name)).unwrap(), "gnu");
        assert!(!destination.join("ignored-name").exists());
    }

    #[test]
    fn refuses_unsafe_paths() {
        for (case, path) in [("absolute", "/etc/evil"), ("parent", "demo/../../evil"), ("backslash", "demo\\..\\evil")]
        {
            let (destination, result) = extract(case, &[(path, b'0', b"x")], LIMITS);
            let message = result.unwrap_err();
            assert!(message.contains("is refused"), "{case}: {message}");
            assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 0, "{case}");
        }
    }

    #[test]
    fn refuses_links_devices_and_fifos() {
        for (type_flag, expected) in [
            (b'1', "hard link"),
            (b'2', "symlink"),
            (b'3', "device"),
            (b'4', "device"),
            (b'6', "FIFO"),
            (b'S', "unsupported"),
        ] {
            let (_, result) =
                extract(&format!("type-{}", char::from(type_flag)), &[("demo/x", type_flag, b"")], LIMITS);
            let message = result.unwrap_err();
            assert!(message.contains(expected), "{}: {message}", char::from(type_flag));
        }
    }

    #[test]
    fn refuses_duplicate_entries() {
        let (_, result) = extract("duplicate", &[("demo/a", b'0', b"one"), ("demo/a", b'0', b"two")], LIMITS);
        assert!(result.unwrap_err().contains("appears twice"));
    }

    #[test]
    fn caps_total_size_and_entry_count() {
        let small = ExtractLimits { max_total_bytes: 8, max_entries: 2 };
        let (_, too_big) = extract("too-big", &[("demo/a", b'0', b"123456789")], small);
        assert!(too_big.unwrap_err().contains("8-byte extraction limit"));
        let (_, too_many) = extract("too-many", &[("a", b'5', b""), ("b", b'5', b""), ("c", b'5', b"")], small);
        assert!(too_many.unwrap_err().contains("more than 2 entries"));
    }

    #[test]
    fn refuses_corrupt_and_truncated_archives() {
        let mut corrupt = tar(&[("demo/a", b'0', b"abc")]);
        corrupt[0] = b'X';
        let destination = scratch("corrupt");
        assert!(extract_tar_gz(&gzip(&corrupt), &destination, LIMITS).unwrap_err().contains("checksum"));
        let mut truncated = tar(&[("demo/a", b'0', &[b'a'; 600])]);
        truncated.truncate(BLOCK_SIZE + 100);
        assert!(extract_tar_gz(&gzip(&truncated), &scratch("truncated"), LIMITS).unwrap_err().contains("truncated"));
        assert!(extract_tar_gz(b"not gzip", &scratch("not-gzip"), LIMITS).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn keeps_only_the_executable_bit() {
        use std::os::unix::fs::PermissionsExt;
        let destination = scratch("modes");
        let mut bytes = Vec::new();
        for (path, mode) in [("run.sh", 0o4777), ("data.txt", 0o666)] {
            bytes.extend_from_slice(&header(path, b'0', 0, mode));
        }
        bytes.extend_from_slice(&[0u8; 2 * BLOCK_SIZE]);
        extract_tar_gz(&gzip(&bytes), &destination, LIMITS).unwrap();
        let mode_of = |name: &str| std::fs::metadata(destination.join(name)).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode_of("run.sh"), 0o755);
        assert_eq!(mode_of("data.txt"), 0o644);
    }
}
