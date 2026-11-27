//! What the sync remembers between runs: the version and content hash of
//! every synced item (to skip unchanged ones and to find deleted ones), the
//! status of the last sync of each source, and a lock so two processes never
//! sync the same source at once.
//!
//! [`MemorySyncState`] keeps it in the process. [`SqliteSyncState`] (feature
//! `sqlite`) keeps it in one SQLite file: every process that opens the same
//! file (the desktop app and `graphrt serve` on the same knowledge base)
//! shares the items and the lock.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// One synced item.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemState {
    pub item_id: String,
    /// The connector's version when it was last indexed.
    pub version: Option<String>,
    /// SHA-256 of the content last indexed.
    pub content_hash: String,
    pub title: String,
}

/// How the last sync of a source ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncOutcome {
    /// Never synced.
    #[default]
    Never,
    /// Running now.
    Running,
    /// Every item is indexed.
    Succeeded,
    /// Indexed, but some items failed or the listing was incomplete (see `warnings`).
    PartlySucceeded,
    /// Nothing changed in the index (see `error`).
    Failed,
}

/// Counts of one sync.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncReport {
    /// Items new or changed since the last sync, indexed now.
    pub indexed: u64,
    /// Items whose version or content had not changed (not indexed again).
    pub unchanged: u64,
    /// Items gone from the source, removed from the knowledge base.
    pub removed: u64,
    /// Items that could not be fetched or indexed (their old passages stay).
    pub failed: u64,
    /// Chunks written for the indexed items.
    pub chunks: u64,
}

/// An error, with a stable kind for translation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusError {
    pub kind: String,
    pub message: String,
}

/// The status of one source, shown by UIs and `GET /v1/knowledge/sources`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceStatus {
    pub source_id: String,
    pub outcome: SyncOutcome,
    /// Unix seconds.
    pub last_started_at: Option<i64>,
    pub last_finished_at: Option<i64>,
    /// End of the last sync that indexed anything (succeeded or partly).
    pub last_success_at: Option<i64>,
    /// Items indexed from this source after the last sync.
    pub document_count: u64,
    pub report: SyncReport,
    /// Why the last sync failed.
    pub error: Option<StatusError>,
    /// Problems that did not stop the last sync.
    #[serde(default)]
    pub warnings: Vec<String>,
    /// How long the last sync took, in milliseconds.
    pub duration_ms: Option<u64>,
}

/// Storage of items, statuses and locks.
pub trait SyncState: Send + Sync + std::fmt::Debug {
    /// Every synced item of a source, by item id.
    fn items(&self, source_id: &str) -> Result<HashMap<String, ItemState>>;
    fn put_item(&self, source_id: &str, item: &ItemState) -> Result<()>;
    fn remove_item(&self, source_id: &str, item_id: &str) -> Result<()>;
    fn status(&self, source_id: &str) -> Result<Option<SourceStatus>>;
    fn put_status(&self, status: &SourceStatus) -> Result<()>;
    /// Take (or renew) the sync lock of a source until `expires_at` (unix
    /// seconds). Fails (`Ok(false)`) while another holder's lock is valid.
    fn try_lock(&self, source_id: &str, holder: &str, now: i64, expires_at: i64) -> Result<bool>;
    /// Release the lock if `holder` has it.
    fn unlock(&self, source_id: &str, holder: &str) -> Result<()>;
    /// Forget a source (its items, status and lock).
    fn remove_source(&self, source_id: &str) -> Result<()>;
}

#[derive(Debug, Default)]
struct Memory {
    items: HashMap<String, HashMap<String, ItemState>>,
    statuses: HashMap<String, SourceStatus>,
    locks: HashMap<String, (String, i64)>,
}

/// Sync state in the process (lost on restart: everything is re-checked once).
#[derive(Debug, Default)]
pub struct MemorySyncState {
    inner: Mutex<Memory>,
}

impl MemorySyncState {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Memory> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl SyncState for MemorySyncState {
    fn items(&self, source_id: &str) -> Result<HashMap<String, ItemState>> {
        Ok(self.lock().items.get(source_id).cloned().unwrap_or_default())
    }

    fn put_item(&self, source_id: &str, item: &ItemState) -> Result<()> {
        self.lock().items.entry(source_id.to_owned()).or_default().insert(item.item_id.clone(), item.clone());
        Ok(())
    }

    fn remove_item(&self, source_id: &str, item_id: &str) -> Result<()> {
        if let Some(items) = self.lock().items.get_mut(source_id) {
            items.remove(item_id);
        }
        Ok(())
    }

    fn status(&self, source_id: &str) -> Result<Option<SourceStatus>> {
        Ok(self.lock().statuses.get(source_id).cloned())
    }

    fn put_status(&self, status: &SourceStatus) -> Result<()> {
        self.lock().statuses.insert(status.source_id.clone(), status.clone());
        Ok(())
    }

