//! A workspace: a [`FileStore`] with limits, a quota and type checks.
//!
//! Everything an agent does with files goes through a [`Workspace`]. The store
//! below it only moves bytes; the workspace decides whether a file may be
//! written at all (size, quota, content type) and names files as graphrt
//! [`File`] values.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use graphrt_core::value::{File, FileTransfer};

use crate::dir::DirStore;
use crate::memory::MemoryStore;
use crate::path::{PathError, WorkspacePath};
use crate::sniff::{MediaClass, MediaType, SNIFF_BYTES, sniff};
use crate::store::{Entry, FileStore, Listing, StoreError, hex};

/// Prefix of the `reference` of files that live in a workspace.
pub const REFERENCE_SCHEME: &str = "workspace:";
/// Directory holding the per-scope sub-workspaces.
const SCOPES_DIR: &str = "scopes";

/// What a workspace allows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceLimits {
    /// Largest single file, in bytes.
    pub max_file_bytes: u64,
    /// Most bytes the workspace (or one scope of it) may hold.
    pub max_total_bytes: u64,
    /// Most files the workspace (or one scope of it) may hold.
    pub max_files: usize,
    /// Refuse every write and delete.
    pub read_only: bool,
    /// Accept native programs and scripts. Off by default: an agent has no
    /// reason to store something the operating system would run.
    pub allow_executables: bool,
}

impl Default for WorkspaceLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 32 * 1024 * 1024,
            max_total_bytes: 512 * 1024 * 1024,
            max_files: 5_000,
            read_only: false,
            allow_executables: false,
        }
    }
}

/// A workspace operation that was refused or failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FilesError {
    #[error("invalid path: {0}")]
    Path(#[from] PathError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("`{path}` is {size} bytes, above the limit of {limit} bytes per file")]
    TooLarge { path: String, size: u64, limit: u64 },
    #[error("the workspace is full: {used} of {limit} bytes used, and `{path}` needs {needed} more")]
    QuotaExceeded { path: String, used: u64, needed: u64, limit: u64 },
    #[error("the workspace already holds {limit} files, the most it may hold")]
    TooManyFiles { limit: usize },
    #[error("the workspace is read only")]
    ReadOnly,
    #[error("`{path}` was refused: {reason}")]
    Refused { path: String, reason: String },
}

/// Files and bytes a workspace holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub files: usize,
    pub bytes: u64,
}

/// A stored file with its detected type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredFile {
    pub entry: Entry,
    pub media: MediaType,
}

/// Usage per view, shared by every clone and scope of one workspace.
type UsageCache = Arc<Mutex<HashMap<String, Usage>>>;

/// A file store with limits. Cheap to clone; clones share the store and the quota.
#[derive(Clone, Debug)]
pub struct Workspace {
    store: Arc<dyn FileStore>,
    limits: Arc<WorkspaceLimits>,
    /// Directory this view is confined to (`scopes/<id>`), if any.
    prefix: Option<WorkspacePath>,
    usage: UsageCache,
    /// Serialises quota check + write, so two writers cannot both fit "the last byte".
    write_gate: Arc<futures::lock::Mutex<()>>,
}

impl Workspace {
    /// A workspace over any store.
    pub fn new(store: Arc<dyn FileStore>, limits: WorkspaceLimits) -> Self {
        Self {
            store,
            limits: Arc::new(limits),
            prefix: None,
            usage: UsageCache::default(),
            write_gate: Arc::new(futures::lock::Mutex::new(())),
        }
    }

    /// A workspace in directory `root` (created when missing).
    pub fn dir(root: impl AsRef<std::path::Path>, limits: WorkspaceLimits) -> Result<Self, FilesError> {
        Ok(Self::new(Arc::new(DirStore::open(root)?), limits))
    }

    /// A workspace held in memory.
    pub fn in_memory(limits: WorkspaceLimits) -> Self {
        Self::new(Arc::new(MemoryStore::new()), limits)
    }

    pub fn limits(&self) -> &WorkspaceLimits {
        &self.limits
    }

    /// A view confined to its own directory, chosen by `scope` (a user id, a
    /// conversation id, a tenant). Two different scopes never see each other's
    /// files, and each has the full quota of [`WorkspaceLimits`].
    ///
    /// The directory name is a hash of the scope, so any text is a valid scope
    /// and none can name a path.
    pub fn scoped(&self, scope: &str) -> Self {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(scope.as_bytes());
        let name = hex(digest.get(..16).unwrap_or_default());
        // 32 hex characters under a fixed directory: always a valid path.
        let prefix = WorkspacePath::parse(&format!("{SCOPES_DIR}/{name}")).ok();
        Self { prefix, ..self.clone() }
    }

