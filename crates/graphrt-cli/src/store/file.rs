//! Durable directory [`RunStore`].
//!
//! Layout:
//!
//! ```text
//! <dir>/.lock                     exclusive lock taken around every operation
//! <dir>/runs/<run_id>/record.json run record (atomic replace)
//! <dir>/runs/<run_id>/events.jsonl one stored event per line (append)
//! <dir>/runs/<run_id>/snapshot.json snapshot of a paused run (atomic replace)
//! <dir>/runs/<run_id>/scores.json evaluation scores (atomic replace; feature `scores`)
//! ```
//!
//! Records and snapshots are written to a unique temporary file, fsynced, renamed
//! over the target and the directory fsynced, so a crash leaves either the old or
//! the new version. Every operation holds an OS file lock on `<dir>/.lock`, which
//! serializes access from threads and from other processes sharing the directory.
//! Listing reads every record, so it is meant for thousands of runs, not millions.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::future::BoxFuture;
use graphrt_core::Snapshot;
use graphrt_core::store::{
    RunFilter, RunPage, RunRecord, RunStore, StoreError, StoreResult, StoredEvent, page_of, sort_newest_first,
    validate_run_id,
};

use super::memory::{DEFAULT_MAX_EVENTS, MAX_PAGE, claim, mark_interrupted, purgeable};
use crate::error::CliError;

const RECORD: &str = "record.json";
const EVENTS: &str = "events.jsonl";
const SNAPSHOT: &str = "snapshot.json";
#[cfg(feature = "scores")]
const SCORES: &str = "scores.json";

#[derive(Debug)]
struct Inner {
    root: PathBuf,
    max_events: usize,
    tmp_counter: AtomicU64,
}

/// Run store persisted in a directory.
#[derive(Clone, Debug)]
pub struct FileStore {
    inner: Arc<Inner>,
}

impl FileStore {
    /// Open (creating if needed) a store directory.
    pub fn open(dir: &Path) -> Result<Self, CliError> {
        fs::create_dir_all(dir.join("runs"))
            .map_err(|e| CliError::usage(format!("cannot create run store `{}`: {e}", dir.display())))?;
        Ok(Self {
            inner: Arc::new(Inner {
                root: dir.to_path_buf(),
                max_events: DEFAULT_MAX_EVENTS,
                tmp_counter: AtomicU64::new(0),
            }),
        })
    }

    /// Keep at most `max` events per run.
    pub fn max_events(self, max: usize) -> Self {
        let inner = Inner {
            root: self.inner.root.clone(),
            max_events: max,
            tmp_counter: AtomicU64::new(self.inner.tmp_counter.load(Ordering::Relaxed)),
        };
        Self { inner: Arc::new(inner) }
    }

    /// Run `op` on the blocking pool while holding the store lock.
    fn blocking<T, F>(&self, op: F) -> BoxFuture<'static, StoreResult<T>>
    where
        T: Send + 'static,
        F: FnOnce(&Inner) -> StoreResult<T> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let _lock = inner.lock()?;
                op(&inner)
            })
            .await
            .map_err(|e| StoreError::Io(format!("store task failed: {e}")))?
        })
    }
}

fn io(context: &str, e: &std::io::Error) -> StoreError {
    StoreError::Io(format!("{context}: {e}"))
}

/// Holds the exclusive OS lock until dropped.
struct LockGuard(File);

impl Drop for LockGuard {
    fn drop(&mut self) {
        // Closing the file also releases the lock; an explicit unlock just makes it prompt.
        let _released = self.0.unlock();
    }
}

