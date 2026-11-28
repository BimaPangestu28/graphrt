//! A small JSON file that several servers on one store directory share: the
//! paired devices, the push subscriptions.
//!
//! The value is kept in memory and read again when the file changed, so a
//! change one server makes (a revoked device) reaches the others. Every change
//! replaces the file atomically (temporary file, sync, rename) and owner-only.
//! Without a path the value lives in memory only. With a shared object store
//! (`--store s3://...`) the value is one object, written with a compare and
//! swap on its ETag and checked for changes at most once a second.

use std::path::{Path, PathBuf};
#[cfg(feature = "store-s3")]
use std::sync::Arc;
use std::sync::Mutex;
#[cfg(feature = "store-s3")]
use std::time::Instant;
use std::time::{Duration, SystemTime};

use graphrt_core::private_fs;
use serde::Serialize;
use serde::de::DeserializeOwned;

#[cfg(feature = "store-s3")]
use crate::store::objects::{Condition, ObjectError, Objects, Read, block_on};

/// The coarsest modification time of a file system this trusts: a file
/// changed more recently than this is read again even when its time did not move.
const FILE_TIME_GRANULARITY: Duration = Duration::from_secs(2);

#[derive(Debug)]
struct Inner<T> {
    value: T,
    /// Modification time of the file when it was last read or written.
    file_time: Option<SystemTime>,
    /// ETag of the object when it was last read or written.
    #[cfg(feature = "store-s3")]
    etag: Option<String>,
    /// When the object was last compared with the store.
    #[cfg(feature = "store-s3")]
    checked: Option<Instant>,
}

/// Where the value is kept.
#[derive(Debug)]
enum Place {
    Memory,
    File(PathBuf),
    /// A shared object (`--store s3://...`), read again at most every [`OBJECT_RECHECK`].
    #[cfg(feature = "store-s3")]
    Object {
        objects: Arc<dyn Objects>,
        key: String,
    },
}

/// How long a value read from a shared object is used before asking the store
/// whether it changed (a conditional read, answered `304` while unchanged).
#[cfg(feature = "store-s3")]
const OBJECT_RECHECK: Duration = Duration::from_secs(1);
/// Writes of one change tried before giving up when other servers keep writing.
const MAX_OBJECT_ATTEMPTS: usize = 32;

/// A value kept in a JSON file, a shared object, or in memory.
#[derive(Debug)]
pub(crate) struct SharedJsonFile<T> {
    place: Place,
    inner: Mutex<Inner<T>>,
}