    /// Whether this view is confined to a scope.
    pub fn is_scoped(&self) -> bool {
        self.prefix.is_some()
    }

    /// The store path of a path given by a caller of this view.
    fn full(&self, path: &WorkspacePath) -> Result<WorkspacePath, FilesError> {
        match &self.prefix {
            Some(prefix) => Ok(prefix.join(path.as_str())?),
            // Without a scope, the scopes directory is out of reach.
            None if path.components().next() == Some(SCOPES_DIR) => {
                Err(FilesError::Refused { path: path.to_string(), reason: format!("`{SCOPES_DIR}` is reserved") })
            }
            None => Ok(path.clone()),
        }
    }

    /// The path a caller of this view uses for a store path.
    fn visible(&self, full: &WorkspacePath) -> Option<WorkspacePath> {
        match &self.prefix {
            Some(prefix) => full.strip_prefix(prefix).and_then(|rest| WorkspacePath::parse(rest).ok()),
            None if full.components().next() == Some(SCOPES_DIR) => None,
            None => Some(full.clone()),
        }
    }

    fn visible_entry(&self, entry: Entry) -> Option<Entry> {
        let path = self.visible(&entry.path)?;
        Some(Entry { path, ..entry })
    }

    fn usage_key(&self) -> String {
        self.prefix.as_ref().map(ToString::to_string).unwrap_or_default()
    }

    fn cached_usage(&self) -> Option<Usage> {
        self.usage.lock().ok().and_then(|cache| cache.get(&self.usage_key()).copied())
    }

    fn set_usage(&self, usage: Usage) {
        if let Ok(mut cache) = self.usage.lock() {
            cache.insert(self.usage_key(), usage);
        }
    }

    /// Files and bytes this view holds. Counted from the store once, then kept
    /// up to date by the writes and deletes that go through the workspace.
    /// Files changed behind its back are picked up by [`Workspace::recount`].
    pub async fn usage(&self) -> Result<Usage, FilesError> {
        match self.cached_usage() {
            Some(usage) => Ok(usage),
            None => self.recount().await,
        }
    }

    /// Count the files of this view again from the store.
    pub async fn recount(&self) -> Result<Usage, FilesError> {
        // One more than the limit is enough to know the workspace is over it.
        let listing = self.store.list(self.prefix.as_ref(), self.limits.max_files.saturating_add(1)).await;
        let listing = match listing {
            Ok(listing) => listing,
            // A scope nobody wrote to yet has no directory.
            Err(StoreError::NotFound(_)) => Listing::default(),
            Err(e) => return Err(e.into()),
        };
        let entries = listing.entries.iter().filter(|entry| self.visible(&entry.path).is_some());
        let usage = entries.fold(Usage::default(), |total, entry| Usage {
            files: total.files + 1,
            bytes: total.bytes.saturating_add(entry.size),
        });
        self.set_usage(usage);
        Ok(usage)
    }

    /// The file at `path`, if it exists.
    pub async fn stat(&self, path: &WorkspacePath) -> Result<Option<Entry>, FilesError> {
        let full = self.full(path)?;
        Ok(self.store.stat(&full).await?.map(|entry| Entry { path: path.clone(), ..entry }))
    }

    /// The file at `path` with its type, detected from its first bytes.
    pub async fn inspect(&self, path: &WorkspacePath) -> Result<StoredFile, FilesError> {
        let full = self.full(path)?;
        let entry = self.store.stat(&full).await?.ok_or_else(|| StoreError::NotFound(path.to_string()))?;
        let head = self.store.read(&full, 0, u64::try_from(SNIFF_BYTES).unwrap_or(u64::MAX)).await?;
        let media = sniff(&head, path.extension());
        Ok(StoredFile { entry: Entry { path: path.clone(), ..entry }, media })
    }

    /// Up to `max_bytes` of the file, from byte `offset`.
    pub async fn read(&self, path: &WorkspacePath, offset: u64, max_bytes: u64) -> Result<Vec<u8>, FilesError> {
        let full = self.full(path)?;
        Ok(self.store.read(&full, offset, max_bytes).await.map_err(|e| rename_error(e, path))?)
    }