    fn try_lock(&self, source_id: &str, holder: &str, now: i64, expires_at: i64) -> Result<bool> {
        let mut memory = self.lock();
        match memory.locks.get(source_id) {
            Some((current, until)) if current != holder && *until > now => Ok(false),
            _ => {
                memory.locks.insert(source_id.to_owned(), (holder.to_owned(), expires_at));
                Ok(true)
            }
        }
    }

    fn unlock(&self, source_id: &str, holder: &str) -> Result<()> {
        let mut memory = self.lock();
        if memory.locks.get(source_id).is_some_and(|(current, _)| current == holder) {
            memory.locks.remove(source_id);
        }
        Ok(())
    }

    fn remove_source(&self, source_id: &str) -> Result<()> {
        let mut memory = self.lock();
        memory.items.remove(source_id);
        memory.statuses.remove(source_id);
        memory.locks.remove(source_id);
        Ok(())
    }
}

#[cfg(feature = "sqlite")]
pub use sqlite::SqliteSyncState;

#[cfg(feature = "sqlite")]
mod sqlite {
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::{Mutex, PoisonError};

    use rusqlite::{Connection, OptionalExtension, params};

    use super::{ItemState, SourceStatus, SyncState};
    use crate::error::{ConnectError, Result};

    const SCHEMA: &str = "
        CREATE TABLE IF NOT EXISTS sync_items (
            source_id TEXT NOT NULL,
            item_id TEXT NOT NULL,
            version TEXT,
            content_hash TEXT NOT NULL,
            title TEXT NOT NULL DEFAULT '',
            PRIMARY KEY (source_id, item_id)
        );
        CREATE TABLE IF NOT EXISTS sync_status (
            source_id TEXT PRIMARY KEY,
            status TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS sync_locks (
            source_id TEXT PRIMARY KEY,
            holder TEXT NOT NULL,
            expires_at INTEGER NOT NULL
        );";

    fn store_error(error: rusqlite::Error) -> ConnectError {
        ConnectError::Store(format!("sync state: {error}"))
    }

    /// Sync state in one SQLite file (WAL), safe to share between processes.
    #[derive(Debug)]
    pub struct SqliteSyncState {
        connection: Mutex<Connection>,
    }

    impl SqliteSyncState {
        /// Open (and create) the state file.
        ///
        /// @throws `ConnectError::Store` when the file cannot be opened.
        pub fn open(path: &Path) -> Result<Self> {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)
                    .map_err(|e| ConnectError::Store(format!("cannot create `{}`: {e}", parent.display())))?;
            }
            let connection = Connection::open(path).map_err(store_error)?;
            Self::init(connection)
        }

        /// A state that lives in memory only (tests).
        ///
        /// @throws `ConnectError::Store` when SQLite cannot start.
        pub fn in_memory() -> Result<Self> {
            Self::init(Connection::open_in_memory().map_err(store_error)?)
        }

        /// The state file kept next to a knowledge store file (`kb.sqlite` -> `kb.sqlite.sync`).
        pub fn path_beside(store: &Path) -> std::path::PathBuf {
            let mut name = store.as_os_str().to_owned();
            name.push(".sync");
            std::path::PathBuf::from(name)
        }

