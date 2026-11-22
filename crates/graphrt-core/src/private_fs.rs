//! Owner-only files and directories for hosts that write run data to disk.
//!
//! Run stores, scores, replay cassettes and evaluation reports hold prompts,
//! model answers and user data. Written with the default umask (often `022`)
//! they are readable by every local user, so hosts create them through these
//! helpers instead: `0600` files and `0700` directories on Unix. Other
//! platforms rely on the per-user ACLs of the parent directory.
//!
//! Modes only apply to what these functions create: an existing file or
//! directory keeps its permissions, so an operator's deliberate `chmod` is
//! respected. Atomic replaces (write a new temporary file, then rename) always
//! end up owner-only.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Mode of directories created here (Unix).
pub const PRIVATE_DIR_MODE: u32 = 0o700;
/// Mode of files created here (Unix).
pub const PRIVATE_FILE_MODE: u32 = 0o600;

/// Create `path` and its missing parents, each new one owner-only.
///
/// @param path - Directory to create; existing directories are left as they are.
/// @throws io::Error - When a directory cannot be created.
///
/// @example
/// graphrt_core::private_fs::create_dir_all(Path::new("store/runs/r1"))?;
pub fn create_dir_all(path: &Path) -> io::Result<()> {
    private_dir_builder().recursive(true).create(path)
}

/// Create a new, owner-only directory under the system temp directory, named
/// `<prefix>-<pid>-<counter>-<nanos>`.
///
/// Creation fails if the path already exists, so another local user cannot
/// pre-create it (or a symlink in its place) to read or swap what the caller
/// writes there. The caller removes it.
///
/// @param prefix - Name prefix, e.g. `graphrt-bundle-config`.
/// @returns The new directory's path.
/// @throws io::Error - When the directory cannot be created.
pub fn create_fresh_temp_dir(prefix: &str) -> io::Result<PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default();
    let unique = format!("{}-{}-{nanos}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed));
    let dir = std::env::temp_dir().join(format!("{prefix}-{unique}"));
    private_dir_builder().create(&dir)?;
    Ok(dir)
}

/// Open `path` for writing, truncating it, creating it owner-only when missing.
///
/// @throws io::Error - When the file cannot be opened.
pub fn create_file(path: &Path) -> io::Result<File> {
    private_open_options().write(true).create(true).truncate(true).open(path)
}

/// Open `path` for appending, creating it owner-only when missing.
///
/// @throws io::Error - When the file cannot be opened.
pub fn append_file(path: &Path) -> io::Result<File> {
    private_open_options().append(true).create(true).open(path)
}

/// Write `bytes` to `path` (like [`std::fs::write`]), creating it owner-only.
///
/// @throws io::Error - When the file cannot be written.
pub fn write(path: &Path, bytes: impl AsRef<[u8]>) -> io::Result<()> {
    create_file(path)?.write_all(bytes.as_ref())
}

fn private_dir_builder() -> DirBuilder {
    #[cfg_attr(not(unix), allow(unused_mut))] // only mutated by the Unix mode below
    let mut builder = DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, PRIVATE_DIR_MODE);
    builder
}

fn private_open_options() -> OpenOptions {
    #[cfg_attr(not(unix), allow(unused_mut))] // only mutated by the Unix mode below
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, PRIVATE_FILE_MODE);
    options
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn fresh_temp_dirs_are_new_and_owner_only() {
        let first = create_fresh_temp_dir("graphrt-private-fs-test").unwrap();
        let second = create_fresh_temp_dir("graphrt-private-fs-test").unwrap();
        assert_ne!(first, second);
        #[cfg(unix)]
        assert_eq!(mode_of(&first), PRIVATE_DIR_MODE);
        for dir in [first, second] {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn files_and_nested_dirs_are_owner_only() {
        let root = create_fresh_temp_dir("graphrt-private-fs-test").unwrap();
        let nested = root.join("a/b");
        create_dir_all(&nested).unwrap();
        let file = nested.join("data.json");
        write(&file, b"{}").unwrap();
        append_file(&nested.join("log.jsonl")).unwrap().write_all(b"x\n").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"{}");
        #[cfg(unix)]
        {
            assert_eq!(mode_of(&root.join("a")), PRIVATE_DIR_MODE);
            assert_eq!(mode_of(&nested), PRIVATE_DIR_MODE);
            assert_eq!(mode_of(&file), PRIVATE_FILE_MODE);
            assert_eq!(mode_of(&nested.join("log.jsonl")), PRIVATE_FILE_MODE);
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
