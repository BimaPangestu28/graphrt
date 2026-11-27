//! A small JSON file that several servers on one store directory share: the
//! paired devices, the push subscriptions.
//!
//! The value is kept in memory and read again when the file changed, so a
//! change one server makes (a revoked device) reaches the others. Every change
//! replaces the file atomically (temporary file, sync, rename) and owner-only.
//! Without a path the value lives in memory only.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use graphrt_core::private_fs;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// The coarsest modification time of a file system this trusts: a file
/// changed more recently than this is read again even when its time did not move.
const FILE_TIME_GRANULARITY: Duration = Duration::from_secs(2);

#[derive(Debug)]
struct Inner<T> {
    value: T,
    /// Modification time of the file when it was last read or written.
    file_time: Option<SystemTime>,
}

/// A value kept in a JSON file (or in memory without one).
#[derive(Debug)]
pub(crate) struct SharedJsonFile<T> {
    path: Option<PathBuf>,
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
        let file = Self { path, inner: Mutex::new(Inner { value: T::default(), file_time: None }) };
        {
            let mut inner = file.lock();
            file.reload(&mut inner)?;
        }
        Ok(file)
    }

    /// Where the value is kept, when it is kept on disk.
    pub(crate) fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner<T>> {
        // A panic while the lock was held cannot leave the value half changed:
        // every change is made on a copy first.
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Read the file again when it changed since it was last read or written.
    fn reload(&self, inner: &mut Inner<T>) -> Result<(), String> {
        let Some(path) = &self.path else { return Ok(()) };
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
    fn persist(&self, value: &T) -> Result<Option<SystemTime>, String> {
        let Some(path) = &self.path else { return Ok(None) };
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

    /// Look at the current value.
    ///
    /// @throws String - When the file changed and cannot be read.
    pub(crate) fn read<R>(&self, look: impl FnOnce(&T) -> R) -> Result<R, String> {
        let mut inner = self.lock();
        self.reload(&mut inner)?;
        Ok(look(&inner.value))
    }

    /// Change the value and write it. `change` works on a copy, which becomes
    /// the value only once it is on disk: a failed write changes nothing.
    ///
    /// @throws E - What `change` refuses with, or the I/O error as a `String`.
    pub(crate) fn update<R, E: From<String>>(&self, change: impl FnOnce(&mut T) -> Result<R, E>) -> Result<R, E> {
        let mut inner = self.lock();
        self.reload(&mut inner)?;
        let mut next = inner.value.clone();
        let result = change(&mut next)?;
        let file_time = self.persist(&next)?;
        inner.value = next;
        if self.path.is_some() {
            inner.file_time = file_time;
        }
        Ok(result)
    }

    /// Change the value in memory, and write it too with `write`. A change
    /// that is not written is lost when another server changes the file: use
    /// it for what is not worth a write each time (when something was last used).
    ///
    /// @throws String - When the file changed and cannot be read. A failed write is reported, not returned.
    pub(crate) fn touch<R>(&self, write: bool, change: impl FnOnce(&mut T) -> R) -> Result<R, String> {
        let mut inner = self.lock();
        self.reload(&mut inner)?;
        let result = change(&mut inner.value);
        if write {
            match self.persist(&inner.value) {
                Ok(file_time) if self.path.is_some() => inner.file_time = file_time,
                Ok(_) => {}
                Err(error) => eprintln!("warning: {error}"),
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

    #[test]
    fn a_file_of_another_shape_is_refused() {
        let path = temp_file("bad");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(SharedJsonFile::<Vec<u32>>::open(Some(path.clone())).is_err());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