        fn init(connection: Connection) -> Result<Self> {
            connection.busy_timeout(std::time::Duration::from_secs(5)).map_err(store_error)?;
            // WAL lets one process read while another writes; in-memory databases ignore it.
            let _mode: String =
                connection.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0)).map_err(store_error)?;
            connection.execute_batch(SCHEMA).map_err(store_error)?;
            Ok(Self { connection: Mutex::new(connection) })
        }

        fn connection(&self) -> std::sync::MutexGuard<'_, Connection> {
            self.connection.lock().unwrap_or_else(PoisonError::into_inner)
        }
    }

    impl SyncState for SqliteSyncState {
        fn items(&self, source_id: &str) -> Result<HashMap<String, ItemState>> {
            let connection = self.connection();
            let mut statement = connection
                .prepare_cached("SELECT item_id, version, content_hash, title FROM sync_items WHERE source_id = ?1")
                .map_err(store_error)?;
            let rows = statement
                .query_map(params![source_id], |row| {
                    Ok(ItemState {
                        item_id: row.get(0)?,
                        version: row.get(1)?,
                        content_hash: row.get(2)?,
                        title: row.get(3)?,
                    })
                })
                .map_err(store_error)?;
            let mut items = HashMap::new();
            for row in rows {
                let item = row.map_err(store_error)?;
                items.insert(item.item_id.clone(), item);
            }
            Ok(items)
        }

        fn put_item(&self, source_id: &str, item: &ItemState) -> Result<()> {
            self.connection()
                .execute(
                    "INSERT INTO sync_items (source_id, item_id, version, content_hash, title) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (source_id, item_id) DO UPDATE SET version = excluded.version,
                       content_hash = excluded.content_hash, title = excluded.title",
                    params![source_id, item.item_id, item.version, item.content_hash, item.title],
                )
                .map_err(store_error)?;
            Ok(())
        }

        fn remove_item(&self, source_id: &str, item_id: &str) -> Result<()> {
            self.connection()
                .execute("DELETE FROM sync_items WHERE source_id = ?1 AND item_id = ?2", params![source_id, item_id])
                .map_err(store_error)?;
            Ok(())
        }

        fn status(&self, source_id: &str) -> Result<Option<SourceStatus>> {
            let text: Option<String> = self
                .connection()
                .query_row("SELECT status FROM sync_status WHERE source_id = ?1", params![source_id], |row| row.get(0))
                .optional()
                .map_err(store_error)?;
            text.map(|text| {
                serde_json::from_str(&text)
                    .map_err(|e| ConnectError::Store(format!("sync status of `{source_id}`: {e}")))
            })
            .transpose()
        }

        fn put_status(&self, status: &SourceStatus) -> Result<()> {
            let text = serde_json::to_string(status).map_err(|e| ConnectError::Store(e.to_string()))?;
            self.connection()
                .execute(
                    "INSERT INTO sync_status (source_id, status) VALUES (?1, ?2)
                     ON CONFLICT (source_id) DO UPDATE SET status = excluded.status",
                    params![status.source_id, text],
                )
                .map_err(store_error)?;
            Ok(())
        }

        fn try_lock(&self, source_id: &str, holder: &str, now: i64, expires_at: i64) -> Result<bool> {
            // One statement, so two processes cannot both see the lock free.
            let changed = self
                .connection()
                .execute(
                    "INSERT INTO sync_locks (source_id, holder, expires_at) VALUES (?1, ?2, ?3)
                     ON CONFLICT (source_id) DO UPDATE SET holder = excluded.holder, expires_at = excluded.expires_at
                     WHERE sync_locks.holder = excluded.holder OR sync_locks.expires_at <= ?4",
                    params![source_id, holder, expires_at, now],
                )
                .map_err(store_error)?;
            Ok(changed == 1)
        }

        fn unlock(&self, source_id: &str, holder: &str) -> Result<()> {
            self.connection()
                .execute("DELETE FROM sync_locks WHERE source_id = ?1 AND holder = ?2", params![source_id, holder])
                .map_err(store_error)?;
            Ok(())
        }

        fn remove_source(&self, source_id: &str) -> Result<()> {
            let connection = self.connection();
            for table in ["sync_items", "sync_status", "sync_locks"] {
                connection
                    .execute(&format!("DELETE FROM {table} WHERE source_id = ?1"), params![source_id])
                    .map_err(store_error)?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exercise(state: &dyn SyncState) {
        let item =
            ItemState { item_id: "a".into(), version: Some("v1".into()), content_hash: "h".into(), title: "A".into() };
        state.put_item("s", &item).unwrap();
        assert_eq!(state.items("s").unwrap().get("a"), Some(&item));
        state.remove_item("s", "a").unwrap();
        assert!(state.items("s").unwrap().is_empty());

        assert!(state.try_lock("s", "desktop", 100, 200).unwrap());
        assert!(!state.try_lock("s", "serve", 150, 250).unwrap(), "held by the desktop app");
        assert!(state.try_lock("s", "desktop", 150, 300).unwrap(), "the holder renews");
        assert!(state.try_lock("s", "serve", 301, 400).unwrap(), "an expired lock is taken over");
        state.unlock("s", "desktop").unwrap();
        assert!(!state.try_lock("s", "desktop", 302, 400).unwrap(), "only the holder unlocks");
        state.unlock("s", "serve").unwrap();
        assert!(state.try_lock("s", "desktop", 303, 400).unwrap());

        let status = SourceStatus { source_id: "s".into(), outcome: SyncOutcome::Succeeded, ..SourceStatus::default() };
        state.put_status(&status).unwrap();
        assert_eq!(state.status("s").unwrap(), Some(status));
        state.remove_source("s").unwrap();
        assert_eq!(state.status("s").unwrap(), None);
    }

    #[test]
    fn memory_state() {
        exercise(&MemorySyncState::new());
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn sqlite_state_is_shared_between_connections() {
        let dir = std::env::temp_dir().join(format!("graphrt-connect-state-{}", std::process::id()));
        let path = dir.join("kb.sqlite.sync");
        let _ = std::fs::remove_file(&path);
        let first = SqliteSyncState::open(&path).unwrap();
        exercise(&first);
        let second = SqliteSyncState::open(&path).unwrap();
        assert!(first.try_lock("x", "desktop", 0, 100).unwrap());
        assert!(!second.try_lock("x", "serve", 10, 110).unwrap(), "a second process sees the lock");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
