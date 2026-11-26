//! [`FileStore`] over one directory on disk.
//!
//! Every path is resolved one component at a time from the root, and a
//! symbolic link (or a Windows junction) anywhere on the way is refused, so
//! nothing inside the directory can point a read or a write somewhere else.
//! Files are opened without following a final link (`O_NOFOLLOW` on Unix,
//! `FILE_FLAG_OPEN_REPARSE_POINT` on Windows) and checked again through the
//! open handle. Writes go to a temporary file that is renamed into place.
//!
//! All of it runs on the blocking pool, never on an async worker.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use futures::future::BoxFuture;

use crate::path::{MAX_PATH_DEPTH, WorkspacePath};
use crate::store::{Entry, FileStore, Listing, StoreError};

/// Buffer for streaming a file through SHA-256.
const HASH_BUFFER_BYTES: usize = 64 * 1024;

/// Numbers the temporary files of this process.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A directory used as a file store.
#[derive(Clone, Debug)]
pub struct DirStore {
    root: Arc<PathBuf>,
}

impl DirStore {
    /// Use `root` as the store, creating it (owner only on Unix) when missing.
    ///
    /// The root itself may be reached through links (a temp directory often
    /// is): it is resolved once, here, and everything below it is link free.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let root = root.as_ref();
        let shown = root.display().to_string();
        if !root.exists() {
            fs::create_dir_all(root).map_err(|e| StoreError::io(&shown, e))?;
            restrict_dir(root);
        }
        let real = fs::canonicalize(root).map_err(|e| StoreError::io(&shown, e))?;
        if !real.is_dir() {
            return Err(StoreError::NotAFile(shown));
        }
        Ok(Self { root: Arc::new(real) })
    }

    /// The resolved root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl FileStore for DirStore {
    fn stat<'a>(&'a self, path: &'a WorkspacePath) -> BoxFuture<'a, Result<Option<Entry>, StoreError>> {
        let (root, path) = (Arc::clone(&self.root), path.clone());
        Box::pin(run(path.to_string(), move || stat_file(&root, &path)))
    }

    fn read<'a>(
        &'a self,
        path: &'a WorkspacePath,
        offset: u64,
        max_bytes: u64,
    ) -> BoxFuture<'a, Result<Vec<u8>, StoreError>> {
        let (root, path) = (Arc::clone(&self.root), path.clone());
        Box::pin(run(path.to_string(), move || read_file(&root, &path, offset, max_bytes)))
    }

    fn write<'a>(
        &'a self,
        path: &'a WorkspacePath,
        bytes: Vec<u8>,
        overwrite: bool,
    ) -> BoxFuture<'a, Result<Entry, StoreError>> {
        let (root, path) = (Arc::clone(&self.root), path.clone());
        Box::pin(run(path.to_string(), move || write_file(&root, &path, &bytes, overwrite)))
    }

    fn delete<'a>(&'a self, path: &'a WorkspacePath) -> BoxFuture<'a, Result<(), StoreError>> {
        let (root, path) = (Arc::clone(&self.root), path.clone());
        Box::pin(run(path.to_string(), move || delete_file(&root, &path)))
    }

    fn list<'a>(
        &'a self,
        dir: Option<&'a WorkspacePath>,
        max_entries: usize,
    ) -> BoxFuture<'a, Result<Listing, StoreError>> {
        let (root, dir) = (Arc::clone(&self.root), dir.cloned());
        let shown = dir.as_ref().map(ToString::to_string).unwrap_or_default();
        Box::pin(run(shown, move || list_files(&root, dir.as_ref(), max_entries)))
    }

    /// One pass over the file with a 64 KiB buffer, in a single blocking task.
    fn sha256<'a>(&'a self, path: &'a WorkspacePath) -> BoxFuture<'a, Result<[u8; 32], StoreError>> {
        let (root, path) = (Arc::clone(&self.root), path.clone());
        Box::pin(run(path.to_string(), move || hash_file(&root, &path)))
    }
}

/// Run blocking file work off the async workers.
async fn run<T: Send + 'static>(
    shown_path: String,
    work: impl FnOnce() -> Result<T, StoreError> + Send + 'static,
) -> Result<T, StoreError> {
    match crate::blocking(work).await {
        Ok(result) => result,
        Err(message) => Err(StoreError::io(&shown_path, message)),
    }
}

fn io_error(path: &WorkspacePath, error: &std::io::Error) -> StoreError {
    match error.kind() {
        std::io::ErrorKind::NotFound => StoreError::NotFound(path.to_string()),
        _ => StoreError::io(path.as_str(), error),
    }
}

/// What to do when a directory on the way does not exist.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Missing {
    Fail,
    Create,
}