impl<T> SharedJsonFile<T>
where
    T: Default + Clone + Serialize + DeserializeOwned,
{
    /// The value of the file at `path` (the default when it does not exist
    /// yet), or a value kept in memory without a path.
    ///
    /// @throws String - When the file cannot be read or is not such a value.
    pub(crate) fn open(path: Option<PathBuf>) -> Result<Self, String> {
        Self::at(path.map_or(Place::Memory, Place::File))
    }

    /// The value of object `key` of `objects`, shared by every server of the store.
    #[cfg(feature = "store-s3")]
    ///
    /// @throws String - When the object cannot be read or is not such a value.
    pub(crate) fn open_object(objects: Arc<dyn Objects>, key: &str) -> Result<Self, String> {
        Self::at(Place::Object { objects, key: key.to_owned() })
    }

    fn at(place: Place) -> Result<Self, String> {
        let inner = Inner {
            value: T::default(),
            file_time: None,
            #[cfg(feature = "store-s3")]
            etag: None,
            #[cfg(feature = "store-s3")]
            checked: None,
        };
        let file = Self { place, inner: Mutex::new(inner) };
        {
            let mut inner = file.lock();
            file.reload(&mut inner, true)?;
        }
        Ok(file)
    }

    /// Where the value is kept, for the startup log; `None` in memory.
    pub(crate) fn location(&self) -> Option<String> {
        match &self.place {
            Place::Memory => None,
            Place::File(path) => Some(path.display().to_string()),
            #[cfg(feature = "store-s3")]
            Place::Object { objects, key } => Some(format!("{}/{key}", objects.describe())),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner<T>> {
        // A panic while the lock was held cannot leave the value half changed:
        // every change is made on a copy first.
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Read the file or object again when it changed since it was last read or
    /// written (`force`: ask the object store even within [`OBJECT_RECHECK`]).
    fn reload(&self, inner: &mut Inner<T>, force: bool) -> Result<(), String> {
        #[cfg(not(feature = "store-s3"))]
        let _ = force;
        match &self.place {
            Place::Memory => Ok(()),
            Place::File(path) => Self::reload_file(path, inner),
            #[cfg(feature = "store-s3")]
            Place::Object { objects, key } => {
                if !force && inner.checked.is_some_and(|checked| checked.elapsed() < OBJECT_RECHECK) {
                    return Ok(());
                }
                let known = inner.etag.clone();
                let read = block_on(objects.read(key, known.as_deref())).map_err(|e| format!("{key}: {e}"))?;
                match read {
                    Read::Unchanged => {}
                    Read::Missing => {
                        inner.value = T::default();
                        inner.etag = None;
                    }
                    Read::Object(object) => {
                        inner.value =
                            serde_json::from_slice(&object.bytes).map_err(|e| format!("{key} cannot be read: {e}"))?;
                        inner.etag = Some(object.etag);
                    }
                }
                inner.checked = Some(Instant::now());
                Ok(())
            }
        }
    }

    fn reload_file(path: &Path, inner: &mut Inner<T>) -> Result<(), String> {
        let modified = match std::fs::metadata(path) {
            Ok(metadata) => metadata.modified().ok(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if inner.file_time.is_some() {
                    // The file was removed: what it held with it.
                    inner.value = T::default();
                    inner.file_time = None;
                }
                return Ok(());
            }
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        let settled = modified.and_then(|time| time.elapsed().ok()).is_some_and(|age| age >= FILE_TIME_GRANULARITY);
        if settled && modified == inner.file_time {
            return Ok(());
        }
        let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        inner.value = serde_json::from_slice(&bytes).map_err(|e| format!("{} cannot be read: {e}", path.display()))?;
        inner.file_time = modified;
        Ok(())
    }

    /// Replace the file with `value`.
    fn persist_file(path: &Path, value: &T) -> Result<Option<SystemTime>, String> {
        let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        let result = (|| {
            use std::io::Write as _;
            let mut file =
                private_fs::create_file(&tmp).map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
            file.write_all(&bytes).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
            file.sync_all().map_err(|e| format!("cannot sync {}: {e}", tmp.display()))?;
            std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
        })();
        if result.is_err() {
            let _cleanup = std::fs::remove_file(&tmp);
        }
        result?;
        Ok(std::fs::metadata(path).and_then(|metadata| metadata.modified()).ok())
    }

    /// Write `value` to the object while it still has the ETag `inner` read.
    #[cfg(feature = "store-s3")]
    /// `Ok(false)`: another server wrote first.
    fn persist_object(objects: &dyn Objects, key: &str, inner: &mut Inner<T>, value: &T) -> Result<bool, String> {
        let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
        let condition = inner.etag.clone().map_or(Condition::Absent, Condition::Matches);
        match block_on(objects.write(key, bytes, condition)) {
            Ok(etag) => {
                inner.etag = Some(etag);
                inner.checked = Some(Instant::now());
                Ok(true)
            }
            Err(ObjectError::Precondition) => Ok(false),
            Err(error) => Err(format!("{key}: {error}")),
        }
    }

    /// Look at the current value.
    ///
    /// @throws String - When the file changed and cannot be read.
    pub(crate) fn read<R>(&self, look: impl FnOnce(&T) -> R) -> Result<R, String> {
        let mut inner = self.lock();
        self.reload(&mut inner, false)?;
        Ok(look(&inner.value))
    }

    /// Change the value and write it. `change` works on a copy, which becomes
    /// the value only once it is stored: a failed write changes nothing. With
    /// a shared object, `change` runs again on the newer value when another
    /// server wrote in between.
    ///
    /// @throws E - What `change` refuses with, or the I/O error as a `String`.
    pub(crate) fn update<R, E: From<String>>(&self, mut change: impl FnMut(&mut T) -> Result<R, E>) -> Result<R, E> {
        let mut inner = self.lock();
        for _ in 0..MAX_OBJECT_ATTEMPTS {
            self.reload(&mut inner, true)?;
            let mut next = inner.value.clone();
            let result = change(&mut next)?;
            match &self.place {
                Place::Memory => {}
                Place::File(path) => inner.file_time = Self::persist_file(path, &next)?,
                #[cfg(feature = "store-s3")]
                Place::Object { objects, key } => {
                    if !Self::persist_object(objects.as_ref(), key, &mut inner, &next)? {
                        continue;
                    }
                }
            }
            inner.value = next;
            return Ok(result);
        }
        Err(E::from("the shared store kept changing while it was written: try again".to_owned()))
    }

    /// Change the value in memory, and write it too with `write`. A change
    /// that is not written is lost when another server changes the file: use
    /// it for what is not worth a write each time (when something was last used).
    ///
    /// @throws String - When the file changed and cannot be read. A failed write is reported, not returned.
    pub(crate) fn touch<R>(&self, write: bool, change: impl FnOnce(&mut T) -> R) -> Result<R, String> {
        let mut inner = self.lock();
        self.reload(&mut inner, false)?;
        let result = change(&mut inner.value);
        if write {
            let value = inner.value.clone();
            let written = match &self.place {
                Place::Memory => Ok(()),
                Place::File(path) => Self::persist_file(path, &value).map(|time| inner.file_time = time),
                // Losing a race only loses a "last used" time.
                #[cfg(feature = "store-s3")]
                Place::Object { objects, key } => {
                    Self::persist_object(objects.as_ref(), key, &mut inner, &value).map(|_| ())
                }
            };
            if let Err(error) = written {
                eprintln!("warning: {error}");
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn temp_file(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("graphrt-shared-file-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("list.json")
    }

    #[test]
    fn a_change_reaches_the_file_and_other_servers() {
        let path = temp_file("shared");
        let first: SharedJsonFile<Vec<String>> = SharedJsonFile::open(Some(path.clone())).unwrap();
        let second: SharedJsonFile<Vec<String>> = SharedJsonFile::open(Some(path.clone())).unwrap();
        first
            .update(|list| {
                list.push("a".to_owned());
                Ok::<_, String>(())
            })
            .unwrap();
        assert_eq!(second.read(Clone::clone).unwrap(), ["a"]);
        second
            .update(|list| {
                list.clear();
                Ok::<_, String>(())
            })
            .unwrap();
        assert!(first.read(Vec::is_empty).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_refused_change_changes_nothing() {
        let file: SharedJsonFile<Vec<u32>> = SharedJsonFile::open(None).unwrap();
        file.update(|list| {
            list.push(1);
            Ok::<_, String>(())
        })
        .unwrap();
        let refused: Result<(), String> = file.update(|list| {
            list.push(2);
            Err("no".to_owned())
        });
        assert_eq!(refused, Err("no".to_owned()));
        assert_eq!(file.read(Clone::clone).unwrap(), [1]);
    }

    #[cfg(feature = "store-s3")]
    #[test]
    fn servers_on_one_shared_object_see_each_other_and_lose_no_change() {
        let objects: Arc<dyn Objects> = Arc::new(crate::store::objects::MemoryObjects::default());
        let first: SharedJsonFile<Vec<String>> =
            SharedJsonFile::open_object(Arc::clone(&objects), "devices.json").unwrap();
        let second: SharedJsonFile<Vec<String>> = SharedJsonFile::open_object(objects, "devices.json").unwrap();
        first
            .update(|list| {
                list.push("a".to_owned());
                Ok::<_, String>(())
            })
            .unwrap();
        // The second server's copy is stale: its write retries on the newer value.
        second
            .update(|list| {
                list.push("b".to_owned());
                Ok::<_, String>(())
            })
            .unwrap();
        first
            .update(|list| {
                list.push("c".to_owned());
                Ok::<_, String>(())
            })
            .unwrap();
        assert_eq!(first.read(Clone::clone).unwrap(), ["a", "b", "c"]);
        assert!(first.location().unwrap().ends_with("/devices.json"));
    }

    #[test]
    fn a_file_of_another_shape_is_refused() {
        let path = temp_file("bad");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(SharedJsonFile::<Vec<u32>>::open(Some(path.clone())).is_err());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
