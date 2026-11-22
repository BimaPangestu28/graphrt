//! T1 store: SQLite in WAL mode on a persistent volume (plan §4).
//!
//! The gateway is the only writer. Every operation is one local transaction,
//! so `claim_dispatch` / `put_if` / `create_if_absent` are atomic without any
//! lease. `synchronous = NORMAL` in WAL mode loses nothing on a process crash
//! (the WAL is in the page cache / on disk), only possibly the last commits on
//! a power loss; disaster recovery is [`SqliteStore::backup_to`] (`VACUUM
//! INTO`) on a timer, see [`crate::store::backup`].
//!
//! Calls run on tokio's blocking pool, so a slow disk never stalls the
//! reactor that proxies streams.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use graphrt_core::Snapshot;
use graphrt_core::coordination::{CoordinationStore, Versioned, validate_key, validate_value};
use graphrt_core::store::{
    RunFilter, RunPage, RunRecord, RunStatus, RunStore, StoreError, StoreResult, StoredEvent, append_mismatch, check_epoch, page_of, plan_append,
    validate_run_id,
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

/// Events kept per run; the count keeps growing past it (`events_truncated`).
pub const DEFAULT_MAX_EVENTS: u64 = 10_000;
/// Largest page of `list`.
pub const MAX_PAGE: usize = 1_000;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS runs (
    run_id TEXT PRIMARY KEY,
    deployment TEXT,
    workflow TEXT NOT NULL,
    status TEXT NOT NULL,
    epoch INTEGER NOT NULL,
    created_ms INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL,
    record TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS runs_by_created ON runs(created_ms DESC, run_id DESC);
CREATE INDEX IF NOT EXISTS runs_by_status ON runs(status, updated_ms);
CREATE TABLE IF NOT EXISTS events (
    run_id TEXT NOT NULL,
    idx INTEGER NOT NULL,
    event TEXT NOT NULL,
    PRIMARY KEY (run_id, idx)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS snapshots (
    run_id TEXT PRIMARY KEY,
    snapshot TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS objects (
    key TEXT PRIMARY KEY,
    version INTEGER NOT NULL,
    value BLOB NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS outbox (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL,
    deployment TEXT NOT NULL,
    url TEXT NOT NULL,
    body TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_ms INTEGER NOT NULL,
    created_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS outbox_due ON outbox(next_attempt_ms);
";

/// A queued completion callback (webhook outbox, plan §4: written in the same
/// transaction as the terminal record).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxEntry {
    /// Row id.
    pub id: i64,
    /// Run the callback reports.
    pub run_id: String,
    /// Deployment (selects the signing key).
    pub deployment: String,
    /// Callback URL.
    pub url: String,
    /// JSON body.
    pub body: String,
    /// Failed attempts so far.
    pub attempts: u32,
}

/// SQLite run + coordination store.
#[derive(Debug, Clone)]
pub struct SqliteStore {
    connection: Arc<Mutex<Connection>>,
    path: Option<PathBuf>,
    max_events: u64,
}

fn io_error(error: impl std::fmt::Display) -> StoreError {
    StoreError::Io(error.to_string())
}

fn corrupt(error: impl std::fmt::Display) -> StoreError {
    StoreError::Corrupt(error.to_string())
}

fn status_text(status: RunStatus) -> String {
    serde_json::to_value(status).ok().and_then(|value| value.as_str().map(str::to_owned)).unwrap_or_default()
}

impl SqliteStore {
    /// Opens (creating) the database at `path`.
    ///
    /// # Errors
    /// When the file cannot be opened or the schema cannot be created.
    pub fn open(path: &Path) -> StoreResult<Self> {
        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(io_error)?;
        }
        let connection = Connection::open(path).map_err(io_error)?;
        Self::configure(connection, Some(path.to_path_buf()))
    }

    /// An in-memory database (`--dev`, tests).
    ///
    /// # Errors
    /// When SQLite cannot allocate it.
    pub fn in_memory() -> StoreResult<Self> {
        Self::configure(Connection::open_in_memory().map_err(io_error)?, None)
    }

    fn configure(connection: Connection, path: Option<PathBuf>) -> StoreResult<Self> {
        connection
            .execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = OFF; PRAGMA busy_timeout = 5000;")
            .map_err(io_error)?;
        connection.execute_batch(SCHEMA).map_err(io_error)?;
        Ok(Self { connection: Arc::new(Mutex::new(connection)), path, max_events: DEFAULT_MAX_EVENTS })
    }

    /// Caps the events kept per run.
    pub fn with_max_events(mut self, max_events: u64) -> Self {
        self.max_events = max_events;
        self
    }

    /// The database file, if on disk.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Runs `operation` with the connection on the blocking pool.
    async fn with_connection<T, F>(&self, operation: F) -> StoreResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> StoreResult<T> + Send + 'static,
    {
        let connection = self.connection.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = connection.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            operation(&mut guard)
        })
        .await
        .map_err(io_error)?
    }

    /// Runs `operation` inside one immediate transaction.
    async fn transaction<T, F>(&self, operation: F) -> StoreResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Transaction<'_>) -> StoreResult<T> + Send + 'static,
    {
        self.with_connection(move |connection| {
            let transaction = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(io_error)?;
            let value = operation(&transaction)?;
            transaction.commit().map_err(io_error)?;
            Ok(value)
        })
        .await
    }

    /// Writes a consistent copy of the database to `target` (`VACUUM INTO`).
    ///
    /// # Errors
    /// When the copy fails (e.g. `target` exists or the disk is full).
    pub async fn backup_to(&self, target: PathBuf) -> StoreResult<()> {
        self.with_connection(move |connection| {
            let target_text = target.to_string_lossy().into_owned();
            connection.execute("VACUUM INTO ?1", params![target_text]).map_err(io_error)?;
            Ok(())
        })
        .await
    }

    /// Stores the terminal record (conditional on `expected_epoch`) and queues
    /// its callback in one transaction: the outbox can never miss a finished run.
    ///
    /// # Errors
    /// [`StoreError::Precondition`] when another epoch owns the run.
    pub async fn finish_with_callback(&self, record: RunRecord, expected_epoch: u64, callback: Option<(String, String)>, now_ms: u64) -> StoreResult<()> {
        self.transaction(move |transaction| {
            put_record(transaction, record.clone(), Some(expected_epoch))?;
            if let Some((url, body)) = callback {
                transaction
                    .execute(
                        "INSERT INTO outbox (run_id, deployment, url, body, next_attempt_ms, created_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                        params![record.run_id, record.deployment.clone().unwrap_or_default(), url, body, now_ms as i64],
                    )
                    .map_err(io_error)?;
            }
            Ok(())
        })
        .await
    }

    /// Outbox entries due at `now_ms`, oldest first.
    ///
    /// # Errors
    /// On I/O errors.
    pub async fn due_callbacks(&self, now_ms: u64, limit: usize) -> StoreResult<Vec<OutboxEntry>> {
        self.with_connection(move |connection| {
            let mut statement = connection
                .prepare("SELECT id, run_id, deployment, url, body, attempts FROM outbox WHERE next_attempt_ms <= ?1 ORDER BY next_attempt_ms LIMIT ?2")
                .map_err(io_error)?;
            let rows = statement
                .query_map(params![now_ms as i64, limit as i64], |row| {
                    Ok(OutboxEntry { id: row.get(0)?, run_id: row.get(1)?, deployment: row.get(2)?, url: row.get(3)?, body: row.get(4)?, attempts: row.get(5)? })
                })
                .map_err(io_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(io_error)
        })
        .await
    }

    /// Marks a callback delivered (deletes it).
    ///
    /// # Errors
    /// On I/O errors.
    pub async fn complete_callback(&self, id: i64) -> StoreResult<()> {
        self.with_connection(move |connection| connection.execute("DELETE FROM outbox WHERE id = ?1", params![id]).map(|_| ()).map_err(io_error)).await
    }

    /// Reschedules a failed callback, or drops it after `max_attempts`.
    /// Returns whether it was dropped.
    ///
    /// # Errors
    /// On I/O errors.
    pub async fn retry_callback(&self, id: i64, next_attempt_ms: u64, max_attempts: u32) -> StoreResult<bool> {
        self.with_connection(move |connection| {
            let attempts: u32 = connection.query_row("SELECT attempts FROM outbox WHERE id = ?1", params![id], |row| row.get(0)).optional().map_err(io_error)?.unwrap_or(0);
            if attempts + 1 >= max_attempts {
                connection.execute("DELETE FROM outbox WHERE id = ?1", params![id]).map_err(io_error)?;
                return Ok(true);
            }
            connection.execute("UPDATE outbox SET attempts = attempts + 1, next_attempt_ms = ?2 WHERE id = ?1", params![id, next_attempt_ms as i64]).map_err(io_error)?;
            Ok(false)
        })
        .await
    }

    /// Runs in `status` (all deployments), for reclaim after a restart.
    ///
    /// # Errors
    /// On I/O or decoding errors.
    pub async fn runs_with_status(&self, status: RunStatus) -> StoreResult<Vec<RunRecord>> {
        let status = status_text(status);
        self.with_connection(move |connection| {
            let mut statement = connection.prepare("SELECT record FROM runs WHERE status = ?1").map_err(io_error)?;
            let rows = statement.query_map(params![status], |row| row.get::<_, String>(0)).map_err(io_error)?;
            rows.map(|row| row.map_err(io_error).and_then(|text| serde_json::from_str(&text).map_err(corrupt))).collect()
        })
        .await
    }

    /// Number of rows per table, for `/metrics` and `gateway store check`.
    ///
    /// # Errors
    /// On I/O errors.
    pub async fn table_counts(&self) -> StoreResult<Vec<(&'static str, u64)>> {
        self.with_connection(|connection| {
            ["runs", "events", "snapshots", "objects", "outbox"]
                .into_iter()
                .map(|table| {
                    let count: i64 = connection.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0)).map_err(io_error)?;
                    Ok((table, count as u64))
                })
                .collect()
        })
        .await
    }
}

fn load_record(transaction: &Connection, run_id: &str) -> StoreResult<Option<RunRecord>> {
    let text: Option<String> = transaction.query_row("SELECT record FROM runs WHERE run_id = ?1", params![run_id], |row| row.get(0)).optional().map_err(io_error)?;
    text.map(|text| serde_json::from_str(&text).map_err(corrupt)).transpose()
}

fn write_record(transaction: &Connection, record: &RunRecord) -> StoreResult<()> {
    let text = serde_json::to_string(record).map_err(corrupt)?;
    transaction
        .execute(
            "INSERT INTO runs (run_id, deployment, workflow, status, epoch, created_ms, updated_ms, record) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(run_id) DO UPDATE SET deployment = ?2, workflow = ?3, status = ?4, epoch = ?5, created_ms = ?6, updated_ms = ?7, record = ?8",
            params![
                record.run_id,
                record.deployment,
                record.workflow,
                status_text(record.status),
                record.epoch as i64,
                record.created_ms as i64,
                record.updated_ms as i64,
                text
            ],
        )
        .map_err(io_error)?;
    Ok(())
}

/// Replaces a record keeping the store's event counters; conditional when
/// `expected_epoch` is given (and then the run must exist).
fn put_record(transaction: &Connection, mut record: RunRecord, expected_epoch: Option<u64>) -> StoreResult<()> {
    validate_run_id(&record.run_id)?;
    let stored = load_record(transaction, &record.run_id)?;
    match (&stored, expected_epoch) {
        (None, Some(_)) => return Err(StoreError::NotFound(record.run_id.clone())),
        (Some(stored), Some(expected)) => check_epoch(stored, expected)?,
        _ => {}
    }
    if let Some(stored) = stored {
        record.event_count = record.event_count.max(stored.event_count);
        record.events_truncated |= stored.events_truncated;
    }
    write_record(transaction, &record)
}

impl RunStore for SqliteStore {
    fn create(&self, record: RunRecord) -> BoxFuture<'_, StoreResult<()>> {
        Box::pin(async move {
            validate_run_id(&record.run_id)?;
            self.transaction(move |transaction| {
                if load_record(transaction, &record.run_id)?.is_some() {
                    return Err(StoreError::Conflict(format!("run `{}` already exists", record.run_id)));
                }
                write_record(transaction, &record)
            })
            .await
        })
    }

    fn put(&self, record: RunRecord) -> BoxFuture<'_, StoreResult<()>> {
        Box::pin(self.transaction(move |transaction| put_record(transaction, record, None)))
    }

    fn put_if(&self, record: RunRecord, expected_epoch: u64) -> BoxFuture<'_, StoreResult<()>> {
        Box::pin(self.transaction(move |transaction| put_record(transaction, record, Some(expected_epoch))))
    }

    fn get<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<Option<RunRecord>>> {
        let run_id = run_id.to_owned();
        Box::pin(async move {
            validate_run_id(&run_id)?;
            self.with_connection(move |connection| load_record(connection, &run_id)).await
        })
    }

    fn list(&self, filter: RunFilter) -> BoxFuture<'_, StoreResult<RunPage>> {
        Box::pin(async move {
            let status = filter.status.map(status_text);
            let workflow = filter.workflow.clone();
            let records: Vec<RunRecord> = self
                .with_connection(move |connection| {
                    let mut statement = connection
                        .prepare(
                            "SELECT record FROM runs WHERE (?1 IS NULL OR status = ?1) AND (?2 IS NULL OR workflow = ?2)
                             ORDER BY created_ms DESC, run_id DESC",
                        )
                        .map_err(io_error)?;
                    let rows = statement.query_map(params![status, workflow], |row| row.get::<_, String>(0)).map_err(io_error)?;
                    rows.map(|row| row.map_err(io_error).and_then(|text| serde_json::from_str(&text).map_err(corrupt))).collect()
                })
                .await?;
            Ok(page_of(records, &filter, MAX_PAGE))
        })
    }

    fn delete<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<bool>> {
        let run_id = run_id.to_owned();
        Box::pin(async move {
            validate_run_id(&run_id)?;
            self.transaction(move |transaction| {
                let removed = transaction.execute("DELETE FROM runs WHERE run_id = ?1", params![run_id]).map_err(io_error)?;
                transaction.execute("DELETE FROM events WHERE run_id = ?1", params![run_id]).map_err(io_error)?;
                transaction.execute("DELETE FROM snapshots WHERE run_id = ?1", params![run_id]).map_err(io_error)?;
                Ok(removed > 0)
            })
            .await
        })
    }

    fn append_events<'a>(&'a self, run_id: &'a str, first_index: u64, events: &'a [StoredEvent]) -> BoxFuture<'a, StoreResult<u64>> {
        let run_id = run_id.to_owned();
        let texts: Vec<String> = events.iter().map(|event| event.get().to_owned()).collect();
        let max_events = self.max_events;
        Box::pin(async move {
            validate_run_id(&run_id)?;
            self.transaction(move |transaction| {
                let mut record = load_record(transaction, &run_id)?.ok_or_else(|| StoreError::NotFound(run_id.clone()))?;
                let plan = plan_append(&run_id, record.event_count, first_index, texts.len())?;
                let (retried, fresh) = texts.split_at(plan.overlap.min(texts.len()));
                for (offset, text) in retried.iter().enumerate() {
                    let index = first_index + offset as u64;
                    let stored: Option<String> =
                        transaction.query_row("SELECT event FROM events WHERE run_id = ?1 AND idx = ?2", params![run_id, index as i64], |row| row.get(0)).optional().map_err(io_error)?;
                    if stored.is_some_and(|stored| stored != *text) {
                        return Err(append_mismatch(&run_id, index));
                    }
                }
                let mut index = first_index + retried.len() as u64;
                let mut dropped = false;
                for text in fresh {
                    if index < max_events {
                        transaction.execute("INSERT INTO events (run_id, idx, event) VALUES (?1, ?2, ?3)", params![run_id, index as i64, text]).map_err(io_error)?;
                    } else {
                        dropped = true;
                    }
                    index += 1;
                }
                record.event_count = plan.new_count;
                record.events_truncated |= dropped;
                write_record(transaction, &record)?;
                Ok(plan.new_count)
            })
            .await
        })
    }

    fn events<'a>(&'a self, run_id: &'a str, from: u64) -> BoxFuture<'a, StoreResult<Vec<StoredEvent>>> {
        let run_id = run_id.to_owned();
        Box::pin(async move {
            validate_run_id(&run_id)?;
            self.with_connection(move |connection| {
                if load_record(connection, &run_id)?.is_none() {
                    return Err(StoreError::NotFound(run_id));
                }
                let mut statement = connection.prepare("SELECT event FROM events WHERE run_id = ?1 AND idx >= ?2 ORDER BY idx").map_err(io_error)?;
                let rows = statement.query_map(params![run_id, from.min(i64::MAX as u64) as i64], |row| row.get::<_, String>(0)).map_err(io_error)?;
                rows.map(|row| row.map_err(io_error).and_then(|text| serde_json::value::RawValue::from_string(text).map_err(corrupt))).collect()
            })
            .await
        })
    }

    fn save_snapshot<'a>(&'a self, run_id: &'a str, snapshot: &'a Snapshot) -> BoxFuture<'a, StoreResult<()>> {
        let run_id = run_id.to_owned();
        let text = serde_json::to_string(snapshot);
        Box::pin(async move {
            validate_run_id(&run_id)?;
            let text = text.map_err(corrupt)?;
            self.transaction(move |transaction| {
                if load_record(transaction, &run_id)?.is_none() {
                    return Err(StoreError::NotFound(run_id));
                }
                transaction
                    .execute("INSERT INTO snapshots (run_id, snapshot) VALUES (?1, ?2) ON CONFLICT(run_id) DO UPDATE SET snapshot = ?2", params![run_id, text])
                    .map_err(io_error)?;
                Ok(())
            })
            .await
        })
    }

    fn load_snapshot<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<Option<Snapshot>>> {
        let run_id = run_id.to_owned();
        Box::pin(async move {
            validate_run_id(&run_id)?;
            self.with_connection(move |connection| {
                let text: Option<String> = connection.query_row("SELECT snapshot FROM snapshots WHERE run_id = ?1", params![run_id], |row| row.get(0)).optional().map_err(io_error)?;
                text.map(|text| serde_json::from_str(&text).map_err(corrupt)).transpose()
            })
            .await
        })
    }

    fn claim_resume<'a>(&'a self, run_id: &'a str, now_ms: u64) -> BoxFuture<'a, StoreResult<RunRecord>> {
        let run_id = run_id.to_owned();
        Box::pin(async move {
            validate_run_id(&run_id)?;
            self.transaction(move |transaction| {
                let mut record = load_record(transaction, &run_id)?.ok_or_else(|| StoreError::NotFound(run_id.clone()))?;
                if record.status != RunStatus::Paused {
                    return Err(StoreError::Conflict(format!("run `{run_id}` is {}, not paused (already resumed or finished)", status_text(record.status))));
                }
                record.status = RunStatus::Running;
                record.epoch = record.epoch.saturating_add(1);
                record.updated_ms = now_ms;
                write_record(transaction, &record)?;
                Ok(record)
            })
            .await
        })
    }

    fn claim_dispatch<'a>(&'a self, run_id: &'a str, expected_epoch: u64, owner: Option<String>, now_ms: u64) -> BoxFuture<'a, StoreResult<RunRecord>> {
        let run_id = run_id.to_owned();
        Box::pin(async move {
            validate_run_id(&run_id)?;
            self.transaction(move |transaction| {
                let mut record = load_record(transaction, &run_id)?.ok_or_else(|| StoreError::NotFound(run_id.clone()))?;
                if !matches!(record.status, RunStatus::Queued | RunStatus::Paused) {
                    return Err(StoreError::Conflict(format!("run `{run_id}` is not queued or paused")));
                }
                check_epoch(&record, expected_epoch)?;
                record.status = RunStatus::Running;
                record.epoch = expected_epoch.saturating_add(1);
                record.owner = owner;
                record.updated_ms = now_ms;
                write_record(transaction, &record)?;
                Ok(record)
            })
            .await
        })
    }

    fn recover_interrupted(&self, now_ms: u64) -> BoxFuture<'_, StoreResult<usize>> {
        Box::pin(self.transaction(move |transaction| {
            let running = status_text(RunStatus::Running);
            let mut statement = transaction.prepare("SELECT record FROM runs WHERE status = ?1").map_err(io_error)?;
            let texts: Vec<String> = statement.query_map(params![running], |row| row.get(0)).map_err(io_error)?.collect::<Result<_, _>>().map_err(io_error)?;
            drop(statement);
            for text in &texts {
                let mut record: RunRecord = serde_json::from_str(text).map_err(corrupt)?;
                record.status = RunStatus::Failed;
                record.error = Some(graphrt_core::store::INTERRUPTED_BY_RESTART.to_owned());
                record.error_type = Some("interrupted".to_owned());
                record.updated_ms = now_ms;
                write_record(transaction, &record)?;
            }
            Ok(texts.len())
        }))
    }

    fn purge_before(&self, cutoff_ms: u64) -> BoxFuture<'_, StoreResult<usize>> {
        Box::pin(self.transaction(move |transaction| {
            let keep = [status_text(RunStatus::Running), status_text(RunStatus::Queued)];
            let mut statement = transaction.prepare("SELECT run_id FROM runs WHERE updated_ms < ?1 AND status NOT IN (?2, ?3)").map_err(io_error)?;
            let ids: Vec<String> =
                statement.query_map(params![cutoff_ms as i64, keep[0], keep[1]], |row| row.get(0)).map_err(io_error)?.collect::<Result<_, _>>().map_err(io_error)?;
            drop(statement);
            for id in &ids {
                for table in ["runs", "events", "snapshots"] {
                    transaction.execute(&format!("DELETE FROM {table} WHERE run_id = ?1"), params![id]).map_err(io_error)?;
                }
            }
            Ok(ids.len())
        }))
    }
}

