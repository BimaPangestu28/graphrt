//! Persistent vector store in a single SQLite file.
//!
//! [`SqliteStore`] is the durable counterpart of the JSON-backed
//! [`MemoryStore`]: every change is one SQLite transaction touching only the
//! affected rows, instead of rewriting the whole file, and several processes
//! can share one database (SQLite locks it; WAL mode lets readers run while a
//! writer commits).
//!
//! Search runs on an in-process index mirrored from the database, so results
//! and scores are identical to [`MemoryStore`] (exact cosine, per-dataset BM25,
//! the same tokenizer). The index holds every chunk in memory, which keeps
//! queries fast up to a few hundred thousand chunks. Before each operation the
//! store checks `PRAGMA data_version`; when another connection has committed
//! since the last sync, the index is reloaded from the file.
//!
//! Vectors are stored as little-endian `f32` blobs and metadata as JSON text.
//! All calls block on SQLite I/O (like [`MemoryStore`] with a file); bulk loads
//! should upsert in batches, each batch being one transaction.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use futures::future::BoxFuture;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::error::RagError;
use crate::memory::MemoryStore;
use crate::types::{DocumentSummary, Hit, KeywordQuery, Metadata, StoredChunk, VectorQuery, VectorStore};

/// Version of the table layout written to the `graphrt_meta` table.
const SCHEMA_VERSION: i64 = 1;
/// How long a write waits for another process holding the database lock.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS graphrt_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS chunks (
    dataset_id   TEXT NOT NULL,
    id           TEXT NOT NULL,
    document_id  TEXT NOT NULL,
    title        TEXT NOT NULL,
    text         TEXT NOT NULL,
    metadata     TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    vector       BLOB NOT NULL,
    PRIMARY KEY (dataset_id, id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS chunks_by_document ON chunks (dataset_id, document_id);
";

/// [`VectorStore`] persisted in one SQLite file, with cosine and BM25 search.
///
/// ```no_run
/// use std::sync::Arc;
/// use graphrt_rag::{HashEmbedder, KnowledgeBase, SqliteStore};
///
/// let store = SqliteStore::open("knowledge.db")?;
/// let kb = KnowledgeBase::new().with_store_dataset("faq", Arc::new(store), Arc::new(HashEmbedder::new(256)), None);
/// # let _ = kb;
/// # Ok::<(), graphrt_rag::RagError>(())
/// ```
#[derive(Debug)]
pub struct SqliteStore {
    path: PathBuf,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    connection: Connection,
    index: MemoryStore,
    /// `PRAGMA data_version` when `index` was last synced with the file.
    synced_version: i64,
    /// Set when the index may disagree with the file (e.g. a failed commit).
    stale: bool,
}

impl SqliteStore {
    /// Open (or create) the database at `path` and load its chunks.
    ///
    /// @throws `RagError::Io` when the file cannot be opened, `RagError::Decode`
    /// when it was written by an unsupported schema version or holds corrupt rows.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, RagError> {
        let path = path.into();
        let connection =
            Connection::open(&path).map_err(|e| RagError::Io(format!("cannot open {}: {e}", path.display())))?;
        prepare_connection(&connection, &path)?;
        let mut state = State { connection, index: MemoryStore::new(), synced_version: 0, stale: true };
        state.refresh(&path)?;
        Ok(Self { path, state: Mutex::new(state) })
    }

    /// The database file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Number of chunks across all datasets, as of the latest commit.
    pub fn len(&self) -> Result<usize, RagError> {
        let mut state = self.lock()?;
        state.refresh(&self.path)?;
        Ok(state.index.len())
    }

    /// Whether the store holds no chunks.
    pub fn is_empty(&self) -> Result<bool, RagError> {
        Ok(self.len()? == 0)
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>, RagError> {
        self.state.lock().map_err(|_| RagError::Store("sqlite store lock poisoned by an earlier panic".into()))
    }

    /// Run a read against an index that reflects the latest commit.
    fn read<T>(&self, f: impl FnOnce(&MemoryStore) -> Result<T, RagError>) -> Result<T, RagError> {
        let mut state = self.lock()?;
        state.refresh(&self.path)?;
        f(&state.index)
    }

    /// Apply `change` in one transaction, hand its value to `mirror` to update the
    /// index, then commit. `mirror` runs before the commit so its validation
    /// errors roll the transaction back; a failed commit marks the index for
    /// reloading from the file.
    fn write<T, R>(
        &self,
        change: impl FnOnce(&Transaction<'_>) -> Result<T, RagError>,
        mirror: impl FnOnce(&MemoryStore, T) -> Result<R, RagError>,
    ) -> Result<R, RagError> {
        let mut guard = self.lock()?;
        guard.refresh(&self.path)?;
        let state = &mut *guard;
        let transaction = state.connection.transaction().map_err(|e| store_error(&self.path, &e))?;
        let changed = change(&transaction)?;
        let result = mirror(&state.index, changed)?;
        if let Err(e) = transaction.commit() {
            state.stale = true;
            return Err(store_error(&self.path, &e));
        }
        Ok(result)
    }
}

impl State {
    /// Reload the index when another connection committed, or after a failure.
    fn refresh(&mut self, path: &Path) -> Result<(), RagError> {
        let version: i64 = self
            .connection
            .query_row("PRAGMA data_version", [], |row| row.get(0))
            .map_err(|e| store_error(path, &e))?;
        if !self.stale && version == self.synced_version {
            return Ok(());
        }
        let chunks = load_chunks(&self.connection, path)?;
        self.index.replace_all(chunks)?;
        self.synced_version = version;
        self.stale = false;
        Ok(())
    }
}

/// Configure the connection, create the tables and check the schema version.
fn prepare_connection(connection: &Connection, path: &Path) -> Result<(), RagError> {
    connection.busy_timeout(BUSY_TIMEOUT).map_err(|e| store_error(path, &e))?;
    // `journal_mode` answers with the resulting mode; network file systems may refuse WAL.
    let _mode: String =
        connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0)).map_err(|e| store_error(path, &e))?;
    connection.execute_batch(SCHEMA).map_err(|e| store_error(path, &e))?;
    let stored: Option<String> = connection
        .query_row("SELECT value FROM graphrt_meta WHERE key = 'schema_version'", [], |row| row.get(0))
        .optional()
        .map_err(|e| store_error(path, &e))?;
    match stored {
        None => {
            connection
                .execute(
                    "INSERT INTO graphrt_meta (key, value) VALUES ('schema_version', ?1)",
                    params![SCHEMA_VERSION.to_string()],
                )
                .map_err(|e| store_error(path, &e))?;
            Ok(())
        }
        Some(v) if v == SCHEMA_VERSION.to_string() => Ok(()),
        Some(v) => Err(RagError::Decode(format!(
            "{}: unsupported store schema version {v} (this build reads {SCHEMA_VERSION})",
            path.display()
        ))),
    }
}

