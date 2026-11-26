//! In-memory [`FileStore`], for tests and hosts without a file system.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use futures::future::BoxFuture;

use crate::path::WorkspacePath;
use crate::store::{Entry, FileStore, Listing, StoreError};

/// Keeps every file in memory. Cloning shares the same files.
#[derive(Clone, Debug, Default)]
pub struct MemoryStore {
    files: Arc<Mutex<BTreeMap<WorkspacePath, Arc<Vec<u8>>>>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// A poisoned lock still holds consistent data: every write replaces one entry.
    fn files(&self) -> MutexGuard<'_, BTreeMap<WorkspacePath, Arc<Vec<u8>>>> {
        self.files.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn entry(path: &WorkspacePath, bytes: &[u8]) -> Entry {
        Entry { path: path.clone(), size: u64::try_from(bytes.len()).unwrap_or(u64::MAX), modified_ms: None }
    }
}

impl FileStore for MemoryStore {
    fn stat<'a>(&'a self, path: &'a WorkspacePath) -> BoxFuture<'a, Result<Option<Entry>, StoreError>> {
        let found = self.files().get(path).map(|bytes| Self::entry(path, bytes));
        Box::pin(futures::future::ready(Ok(found)))
    }

    fn read<'a>(
        &'a self,
        path: &'a WorkspacePath,
        offset: u64,
        max_bytes: u64,
    ) -> BoxFuture<'a, Result<Vec<u8>, StoreError>> {
        let result = match self.files().get(path) {
            None => Err(StoreError::NotFound(path.to_string())),
            Some(bytes) => {
                let start = usize::try_from(offset).unwrap_or(usize::MAX).min(bytes.len());
                let wanted = usize::try_from(max_bytes).unwrap_or(usize::MAX);
                let end = start.saturating_add(wanted).min(bytes.len());
                Ok(bytes.get(start..end).unwrap_or_default().to_vec())
            }
        };
        Box::pin(futures::future::ready(result))
    }

    fn write<'a>(
        &'a self,
        path: &'a WorkspacePath,
        bytes: Vec<u8>,
        overwrite: bool,
    ) -> BoxFuture<'a, Result<Entry, StoreError>> {
        let mut files = self.files();
        let result = if !overwrite && files.contains_key(path) {
            Err(StoreError::AlreadyExists(path.to_string()))
        } else if files.keys().any(|other| other.starts_with(path) && other != path) {
            // A directory of that name exists.
            Err(StoreError::NotAFile(path.to_string()))
        } else {
            let entry = Self::entry(path, &bytes);
            files.insert(path.clone(), Arc::new(bytes));
            Ok(entry)
        };
        Box::pin(futures::future::ready(result))
    }

    fn delete<'a>(&'a self, path: &'a WorkspacePath) -> BoxFuture<'a, Result<(), StoreError>> {
        let result = match self.files().remove(path) {
            Some(_) => Ok(()),
            None => Err(StoreError::NotFound(path.to_string())),
        };
        Box::pin(futures::future::ready(result))
    }

    fn list<'a>(
        &'a self,
        dir: Option<&'a WorkspacePath>,
        max_entries: usize,
    ) -> BoxFuture<'a, Result<Listing, StoreError>> {
        let files = self.files();
        let mut listing = Listing::default();
        for (path, bytes) in
            files.iter().filter(|(path, _)| dir.is_none_or(|dir| path.starts_with(dir) && *path != dir))
        {
            if listing.entries.len() >= max_entries {
                listing.truncated = true;
                break;
            }
            listing.entries.push(Self::entry(path, bytes));
        }
        Box::pin(futures::future::ready(Ok(listing)))
    }
}