fn load_object(connection: &Connection, key: &str) -> StoreResult<Option<Versioned>> {
    connection
        .query_row("SELECT value, version FROM objects WHERE key = ?1", params![key], |row| Ok(Versioned { value: row.get(0)?, version: row.get::<_, i64>(1)? as u64 }))
        .optional()
        .map_err(io_error)
}

impl CoordinationStore for SqliteStore {
    fn create_if_absent<'a>(&'a self, key: &'a str, value: Vec<u8>) -> BoxFuture<'a, StoreResult<u64>> {
        let key = key.to_owned();
        Box::pin(async move {
            validate_key(&key)?;
            validate_value(&value)?;
            self.transaction(move |transaction| {
                if load_object(transaction, &key)?.is_some() {
                    return Err(StoreError::Conflict(format!("key `{key}` already exists")));
                }
                transaction.execute("INSERT INTO objects (key, version, value) VALUES (?1, 1, ?2)", params![key, value]).map_err(io_error)?;
                Ok(1)
            })
            .await
        })
    }

    fn compare_and_swap<'a>(&'a self, key: &'a str, expected_version: u64, value: Vec<u8>) -> BoxFuture<'a, StoreResult<u64>> {
        let key = key.to_owned();
        Box::pin(async move {
            validate_key(&key)?;
            validate_value(&value)?;
            self.transaction(move |transaction| {
                let stored = load_object(transaction, &key)?.ok_or_else(|| StoreError::NotFound(key.clone()))?;
                if stored.version != expected_version {
                    return Err(StoreError::Precondition(format!("key `{key}` is at version {}, expected {expected_version}", stored.version)));
                }
                let next = expected_version + 1;
                transaction.execute("UPDATE objects SET version = ?2, value = ?3 WHERE key = ?1", params![key, next as i64, value]).map_err(io_error)?;
                Ok(next)
            })
            .await
        })
    }