/// The on-disk location of `path`, after checking every directory above it.
///
/// Each existing component must be a real directory: a symbolic link, a
/// junction or a file in that position is refused. The last component is not
/// checked here; the caller opens it without following links.
fn locate(root: &Path, path: &WorkspacePath, missing: Missing) -> Result<PathBuf, StoreError> {
    let mut current = root.to_path_buf();
    let components: Vec<&str> = path.components().collect();
    let (file_name, directories) = components.split_last().ok_or_else(|| StoreError::NotFound(path.to_string()))?;
    for directory in directories {
        current.push(directory);
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(StoreError::denied(path, "a directory on the path is a link"));
            }
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(StoreError::NotAFile(path.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && missing == Missing::Create => {
                match fs::create_dir(&current) {
                    Ok(()) => restrict_dir(&current),
                    // Another writer created it in the meantime.
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(io_error(path, &e)),
                }
            }
            Err(e) => return Err(io_error(path, &e)),
        }
    }
    current.push(file_name);
    Ok(current)
}

/// Open a regular file for reading, refusing links.
fn open_regular(root: &Path, path: &WorkspacePath) -> Result<(fs::File, fs::Metadata), StoreError> {
    let location = locate(root, path, Missing::Fail)?;
    // A link as the last component is refused by name first (a clear error on
    // every system), then again by the open flags below (no window to swap it in).
    match fs::symlink_metadata(&location) {
        Ok(meta) if meta.file_type().is_symlink() => return Err(StoreError::denied(path, "the file is a link")),
        // Directories, FIFOs, sockets and devices are never opened.
        Ok(meta) if !meta.is_file() => return Err(StoreError::NotAFile(path.to_string())),
        Ok(_) => {}
        Err(e) => return Err(io_error(path, &e)),
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    no_follow(&mut options);
    let file = options.open(&location).map_err(|e| match e.raw_os_error() {
        // ELOOP: the last component turned into a link after the check above.
        #[cfg(unix)]
        Some(code) if code == libc::ELOOP => StoreError::denied(path, "the file is a link"),
        _ => io_error(path, &e),
    })?;
    let meta = file.metadata().map_err(|e| io_error(path, &e))?;
    if meta.file_type().is_symlink() {
        return Err(StoreError::denied(path, "the file is a link"));
    }
    if !meta.is_file() {
        return Err(StoreError::NotAFile(path.to_string()));
    }
    Ok((file, meta))
}

#[cfg(unix)]
fn no_follow(options: &mut fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    // O_NONBLOCK: opening a FIFO someone planted must not hang the worker.
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
}

#[cfg(windows)]
fn no_follow(options: &mut fs::OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;
    /// Open a reparse point (symbolic link, junction) itself instead of its target.
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
}

#[cfg(not(any(unix, windows)))]
fn no_follow(_options: &mut fs::OpenOptions) {}

fn entry(path: &WorkspacePath, meta: &fs::Metadata) -> Entry {
    let modified_ms = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|since| u64::try_from(since.as_millis()).ok());
    Entry { path: path.clone(), size: meta.len(), modified_ms }
}

fn stat_file(root: &Path, path: &WorkspacePath) -> Result<Option<Entry>, StoreError> {
    let location = match locate(root, path, Missing::Fail) {
        Ok(location) => location,
        Err(StoreError::NotFound(_)) => return Ok(None),
        Err(e) => return Err(e),
    };
    match fs::symlink_metadata(&location) {
        Ok(meta) if meta.file_type().is_symlink() => Err(StoreError::denied(path, "the file is a link")),
        Ok(meta) if meta.is_file() => Ok(Some(entry(path, &meta))),
        Ok(_) => Err(StoreError::NotAFile(path.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error(path, &e)),
    }
}

fn read_file(root: &Path, path: &WorkspacePath, offset: u64, max_bytes: u64) -> Result<Vec<u8>, StoreError> {
    let (mut file, meta) = open_regular(root, path)?;
    if offset >= meta.len() {
        return Ok(Vec::new());
    }
    if offset > 0 {
        file.seek(SeekFrom::Start(offset)).map_err(|e| io_error(path, &e))?;
    }
    // The file may grow while it is read: `take` is the hard cap, the size only a hint.
    let expected = (meta.len() - offset).min(max_bytes);
    let mut bytes = Vec::with_capacity(usize::try_from(expected).unwrap_or(0));
    file.take(max_bytes).read_to_end(&mut bytes).map_err(|e| io_error(path, &e))?;
    Ok(bytes)
}

fn hash_file(root: &Path, path: &WorkspacePath) -> Result<[u8; 32], StoreError> {
    use sha2::Digest;
    let (mut file, _) = open_regular(root, path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = vec![0u8; HASH_BUFFER_BYTES];
    loop {
        let read = file.read(&mut buffer).map_err(|e| io_error(path, &e))?;
        if read == 0 {
            break;
        }
        hasher.update(buffer.get(..read).unwrap_or_default());
    }
    Ok(hasher.finalize().into())
}

fn write_file(root: &Path, path: &WorkspacePath, bytes: &[u8], overwrite: bool) -> Result<Entry, StoreError> {
    let location = locate(root, path, Missing::Create)?;
    match fs::symlink_metadata(&location) {
        Ok(meta) if meta.file_type().is_symlink() => return Err(StoreError::denied(path, "the file is a link")),
        Ok(meta) if !meta.is_file() => return Err(StoreError::NotAFile(path.to_string())),
        Ok(_) if !overwrite => return Err(StoreError::AlreadyExists(path.to_string())),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_error(path, &e)),
    }
    let directory = location.parent().unwrap_or(root);
    // The leading dot keeps the temporary file out of listings and out of reach
    // of any workspace path (hidden names are never valid).
    let temp_name = format!(".graphrt-{}-{}.tmp", std::process::id(), TEMP_COUNTER.fetch_add(1, Ordering::Relaxed));
    let temp = directory.join(temp_name);
    let written = write_temp(&temp, bytes).and_then(|()| rename_into_place(&temp, &location));
    if let Err(e) = written {
        // Best effort: the temporary file is hidden and harmless if it stays.
        let _ = fs::remove_file(&temp);
        return Err(io_error(path, &e));
    }
    let meta = fs::symlink_metadata(&location).map_err(|e| io_error(path, &e))?;
    Ok(entry(path, &meta))
}

/// Rename the finished temporary file over the target.
///
/// On Windows a rename fails with "access denied" while another handle has the
/// target open (a concurrent reader, a second writer finishing its own rename,
/// an indexer). That state lasts milliseconds, so the rename is tried again a
/// few times before the write is reported as failed.
fn rename_into_place(temp: &Path, location: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        const ATTEMPTS: u32 = 8;
        let mut wait = std::time::Duration::from_millis(1);
        for _ in 1..ATTEMPTS {
            match fs::rename(temp, location) {
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    std::thread::sleep(wait);
                    wait *= 2;
                }
                other => return other,
            }
        }
    }
    fs::rename(temp, location)
}