    /// The whole file, failing with [`FilesError::TooLarge`] when it is larger than `max_bytes`.
    pub async fn read_all(&self, path: &WorkspacePath, max_bytes: u64) -> Result<Vec<u8>, FilesError> {
        let too_large = |size| FilesError::TooLarge { path: path.to_string(), size, limit: max_bytes };
        let entry = self.stat(path).await?.ok_or_else(|| StoreError::NotFound(path.to_string()))?;
        if entry.size > max_bytes {
            return Err(too_large(entry.size));
        }
        // One byte more than allowed shows a file that grew after the size check.
        let bytes = self.read(path, 0, max_bytes.saturating_add(1)).await?;
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if size > max_bytes { Err(too_large(size)) } else { Ok(bytes) }
    }

    /// SHA-256 of the file, as lowercase hex.
    pub async fn sha256(&self, path: &WorkspacePath) -> Result<String, FilesError> {
        let full = self.full(path)?;
        let digest = self.store.sha256(&full).await.map_err(|e| rename_error(e, path))?;
        Ok(hex(&digest))
    }

    /// Store `bytes` at `path`.
    ///
    /// Refused when the workspace is read only, the file is larger than
    /// [`WorkspaceLimits::max_file_bytes`], the content is a program or a
    /// script (unless [`WorkspaceLimits::allow_executables`]), or the file
    /// would take the workspace over its quota. The type is always judged from
    /// the bytes, never from the name.
    pub async fn write(&self, path: &WorkspacePath, bytes: Vec<u8>, overwrite: bool) -> Result<StoredFile, FilesError> {
        if self.limits.read_only {
            return Err(FilesError::ReadOnly);
        }
        let full = self.full(path)?;
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if size > self.limits.max_file_bytes {
            return Err(FilesError::TooLarge { path: path.to_string(), size, limit: self.limits.max_file_bytes });
        }
        let media = sniff(&bytes, path.extension());
        if media.class == MediaClass::Executable && !self.limits.allow_executables {
            return Err(FilesError::Refused {
                path: path.to_string(),
                reason: format!("the content is executable ({})", media.mime),
            });
        }
        let _gate = self.write_gate.lock().await;
        let usage = self.usage().await?;
        let replaced = self.store.stat(&full).await.map_err(|e| rename_error(e, path))?;
        if replaced.is_some() && !overwrite {
            return Err(StoreError::AlreadyExists(path.to_string()).into());
        }
        let replaced_bytes = replaced.as_ref().map_or(0, |entry| entry.size);
        let bytes_after = usage.bytes.saturating_sub(replaced_bytes).saturating_add(size);
        if bytes_after > self.limits.max_total_bytes {
            return Err(FilesError::QuotaExceeded {
                path: path.to_string(),
                used: usage.bytes,
                needed: size.saturating_sub(replaced_bytes),
                limit: self.limits.max_total_bytes,
            });
        }
        if replaced.is_none() && usage.files >= self.limits.max_files {
            return Err(FilesError::TooManyFiles { limit: self.limits.max_files });
        }
        let entry = self.store.write(&full, bytes, overwrite).await.map_err(|e| rename_error(e, path))?;
        self.set_usage(Usage { files: usage.files + usize::from(replaced.is_none()), bytes: bytes_after });
        Ok(StoredFile { entry: Entry { path: path.clone(), ..entry }, media })
    }

    /// Remove the file at `path`.
    pub async fn delete(&self, path: &WorkspacePath) -> Result<(), FilesError> {
        if self.limits.read_only {
            return Err(FilesError::ReadOnly);
        }
        let full = self.full(path)?;
        let _gate = self.write_gate.lock().await;
        let removed = self.store.stat(&full).await.map_err(|e| rename_error(e, path))?;
        let removed = removed.ok_or_else(|| StoreError::NotFound(path.to_string()))?;
        self.store.delete(&full).await.map_err(|e| rename_error(e, path))?;
        if let Some(usage) = self.cached_usage() {
            self.set_usage(Usage {
                files: usage.files.saturating_sub(1),
                bytes: usage.bytes.saturating_sub(removed.size),
            });
        }
        Ok(())
    }