    fn get_object<'a>(&'a self, key: &'a str) -> BoxFuture<'a, StoreResult<Option<Versioned>>> {
        let key = key.to_owned();
        Box::pin(async move {
            validate_key(&key)?;
            self.with_connection(move |connection| load_object(connection, &key)).await
        })
    }

    fn list_objects<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, StoreResult<Vec<(String, Versioned)>>> {
        let prefix = prefix.to_owned();
        Box::pin(self.with_connection(move |connection| {
            let mut statement = connection.prepare("SELECT key, value, version FROM objects WHERE substr(key, 1, ?2) = ?1 ORDER BY key").map_err(io_error)?;
            let rows = statement
                .query_map(params![prefix, prefix.len() as i64], |row| Ok((row.get::<_, String>(0)?, Versioned { value: row.get(1)?, version: row.get::<_, i64>(2)? as u64 })))
                .map_err(io_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(io_error)
        }))
    }

    fn delete_object<'a>(&'a self, key: &'a str, expected_version: u64) -> BoxFuture<'a, StoreResult<bool>> {
        let key = key.to_owned();
        Box::pin(async move {
            validate_key(&key)?;
            self.transaction(move |transaction| {
                let Some(stored) = load_object(transaction, &key)? else { return Ok(false) };
                if stored.version != expected_version {
                    return Err(StoreError::Precondition(format!("key `{key}` is at version {}, expected {expected_version}", stored.version)));
                }
                transaction.execute("DELETE FROM objects WHERE key = ?1", params![key]).map_err(io_error)?;
                Ok(true)
            })
            .await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn passes_the_run_store_suite() {
        let store = SqliteStore::in_memory().unwrap();
        graphrt_core::store_conformance::run_store_suite(&store).await;
    }

    #[tokio::test]
    async fn append_retries_never_duplicate() {
        let store = SqliteStore::in_memory().unwrap();
        graphrt_core::store_conformance::append_retry_property(&store, 50).await;
    }

    #[tokio::test]
    async fn passes_the_coordination_suite() {
        let store = SqliteStore::in_memory().unwrap();
        graphrt_core::store_conformance::coordination_suite(&store).await;
    }

    #[tokio::test]
    async fn outbox_is_written_with_the_terminal_record() {
        let store = SqliteStore::in_memory().unwrap();
        let mut record = RunRecord::queued("r1", "wf", "fp", Some("shop".into()), 1);
        store.create(record.clone()).await.unwrap();
        record.status = RunStatus::Succeeded;
        let stale = store.finish_with_callback(record.clone(), 7, Some(("http://cb".into(), "{}".into())), 5).await;
        assert!(matches!(stale, Err(StoreError::Precondition(_))));
        assert!(store.due_callbacks(10, 10).await.unwrap().is_empty(), "a refused finish queues nothing");
        store.finish_with_callback(record, 0, Some(("http://cb".into(), "{}".into())), 5).await.unwrap();
        let due = store.due_callbacks(10, 10).await.unwrap();
        assert_eq!(due.len(), 1);
        assert!(!store.retry_callback(due[0].id, 100, 3).await.unwrap());
        assert!(store.due_callbacks(10, 10).await.unwrap().is_empty());
        assert_eq!(store.due_callbacks(100, 10).await.unwrap().len(), 1);
        store.complete_callback(due[0].id).await.unwrap();
        assert!(store.due_callbacks(1000, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn survives_reopen_and_backs_up() {
        let dir = std::env::temp_dir().join(format!("graphrt-gw-sqlite-{}", std::process::id()));
        let _cleanup = std::fs::remove_dir_all(&dir);
        let path = dir.join("gateway.db");
        {
            let store = SqliteStore::open(&path).unwrap();
            store.create(RunRecord::queued("r1", "wf", "fp", Some("shop".into()), 1)).await.unwrap();
            store.backup_to(dir.join("backup.db")).await.unwrap();
        }
        let reopened = SqliteStore::open(&path).unwrap();
        assert_eq!(reopened.get("r1").await.unwrap().unwrap().status, RunStatus::Queued);
        let backup = SqliteStore::open(&dir.join("backup.db")).unwrap();
        assert!(backup.get("r1").await.unwrap().is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