impl Inner {
    fn lock(&self) -> StoreResult<LockGuard> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.root.join(".lock"))
            .map_err(|e| io("cannot open store lock", &e))?;
        file.lock().map_err(|e| io("cannot lock store", &e))?;
        Ok(LockGuard(file))
    }

    fn run_dir(&self, run_id: &str) -> StoreResult<PathBuf> {
        validate_run_id(run_id)?;
        Ok(self.root.join("runs").join(run_id))
    }

    fn read_record(&self, dir: &Path) -> StoreResult<Option<RunRecord>> {
        match fs::read(dir.join(RECORD)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| StoreError::Corrupt(format!("{}: {e}", dir.join(RECORD).display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io("cannot read run record", &e)),
        }
    }

    fn require_record(&self, run_id: &str) -> StoreResult<(PathBuf, RunRecord)> {
        let dir = self.run_dir(run_id)?;
        let record = self.read_record(&dir)?.ok_or_else(|| StoreError::NotFound(run_id.to_owned()))?;
        Ok((dir, record))
    }

    fn write_record(&self, dir: &Path, record: &RunRecord) -> StoreResult<()> {
        let bytes = serde_json::to_vec_pretty(record).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        self.atomic_write(dir, RECORD, &bytes)
    }

    fn atomic_write(&self, dir: &Path, name: &str, bytes: &[u8]) -> StoreResult<()> {
        fs::create_dir_all(dir).map_err(|e| io("cannot create run directory", &e))?;
        let n = self.tmp_counter.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".{name}.{}.{n}.tmp", std::process::id()));
        let result = (|| {
            let mut file = File::create(&tmp).map_err(|e| io("cannot create temporary file", &e))?;
            file.write_all(bytes).map_err(|e| io("cannot write temporary file", &e))?;
            file.sync_all().map_err(|e| io("cannot sync temporary file", &e))?;
            fs::rename(&tmp, dir.join(name)).map_err(|e| io("cannot replace file", &e))?;
            sync_dir(dir)
        })();
        if result.is_err() {
            let _cleanup = fs::remove_file(&tmp);
        }
        result
    }

    fn all_records(&self) -> StoreResult<Vec<(PathBuf, RunRecord)>> {
        let runs = self.root.join("runs");
        let entries = fs::read_dir(&runs).map_err(|e| io("cannot list runs", &e))?;
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| io("cannot list runs", &e))?;
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            match self.read_record(&dir) {
                Ok(Some(record)) => out.push((dir, record)),
                Ok(None) => {}
                // One damaged record must not hide every other run; report it and go on.
                Err(e) => eprintln!("warning: skipping run store entry `{}`: {e}", dir.display()),
            }
        }
        Ok(out)
    }

    fn count_lines(path: &Path) -> StoreResult<usize> {
        match File::open(path) {
            Ok(file) => Ok(BufReader::new(file).lines().count()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(io("cannot read events", &e)),
        }
    }
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> StoreResult<()> {
    File::open(dir).and_then(|d| d.sync_all()).map_err(|e| io("cannot sync directory", &e))
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> StoreResult<()> {
    Ok(())
}

#[cfg(feature = "scores")]
impl Inner {
    fn read_scores(dir: &Path) -> StoreResult<Vec<crate::scores::Score>> {
        let corrupt = |e: &dyn std::fmt::Display| StoreError::Corrupt(format!("{}: {e}", dir.join(SCORES).display()));
        match fs::read(dir.join(SCORES)) {
            Ok(bytes) => {
                let list: Vec<serde_json::Value> = serde_json::from_slice(&bytes).map_err(|e| corrupt(&e))?;
                list.iter()
                    .map(|s| crate::scores::Score::from_json(s).ok_or_else(|| corrupt(&"invalid score")))
                    .collect()
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(io("cannot read scores", &e)),
        }
    }
}

#[cfg(feature = "scores")]
impl crate::scores::ScoreStore for FileStore {
    fn put_scores<'a>(&'a self, run_id: &'a str, scores: Vec<crate::scores::Score>) -> BoxFuture<'a, StoreResult<()>> {
        let run_id = run_id.to_owned();
        self.blocking(move |inner| {
            let (dir, _) = inner.require_record(&run_id)?;
            let mut stored = Inner::read_scores(&dir)?;
            crate::scores::merge_scores(&mut stored, scores, &run_id)?;
            let bytes = serde_json::to_vec_pretty(&crate::scores::scores_json(&stored))
                .map_err(|e| StoreError::Corrupt(e.to_string()))?;
            inner.atomic_write(&dir, SCORES, &bytes)
        })
    }

    fn scores<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<Vec<crate::scores::Score>>> {
        let run_id = run_id.to_owned();
        self.blocking(move |inner| {
            let (dir, _) = inner.require_record(&run_id)?;
            Inner::read_scores(&dir)
        })
    }
}

impl RunStore for FileStore {
    fn create(&self, record: RunRecord) -> BoxFuture<'_, StoreResult<()>> {
        self.blocking(move |inner| {
            let dir = inner.run_dir(&record.run_id)?;
            if inner.read_record(&dir)?.is_some() {
                return Err(StoreError::Conflict(format!("run `{}` already exists", record.run_id)));
            }
            inner.write_record(&dir, &record)
        })
    }

    fn put(&self, record: RunRecord) -> BoxFuture<'_, StoreResult<()>> {
        self.blocking(move |inner| {
            let dir = inner.run_dir(&record.run_id)?;
            let mut record = record;
            if let Some(old) = inner.read_record(&dir)? {
                // Event counters are maintained by the store.
                record.event_count = record.event_count.max(old.event_count);
                record.events_truncated |= old.events_truncated;
            }
            inner.write_record(&dir, &record)
        })
    }

    fn get<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<Option<RunRecord>>> {
        let run_id = run_id.to_owned();
        self.blocking(move |inner| {
            let dir = inner.run_dir(&run_id)?;
            inner.read_record(&dir)
        })
    }

    fn list(&self, filter: RunFilter) -> BoxFuture<'_, StoreResult<RunPage>> {
        self.blocking(move |inner| {
            let mut records: Vec<RunRecord> = inner.all_records()?.into_iter().map(|(_, r)| r).collect();
            sort_newest_first(&mut records);
            Ok(page_of(records, &filter, MAX_PAGE))
        })
    }

    fn delete<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<bool>> {
        let run_id = run_id.to_owned();
        self.blocking(move |inner| {
            let dir = inner.run_dir(&run_id)?;
            if inner.read_record(&dir)?.is_none() {
                return Ok(false);
            }
            fs::remove_dir_all(&dir).map_err(|e| io("cannot delete run", &e))?;
            Ok(true)
        })
    }

    fn append_events<'a>(&'a self, run_id: &'a str, events: Vec<StoredEvent>) -> BoxFuture<'a, StoreResult<()>> {
        let run_id = run_id.to_owned();
        self.blocking(move |inner| {
            let (dir, mut record) = inner.require_record(&run_id)?;
            let path = dir.join(EVENTS);
            let stored = Inner::count_lines(&path)?;
            let room = inner.max_events.saturating_sub(stored);
            let dropped = events.len() > room;
            let mut buf = Vec::new();
            for event in events.iter().take(room) {
                buf.extend_from_slice(event.get().as_bytes());
                buf.push(b'\n');
            }
            if !buf.is_empty() {
                let mut file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .map_err(|e| io("cannot open events", &e))?;
                file.write_all(&buf).map_err(|e| io("cannot append events", &e))?;
                file.sync_data().map_err(|e| io("cannot sync events", &e))?;
            }
            let total = stored.saturating_add(events.len().min(room));
            record.event_count = u64::try_from(total).unwrap_or(u64::MAX);
            record.events_truncated |= dropped;
            inner.write_record(&dir, &record)
        })
    }

    fn events<'a>(&'a self, run_id: &'a str, from: u64) -> BoxFuture<'a, StoreResult<Vec<StoredEvent>>> {
        let run_id = run_id.to_owned();
        self.blocking(move |inner| {
            let (dir, _) = inner.require_record(&run_id)?;
            let file = match File::open(dir.join(EVENTS)) {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(e) => return Err(io("cannot read events", &e)),
            };
            let skip = usize::try_from(from).unwrap_or(usize::MAX);
            let mut out = Vec::new();
            for line in BufReader::new(file).lines().skip(skip) {
                let line = line.map_err(|e| io("cannot read events", &e))?;
                let value = serde_json::value::RawValue::from_string(line)
                    .map_err(|e| StoreError::Corrupt(format!("event line in run `{run_id}`: {e}")))?;
                out.push(value);
            }
            Ok(out)
        })
    }

    fn save_snapshot<'a>(&'a self, run_id: &'a str, snapshot: &'a Snapshot) -> BoxFuture<'a, StoreResult<()>> {
        let run_id = run_id.to_owned();
        let encoded = snapshot.to_json();
        self.blocking(move |inner| {
            let (dir, _) = inner.require_record(&run_id)?;
            let text = encoded.map_err(|e| StoreError::Corrupt(format!("cannot encode snapshot: {e}")))?;
            inner.atomic_write(&dir, SNAPSHOT, text.as_bytes())
        })
    }

    fn load_snapshot<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<Option<Snapshot>>> {
        let run_id = run_id.to_owned();
        self.blocking(move |inner| {
            let dir = inner.run_dir(&run_id)?;
            match fs::read_to_string(dir.join(SNAPSHOT)) {
                Ok(text) => Snapshot::from_json(&text)
                    .map(Some)
                    .map_err(|e| StoreError::Corrupt(format!("snapshot of run `{run_id}`: {e}"))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(io("cannot read snapshot", &e)),
            }
        })
    }

    fn claim_resume<'a>(&'a self, run_id: &'a str, now_ms: u64) -> BoxFuture<'a, StoreResult<RunRecord>> {
        let run_id = run_id.to_owned();
        self.blocking(move |inner| {
            let (dir, mut record) = inner.require_record(&run_id)?;
            let claimed = claim(&mut record, now_ms)?;
            inner.write_record(&dir, &record)?;
            Ok(claimed)
        })
    }

    fn recover_interrupted(&self, now_ms: u64) -> BoxFuture<'_, StoreResult<usize>> {
        self.blocking(move |inner| {
            let mut marked = 0;
            for (dir, mut record) in inner.all_records()? {
                if mark_interrupted(&mut record, now_ms) {
                    inner.write_record(&dir, &record)?;
                    marked += 1;
                }
            }
            Ok(marked)
        })
    }

    fn purge_before(&self, cutoff_ms: u64) -> BoxFuture<'_, StoreResult<usize>> {
        self.blocking(move |inner| {
            let mut removed = 0;
            for (dir, record) in inner.all_records()? {
                if purgeable(&record, cutoff_ms) {
                    fs::remove_dir_all(&dir).map_err(|e| io("cannot delete run", &e))?;
                    removed += 1;
                }
            }
            Ok(removed)
        })
    }
}