fn write_temp(temp: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = fs::OpenOptions::new();
    // `create_new` never follows a link and never reuses a file someone else made.
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(temp)?;
    file.write_all(bytes)?;
    file.flush()
}

fn delete_file(root: &Path, path: &WorkspacePath) -> Result<(), StoreError> {
    let location = locate(root, path, Missing::Fail)?;
    match fs::symlink_metadata(&location) {
        Ok(meta) if meta.file_type().is_symlink() => Err(StoreError::denied(path, "the file is a link")),
        Ok(meta) if !meta.is_file() => Err(StoreError::NotAFile(path.to_string())),
        Ok(_) => fs::remove_file(&location).map_err(|e| io_error(path, &e)),
        Err(e) => Err(io_error(path, &e)),
    }
}

fn list_files(root: &Path, dir: Option<&WorkspacePath>, max_entries: usize) -> Result<Listing, StoreError> {
    let start = match dir {
        None => root.to_path_buf(),
        Some(dir) => {
            let location = locate(root, dir, Missing::Fail)?;
            match fs::symlink_metadata(&location) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(StoreError::denied(dir, "the directory is a link"));
                }
                Ok(meta) if meta.is_dir() => location,
                Ok(_) => return Err(StoreError::NotAFile(dir.to_string())),
                Err(e) => return Err(io_error(dir, &e)),
            }
        }
    };
    let mut listing = Listing::default();
    walk(&start, dir.map(WorkspacePath::as_str), 0, max_entries, &mut listing)?;
    Ok(listing)
}

/// Depth-first walk in name order. Links, hidden names and names that are not
/// valid workspace paths are skipped: they cannot be addressed anyway.
fn walk(
    directory: &Path,
    prefix: Option<&str>,
    depth: usize,
    max_entries: usize,
    listing: &mut Listing,
) -> Result<(), StoreError> {
    let shown = prefix.unwrap_or_default();
    let reader = fs::read_dir(directory).map_err(|e| StoreError::io(shown, e))?;
    let mut names: Vec<(String, fs::FileType)> = Vec::new();
    for item in reader {
        let item = item.map_err(|e| StoreError::io(shown, e))?;
        let (Ok(name), Ok(file_type)) = (item.file_name().into_string(), item.file_type()) else { continue };
        names.push((name, file_type));
    }
    names.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, file_type) in names {
        if listing.truncated {
            return Ok(());
        }
        let relative = match prefix {
            Some(prefix) => format!("{prefix}/{name}"),
            None => name.clone(),
        };
        let Ok(path) = WorkspacePath::parse(&relative) else { continue };
        if file_type.is_symlink() {
            continue;
        }
        let location = directory.join(&name);
        if file_type.is_dir() {
            if depth + 1 < MAX_PATH_DEPTH {
                walk(&location, Some(path.as_str()), depth + 1, max_entries, listing)?;
            }
        } else if file_type.is_file() {
            if listing.entries.len() >= max_entries {
                listing.truncated = true;
                return Ok(());
            }
            // A file removed since `read_dir` is simply not listed.
            if let Ok(meta) = fs::symlink_metadata(&location) {
                listing.entries.push(entry(&path, &meta));
            }
        }
    }
    Ok(())
}

/// Owner-only directory on Unix (0700). Other systems inherit the parent's access rules.
fn restrict_dir(directory: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Best effort: a directory on a file system without modes still works.
        let _ = fs::set_permissions(directory, fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = directory;
}