    /// Files below `dir` (the whole view when `None`), at most `max_entries`.
    pub async fn list(&self, dir: Option<&WorkspacePath>, max_entries: usize) -> Result<Listing, FilesError> {
        let full = match (dir, &self.prefix) {
            (Some(dir), _) => Some(self.full(dir)?),
            (None, Some(prefix)) => Some(prefix.clone()),
            (None, None) => None,
        };
        let listing = match self.store.list(full.as_ref(), max_entries).await {
            Ok(listing) => listing,
            // The root of a scope that holds nothing yet.
            Err(StoreError::NotFound(_)) if dir.is_none() => Listing::default(),
            Err(e) => return Err(dir.map_or(e.clone(), |dir| rename_error(e, dir)).into()),
        };
        let entries = listing.entries.into_iter().filter_map(|entry| self.visible_entry(entry)).collect();
        Ok(Listing { entries, truncated: listing.truncated })
    }

    /// The graphrt [`File`] value naming a stored file. Its `reference` is
    /// `workspace:<store path>`, which [`crate::WorkspaceResolver`] reads back.
    pub fn file_value(&self, stored: &StoredFile) -> File {
        let path = &stored.entry.path;
        let store_path = self.full(path).map(|full| full.to_string()).unwrap_or_else(|_| path.to_string());
        File {
            marker: true,
            id: None,
            name: path.file_name().to_owned(),
            kind: stored.media.file_kind(),
            transfer: FileTransfer::ToolFile,
            url: None,
            reference: Some(format!("{REFERENCE_SCHEME}{store_path}")),
            mime_type: Some(stored.media.mime.to_owned()),
            size: Some(stored.entry.size),
            extension: path.extension().map(str::to_ascii_lowercase),
        }
    }

    /// The path, inside this view, that a `workspace:` reference names. `None`
    /// for other references and for files outside this view (another scope).
    pub fn path_of_reference(&self, reference: &str) -> Option<WorkspacePath> {
        let full = WorkspacePath::parse(reference.strip_prefix(REFERENCE_SCHEME)?).ok()?;
        self.visible(&full)
    }

    /// The view and the path a `workspace:` reference names, in whatever scope
    /// it lies. Only an unscoped workspace looks into scopes; a scoped view
    /// answers for its own files, like [`Workspace::path_of_reference`].
    ///
    /// For hosts that resolve files on behalf of every user of a workspace
    /// (the file values themselves are created by the tools, inside a scope,
    /// and cannot be forged by untrusted JSON).
    pub fn locate_reference(&self, reference: &str) -> Option<(Workspace, WorkspacePath)> {
        if self.prefix.is_some() {
            return self.path_of_reference(reference).map(|path| (self.clone(), path));
        }
        let full = WorkspacePath::parse(reference.strip_prefix(REFERENCE_SCHEME)?).ok()?;
        let names: Vec<&str> = full.components().take(2).collect();
        let Some((&SCOPES_DIR, scope_dir)) = names.split_first().map(|(first, rest)| (first, rest.first())) else {
            return Some((self.clone(), full));
        };
        let prefix = WorkspacePath::parse(&format!("{SCOPES_DIR}/{}", scope_dir?)).ok()?;
        let path = WorkspacePath::parse(full.strip_prefix(&prefix)?).ok()?;
        Some((Self { prefix: Some(prefix), ..self.clone() }, path))
    }
}