#[cfg(test)]
mod tests {
    use graphrt_core::store::RunStatus;

    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("graphrt-store-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[tokio::test]
    async fn persists_across_reopen_and_claims_once() {
        let dir = temp_dir("reopen");
        let store = FileStore::open(&dir).unwrap().max_events(2);
        let mut paused = RunRecord::running("p1", "wf", "fp", 1);
        paused.status = RunStatus::Paused;
        store.create(paused).await.unwrap();
        store.create(RunRecord::running("r1", "wf", "fp", 2)).await.unwrap();
        assert!(matches!(store.create(RunRecord::running("r1", "wf", "fp", 2)).await, Err(StoreError::Conflict(_))));
        store.append_events("p1", crate::store::raw(&[r#"{"a":1}"#, r#"{"b":2}"#])).await.unwrap();
        store.append_events("p1", crate::store::raw(&[r#"{"c":3}"#])).await.unwrap();
        drop(store);

        let store = FileStore::open(&dir).unwrap();
        assert_eq!(store.recover_interrupted(5).await.unwrap(), 1);
        assert_eq!(store.get("r1").await.unwrap().unwrap().status, RunStatus::Failed);
        let p1 = store.get("p1").await.unwrap().unwrap();
        assert_eq!(p1.status, RunStatus::Paused);
        assert_eq!(p1.event_count, 2);
        assert!(p1.events_truncated);
        assert_eq!(crate::store::texts(&store.events("p1", 1).await.unwrap()), [r#"{"b":2}"#]);

        let (a, b) = tokio::join!(store.claim_resume("p1", 6), store.claim_resume("p1", 6));
        assert!(a.is_ok() != b.is_ok(), "exactly one claim must win");
        assert!(store.delete("p1").await.unwrap());
        assert!(!store.delete("p1").await.unwrap());
        assert!(matches!(store.get("../etc").await, Err(StoreError::InvalidId(_))));
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn evaluation_fields_survive_reopen_and_claims() {
        let dir = temp_dir("eval-fields");
        let store = FileStore::open(&dir).unwrap();
        let mut record = RunRecord::running("e1", "wf", "fp", 1);
        record.status = RunStatus::Paused;
        let mut inputs = graphrt_core::IndexMap::new();
        inputs.insert("q".to_owned(), graphrt_core::Value::from("hi"));
        record.inputs = Some(inputs);
        record.trace_id = Some("4bf92f3577b34da6a3ce929d0e0e4736".into());
        record.span_id = Some("00f067aa0ba902b7".into());
        record.metadata.insert("tenant".into(), "acme".into());
        record.session_id = Some("s".into());
        record.user_id = Some("u".into());
        record.tags = vec!["t".into()];
        store.create(record.clone()).await.unwrap();
        drop(store);
        let store = FileStore::open(&dir).unwrap();
        assert_eq!(store.get("e1").await.unwrap(), Some(record.clone()));
        let claimed = store.claim_resume("e1", 2).await.unwrap();
        assert_eq!((claimed.inputs, claimed.metadata, claimed.tags), (record.inputs, record.metadata, record.tags));
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "scores")]
    #[tokio::test]
    async fn scores_are_kept_next_to_the_run() {
        use crate::scores::{Score, ScoreSource, ScoreStore, ScoreValue};
        let dir = temp_dir("scores");
        let store = FileStore::open(&dir).unwrap();
        let score =
            |name: &str, value: f64| Score::new("s1", name, ScoreValue::Number(value), ScoreSource::Online, None);
        assert!(matches!(store.put_scores("s1", vec![score("a", 1.0)]).await, Err(StoreError::NotFound(_))));
        let mut record = RunRecord::running("s1", "wf", "fp", 1);
        record.status = RunStatus::Succeeded;
        store.create(record.clone()).await.unwrap();
        assert!(store.scores("s1").await.unwrap().is_empty());
        store.put_scores("s1", vec![score("a", 1.0), score("b", 0.5)]).await.unwrap();
        store.put_scores("s1", vec![score("a", 0.0)]).await.unwrap();
        store.put(record).await.unwrap();
        assert!(dir.join("runs/s1/scores.json").is_file());
        drop(store);
        let store = FileStore::open(&dir).unwrap();
        let scores = store.scores("s1").await.unwrap();
        assert_eq!(scores.len(), 2);
        assert_eq!((scores[0].name.as_str(), &scores[0].value), ("a", &ScoreValue::Number(0.0)));
        // Removed with the run by the TTL cleanup.
        assert_eq!(store.purge_before(u64::MAX).await.unwrap(), 1);
        assert!(matches!(store.scores("s1").await, Err(StoreError::NotFound(_))));
        assert!(!dir.join("runs/s1").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