fn load_chunks(connection: &Connection, path: &Path) -> Result<Vec<StoredChunk>, RagError> {
    let mut statement = connection
        .prepare(
            "SELECT dataset_id, id, document_id, title, text, metadata, content_hash, vector \
             FROM chunks ORDER BY dataset_id, id",
        )
        .map_err(|e| store_error(path, &e))?;
    let rows = statement
        .query_map([], |row| {
            Ok(RawChunk {
                dataset_id: row.get(0)?,
                id: row.get(1)?,
                document_id: row.get(2)?,
                title: row.get(3)?,
                text: row.get(4)?,
                metadata: row.get(5)?,
                content_hash: row.get(6)?,
                vector: row.get(7)?,
            })
        })
        .map_err(|e| store_error(path, &e))?;
    let mut chunks = Vec::new();
    for row in rows {
        let raw = row.map_err(|e| store_error(path, &e))?;
        chunks.push(raw.decode(path)?);
    }
    Ok(chunks)
}

/// A row as read from SQLite, before decoding metadata and vector.
struct RawChunk {
    dataset_id: String,
    id: String,
    document_id: String,
    title: String,
    text: String,
    metadata: String,
    content_hash: String,
    vector: Vec<u8>,
}

impl RawChunk {
    fn decode(self, path: &Path) -> Result<StoredChunk, RagError> {
        let corrupt = |what: String| {
            RagError::Decode(format!(
                "{}: chunk `{}` of dataset `{}`: {what}",
                path.display(),
                self.id,
                self.dataset_id
            ))
        };
        let metadata: Metadata =
            serde_json::from_str(&self.metadata).map_err(|e| corrupt(format!("invalid metadata: {e}")))?;
        let vector = decode_vector(&self.vector).ok_or_else(|| {
            corrupt(format!("vector blob of {} bytes is not a whole number of f32 values", self.vector.len()))
        })?;
        Ok(StoredChunk {
            id: self.id,
            dataset_id: self.dataset_id,
            document_id: self.document_id,
            title: self.title,
            text: self.text,
            metadata,
            content_hash: self.content_hash,
            vector,
        })
    }
}