/// Errors name the path the caller used, not the store path behind a scope.
fn rename_error(error: StoreError, path: &WorkspacePath) -> StoreError {
    let shown = path.to_string();
    match error {
        StoreError::NotFound(_) => StoreError::NotFound(shown),
        StoreError::AlreadyExists(_) => StoreError::AlreadyExists(shown),
        StoreError::NotAFile(_) => StoreError::NotAFile(shown),
        StoreError::Denied { reason, .. } => StoreError::Denied { path: shown, reason },
        StoreError::Io { message, .. } => StoreError::Io { path: shown, message },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(raw: &str) -> WorkspacePath {
        WorkspacePath::parse(raw).unwrap()
    }

    fn small() -> WorkspaceLimits {
        WorkspaceLimits { max_file_bytes: 100, max_total_bytes: 250, max_files: 3, ..WorkspaceLimits::default() }
    }

    #[tokio::test]
    async fn write_read_list_delete() {
        let ws = Workspace::in_memory(WorkspaceLimits::default());
        let stored = ws.write(&path("notes/a.md"), b"# hi".to_vec(), false).await.unwrap();
        assert_eq!((stored.entry.size, stored.media.mime), (4, "text/markdown"));
        assert_eq!(ws.read(&path("notes/a.md"), 2, 10).await.unwrap(), b"hi");
        assert_eq!(ws.read_all(&path("notes/a.md"), 4).await.unwrap(), b"# hi");
        assert!(matches!(ws.read_all(&path("notes/a.md"), 3).await, Err(FilesError::TooLarge { .. })));
        assert_eq!(ws.list(None, 10).await.unwrap().entries.len(), 1);
        assert_eq!(ws.usage().await.unwrap(), Usage { files: 1, bytes: 4 });
        ws.delete(&path("notes/a.md")).await.unwrap();
        assert_eq!(ws.usage().await.unwrap(), Usage::default());
        assert!(matches!(ws.delete(&path("notes/a.md")).await, Err(FilesError::Store(StoreError::NotFound(_)))));
    }

    #[tokio::test]
    async fn size_quota_and_count_limits_hold() {
        let ws = Workspace::in_memory(small());
        assert!(matches!(ws.write(&path("big"), vec![b'a'; 101], false).await, Err(FilesError::TooLarge { .. })));
        ws.write(&path("a"), vec![b'a'; 100], false).await.unwrap();
        ws.write(&path("b"), vec![b'a'; 100], false).await.unwrap();
        // 200 used: 60 more would pass 250.
        let full = ws.write(&path("c"), vec![b'a'; 60], false).await.unwrap_err();
        assert!(matches!(full, FilesError::QuotaExceeded { used: 200, needed: 60, limit: 250, .. }), "{full:?}");
        // Replacing a file only counts the difference.
        ws.write(&path("a"), vec![b'a'; 40], true).await.unwrap();
        ws.write(&path("c"), vec![b'a'; 60], false).await.unwrap();
        assert_eq!(ws.usage().await.unwrap(), Usage { files: 3, bytes: 200 });
        assert!(matches!(ws.write(&path("d"), vec![b'a'], false).await, Err(FilesError::TooManyFiles { limit: 3 })));
        // Overwriting an existing file is not a fourth file.
        ws.write(&path("c"), vec![b'a'; 10], true).await.unwrap();
        assert!(matches!(
            ws.write(&path("c"), vec![b'a'], false).await,
            Err(FilesError::Store(StoreError::AlreadyExists(_)))
        ));
    }

    #[tokio::test]
    async fn concurrent_writers_cannot_pass_the_quota_together() {
        let ws = Workspace::in_memory(WorkspaceLimits { max_total_bytes: 250, ..WorkspaceLimits::default() });
        let writes = (0..10).map(|i| {
            let ws = ws.clone();
            async move { ws.write(&path(&format!("f{i}")), vec![b'a'; 100], false).await.is_ok() }
        });
        let accepted = futures::future::join_all(writes).await.into_iter().filter(|ok| *ok).count();
        assert_eq!(accepted, 2);
        assert_eq!(ws.recount().await.unwrap(), Usage { files: 2, bytes: 200 });
    }

    #[tokio::test]
    async fn the_type_comes_from_the_bytes_and_programs_are_refused() {
        let ws = Workspace::in_memory(WorkspaceLimits::default());
        // A Windows program named like a picture.
        let refused = ws.write(&path("photo.png"), b"MZ\x90\0\x03\0\0\0".to_vec(), false).await.unwrap_err();
        assert!(matches!(refused, FilesError::Refused { .. }), "{refused:?}");
        assert!(ws.write(&path("run.txt"), b"#!/bin/sh\nrm -rf ~".to_vec(), false).await.is_err());
        // A picture named like text is stored and reported as a picture.
        let stored = ws.write(&path("notes.txt"), b"\x89PNG\r\n\x1a\n".to_vec(), false).await.unwrap();
        assert_eq!(stored.media.mime, "image/png");
        assert_eq!(ws.file_value(&stored).mime_type.as_deref(), Some("image/png"));
        // HTML in a `.txt` is stored, and reported as HTML so no host shows it inline.
        let html = ws.write(&path("page.txt"), b"<html><script>x()</script>".to_vec(), false).await.unwrap();
        assert_eq!(html.media.mime, "text/html");
        assert!(!html.media.is_inline_safe());
        let allowing = Workspace::in_memory(WorkspaceLimits { allow_executables: true, ..WorkspaceLimits::default() });
        assert!(allowing.write(&path("tool.sh"), b"#!/bin/sh\n".to_vec(), false).await.is_ok());
    }

    #[tokio::test]
    async fn read_only_refuses_writes_and_deletes() {
        let store: Arc<dyn FileStore> = Arc::new(MemoryStore::new());
        Workspace::new(Arc::clone(&store), WorkspaceLimits::default())
            .write(&path("a.txt"), b"x".to_vec(), false)
            .await
            .unwrap();
        let ws = Workspace::new(store, WorkspaceLimits { read_only: true, ..WorkspaceLimits::default() });
        assert_eq!(ws.read(&path("a.txt"), 0, 10).await.unwrap(), b"x");
        assert_eq!(ws.write(&path("b.txt"), b"x".to_vec(), false).await.unwrap_err(), FilesError::ReadOnly);
        assert_eq!(ws.delete(&path("a.txt")).await.unwrap_err(), FilesError::ReadOnly);
    }

    #[tokio::test]
    async fn scopes_are_invisible_to_each_other_and_to_the_root() {
        let root = Workspace::in_memory(WorkspaceLimits::default());
        let (alice, bob) = (root.scoped("alice"), root.scoped("bob"));
        let secret = alice.write(&path("secret.txt"), b"alice only".to_vec(), false).await.unwrap();
        root.write(&path("shared.txt"), b"root".to_vec(), false).await.unwrap();

        assert_eq!(alice.read(&path("secret.txt"), 0, 100).await.unwrap(), b"alice only");
        assert!(matches!(bob.read(&path("secret.txt"), 0, 100).await, Err(FilesError::Store(StoreError::NotFound(_)))));
        assert!(bob.list(None, 100).await.unwrap().entries.is_empty());
        assert_eq!(alice.list(None, 100).await.unwrap().entries[0].path, path("secret.txt"));
        // Bob cannot name Alice's directory, and neither can the unscoped view.
        let alice_dir = alice.prefix.clone().unwrap();
        let through_bob = bob.read(&alice_dir.join("secret.txt").unwrap(), 0, 100).await;
        assert!(matches!(through_bob, Err(FilesError::Store(StoreError::NotFound(_)))), "{through_bob:?}");
        let through_root = root.read(&alice_dir.join("secret.txt").unwrap(), 0, 100).await;
        assert!(matches!(through_root, Err(FilesError::Refused { .. })), "{through_root:?}");
        assert_eq!(root.list(None, 100).await.unwrap().entries.len(), 1);
        assert!(root.write(&path("scopes/x/planted.txt"), b"x".to_vec(), false).await.is_err());
        // The same scope text always maps to the same files; a path-like scope is just text.
        assert_eq!(root.scoped("alice").read(&path("secret.txt"), 0, 100).await.unwrap(), b"alice only");
        let odd = root.scoped("../../etc");
        odd.write(&path("x.txt"), b"x".to_vec(), false).await.unwrap();
        assert!(odd.prefix.clone().unwrap().as_str().starts_with("scopes/"));

        // A file reference of one scope does not resolve in another.
        let reference = alice.file_value(&secret).reference.unwrap();
        assert_eq!(alice.path_of_reference(&reference), Some(path("secret.txt")));
        assert_eq!(bob.path_of_reference(&reference), None);
        assert_eq!(root.path_of_reference(&reference), None);
        assert_eq!(alice.path_of_reference("workspace:../x"), None);
        assert_eq!(alice.path_of_reference("other:secret.txt"), None);
        // A host resolving for every user finds the file through the unscoped workspace.
        let (view, found) = root.locate_reference(&reference).unwrap();
        assert_eq!(view.read(&found, 0, 100).await.unwrap(), b"alice only");
        assert!(bob.locate_reference(&reference).is_none());
        assert!(root.locate_reference("workspace:scopes/x").is_none());
        assert_eq!(root.locate_reference("workspace:shared.txt").unwrap().1, path("shared.txt"));
    }

    #[tokio::test]
    async fn every_scope_has_its_own_quota() {
        let root = Workspace::in_memory(small());
        let (alice, bob) = (root.scoped("alice"), root.scoped("bob"));
        for name in ["a", "b"] {
            alice.write(&path(name), vec![b'a'; 100], false).await.unwrap();
        }
        assert!(alice.write(&path("c"), vec![b'a'; 100], false).await.is_err());
        // Alice being full does not stop Bob.
        bob.write(&path("a"), vec![b'a'; 100], false).await.unwrap();
        assert_eq!(bob.usage().await.unwrap(), Usage { files: 1, bytes: 100 });
        assert_eq!(alice.recount().await.unwrap(), Usage { files: 2, bytes: 200 });
    }
}