fn encode_vector(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|value| value.to_le_bytes()).collect()
}

/// `None` when the blob length is not a multiple of 4.
fn decode_vector(bytes: &[u8]) -> Option<Vec<f32>> {
    let (values, rest) = bytes.as_chunks::<4>();
    if !rest.is_empty() {
        return None;
    }
    Some(values.iter().copied().map(f32::from_le_bytes).collect())
}

fn store_error(path: &Path, error: &rusqlite::Error) -> RagError {
    RagError::Store(format!("{}: {error}", path.display()))
}

fn insert_rows(transaction: &Transaction<'_>, path: &Path, chunks: &[StoredChunk]) -> Result<(), RagError> {
    let mut statement = transaction
        .prepare_cached(
            "INSERT OR REPLACE INTO chunks \
             (dataset_id, id, document_id, title, text, metadata, content_hash, vector) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .map_err(|e| store_error(path, &e))?;
    for chunk in chunks {
        let metadata = serde_json::to_string(&chunk.metadata)
            .map_err(|e| RagError::Store(format!("chunk `{}`: cannot encode metadata: {e}", chunk.id)))?;
        statement
            .execute(params![
                chunk.dataset_id,
                chunk.id,
                chunk.document_id,
                chunk.title,
                chunk.text,
                metadata,
                chunk.content_hash,
                encode_vector(&chunk.vector),
            ])
            .map_err(|e| store_error(path, &e))?;
    }
    Ok(())
}

impl VectorStore for SqliteStore {
    fn upsert(&self, chunks: Vec<StoredChunk>) -> BoxFuture<'_, Result<(), RagError>> {
        let result = self.write(
            |tx| insert_rows(tx, &self.path, &chunks).map(|()| chunks),
            |index, chunks| index.insert_all(chunks),
        );
        Box::pin(futures::future::ready(result))
    }

    fn delete_document<'a>(
        &'a self,
        dataset_id: &'a str,
        document_id: &'a str,
    ) -> BoxFuture<'a, Result<usize, RagError>> {
        let result = self.write(
            |tx| {
                tx.execute(
                    "DELETE FROM chunks WHERE dataset_id = ?1 AND document_id = ?2",
                    params![dataset_id, document_id],
                )
                .map_err(|e| store_error(&self.path, &e))
            },
            |index, removed| index.remove_document(dataset_id, document_id).map(|_| removed),
        );
        Box::pin(futures::future::ready(result))
    }

    fn delete_dataset<'a>(&'a self, dataset_id: &'a str) -> BoxFuture<'a, Result<usize, RagError>> {
        let result = self.write(
            |tx| {
                tx.execute("DELETE FROM chunks WHERE dataset_id = ?1", params![dataset_id])
                    .map_err(|e| store_error(&self.path, &e))
            },
            |index, removed| index.remove_dataset(dataset_id).map(|_| removed),
        );
        Box::pin(futures::future::ready(result))
    }

    fn document_chunks<'a>(
        &'a self,
        dataset_id: &'a str,
        document_id: &'a str,
    ) -> BoxFuture<'a, Result<Vec<StoredChunk>, RagError>> {
        Box::pin(futures::future::ready(self.read(|index| index.find_document_chunks(dataset_id, document_id))))
    }

    fn delete_chunks<'a>(&'a self, dataset_id: &'a str, ids: &'a [String]) -> BoxFuture<'a, Result<usize, RagError>> {
        let result = self.write(
            |tx| {
                let mut statement = tx
                    .prepare_cached("DELETE FROM chunks WHERE dataset_id = ?1 AND id = ?2")
                    .map_err(|e| store_error(&self.path, &e))?;
                let mut removed = 0;
                for id in ids {
                    removed += statement.execute(params![dataset_id, id]).map_err(|e| store_error(&self.path, &e))?;
                }
                Ok(removed)
            },
            |index, removed| index.remove_chunks(dataset_id, ids).map(|_| removed),
        );
        Box::pin(futures::future::ready(result))
    }

    fn list_documents<'a>(
        &'a self,
        dataset_id: &'a str,
        limit: usize,
        after: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<DocumentSummary>, RagError>> {
        Box::pin(futures::future::ready(self.read(|index| index.document_summaries(dataset_id, limit, after))))
    }

    fn search<'a>(&'a self, query: VectorQuery<'a>) -> BoxFuture<'a, Result<Vec<Hit>, RagError>> {
        Box::pin(futures::future::ready(self.read(|index| index.vector_search(&query))))
    }

    fn supports_keyword(&self) -> bool {
        true
    }

    fn keyword_search<'a>(&'a self, query: KeywordQuery<'a>) -> BoxFuture<'a, Result<Vec<Hit>, RagError>> {
        Box::pin(futures::future::ready(self.read(|index| index.bm25_search(&query))))
    }

    fn contains_hash<'a>(
        &'a self,
        dataset_id: &'a str,
        content_hash: &'a str,
    ) -> BoxFuture<'a, Result<bool, RagError>> {
        Box::pin(futures::future::ready(self.read(|index| index.has_hash(dataset_id, content_hash))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(id: &str, document_id: &str, text: &str, vector: Vec<f32>) -> StoredChunk {
        let mut metadata = Metadata::new();
        metadata.insert("lang".into(), serde_json::Value::String("en".into()));
        StoredChunk {
            id: id.into(),
            dataset_id: "d".into(),
            document_id: document_id.into(),
            title: format!("title {id}"),
            text: text.into(),
            metadata,
            content_hash: crate::types::content_hash("d", text),
            vector,
        }
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        futures::executor::block_on(f)
    }

    /// A fresh directory per test, removed when dropped.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("graphrt-rag-sqlite-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn db(&self) -> PathBuf {
            self.0.join("kb.db")
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn datasets() -> Vec<String> {
        vec!["d".to_owned()]
    }

    #[test]
    fn chunks_survive_reopening_with_vectors_and_metadata() {
        let scratch = ScratchDir::new("reopen");
        let original = chunk("a", "doc", "persist me", vec![0.25, -1.5, 3.0]);
        {
            let store = SqliteStore::open(scratch.db()).unwrap();
            block_on(store.upsert(vec![original.clone()])).unwrap();
        }
        let reopened = SqliteStore::open(scratch.db()).unwrap();
        assert_eq!(reopened.len().unwrap(), 1);
        assert_eq!(block_on(reopened.document_chunks("d", "doc")).unwrap(), vec![original]);
    }

    #[test]
    fn search_results_match_memory_store() {
        let scratch = ScratchDir::new("parity");
        let chunks = vec![
            chunk("1", "a", "the cat sat on the mat", vec![1.0, 0.0, 0.0]),
            chunk("2", "b", "the dog chased the cat", vec![0.7, 0.7, 0.0]),
            chunk("3", "c", "quantum entanglement explained", vec![0.0, 0.0, 1.0]),
        ];
        let sqlite = SqliteStore::open(scratch.db()).unwrap();
        let memory = MemoryStore::new();
        block_on(sqlite.upsert(chunks.clone())).unwrap();
        block_on(memory.upsert(chunks)).unwrap();
        let ds = datasets();

        let vector = VectorQuery { datasets: &ds, vector: &[1.0, 0.1, 0.0], top_k: 3, filter: None };
        assert_eq!(block_on(sqlite.search(vector.clone())).unwrap(), block_on(memory.search(vector)).unwrap());
        let keyword = KeywordQuery { datasets: &ds, text: "cat quantum", top_k: 3, filter: None };
        assert_eq!(
            block_on(sqlite.keyword_search(keyword.clone())).unwrap(),
            block_on(memory.keyword_search(keyword)).unwrap()
        );
    }

    #[test]
    fn deletes_are_persisted() {
        let scratch = ScratchDir::new("deletes");
        let store = SqliteStore::open(scratch.db()).unwrap();
        block_on(store.upsert(vec![
            chunk("1", "a", "one", vec![1.0]),
            chunk("2", "a", "two", vec![1.0]),
            chunk("3", "b", "three", vec![1.0]),
            chunk("4", "c", "four", vec![1.0]),
        ]))
        .unwrap();
        assert_eq!(block_on(store.delete_document("d", "a")).unwrap(), 2);
        assert_eq!(block_on(store.delete_chunks("d", &["3".to_owned(), "missing".to_owned()])).unwrap(), 1);
        drop(store);

        let reopened = SqliteStore::open(scratch.db()).unwrap();
        assert_eq!(reopened.len().unwrap(), 1);
        assert_eq!(block_on(reopened.delete_dataset("d")).unwrap(), 1);
        assert!(SqliteStore::open(scratch.db()).unwrap().is_empty().unwrap());
    }

    #[test]
    fn upsert_replaces_by_id() {
        let scratch = ScratchDir::new("replace");
        let store = SqliteStore::open(scratch.db()).unwrap();
        block_on(store.upsert(vec![chunk("x", "doc", "apple banana", vec![1.0, 0.0])])).unwrap();
        block_on(store.upsert(vec![chunk("x", "doc", "cherry", vec![0.0, 1.0])])).unwrap();
        drop(store);
        let reopened = SqliteStore::open(scratch.db()).unwrap();
        let stored = block_on(reopened.document_chunks("d", "doc")).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].text, "cherry");
    }

    #[test]
    fn invalid_batch_leaves_file_and_index_unchanged() {
        let scratch = ScratchDir::new("invalid");
        let store = SqliteStore::open(scratch.db()).unwrap();
        block_on(store.upsert(vec![chunk("1", "a", "kept", vec![1.0, 0.0])])).unwrap();
        let mixed = vec![chunk("2", "b", "ok", vec![1.0, 0.0]), chunk("3", "b", "wrong size", vec![1.0, 0.0, 0.0])];
        let error = block_on(store.upsert(mixed)).unwrap_err();
        assert_eq!(error.kind(), "invalid_input");
        assert_eq!(store.len().unwrap(), 1);
        drop(store);
        assert_eq!(SqliteStore::open(scratch.db()).unwrap().len().unwrap(), 1);
    }

    #[test]
    fn sees_writes_from_another_connection() {
        let scratch = ScratchDir::new("shared");
        let reader = SqliteStore::open(scratch.db()).unwrap();
        let writer = SqliteStore::open(scratch.db()).unwrap();
        block_on(writer.upsert(vec![chunk("1", "a", "written elsewhere", vec![1.0])])).unwrap();
        let ds = datasets();
        let hits =
            block_on(reader.keyword_search(KeywordQuery { datasets: &ds, text: "elsewhere", top_k: 1, filter: None }))
                .unwrap();
        assert_eq!(hits.len(), 1, "reader reloads after another connection commits");
        block_on(writer.delete_dataset("d")).unwrap();
        assert!(reader.is_empty().unwrap());
    }

    #[test]
    fn rejects_unknown_schema_version() {
        let scratch = ScratchDir::new("schema");
        drop(SqliteStore::open(scratch.db()).unwrap());
        let connection = Connection::open(scratch.db()).unwrap();
        connection.execute("UPDATE graphrt_meta SET value = '99' WHERE key = 'schema_version'", []).unwrap();
        drop(connection);
        let error = SqliteStore::open(scratch.db()).unwrap_err();
        assert!(error.to_string().contains("schema version 99"), "{error}");
    }

    #[test]
    fn rejects_corrupt_vector_blob() {
        let scratch = ScratchDir::new("corrupt");
        drop(SqliteStore::open(scratch.db()).unwrap());
        let connection = Connection::open(scratch.db()).unwrap();
        connection
            .execute("INSERT INTO chunks VALUES ('d', 'bad', 'doc', '', 'x', '{}', 'h', ?1)", params![vec![0u8, 1, 2]])
            .unwrap();
        drop(connection);
        let error = SqliteStore::open(scratch.db()).unwrap_err();
        assert_eq!(error.kind(), "invalid_response");
        assert!(error.to_string().contains("chunk `bad`"), "{error}");
    }

    #[test]
    fn closing_the_store_checkpoints_the_write_ahead_log() {
        let scratch = ScratchDir::new("checkpoint");
        let store = SqliteStore::open(scratch.db()).unwrap();
        block_on(store.upsert(vec![chunk("1", "a", "flushed on close", vec![1.0])])).unwrap();
        drop(store);
        // Bundles copy only the main file, so it must hold every commit once closed.
        let wal = scratch.0.join("kb.db-wal");
        assert!(std::fs::metadata(&wal).map_or(true, |m| m.len() == 0), "{} still holds commits", wal.display());
    }

    #[test]
    fn vector_encoding_roundtrip() {
        let vector = vec![0.0, -0.0, 1.5, f32::MIN_POSITIVE, -123.456];
        assert_eq!(decode_vector(&encode_vector(&vector)).unwrap(), vector);
        assert!(decode_vector(&[1, 2, 3, 4, 5]).is_none());
    }
}
