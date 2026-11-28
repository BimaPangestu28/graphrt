//! [`RunStore`] (with scores and coordination objects) kept as objects that
//! several servers share: `graphrt serve --store s3://bucket/prefix`.
//!
//! Layout below the prefix:
//!
//! ```text
//! runs/<run_id>/record.json            run record, replaced with If-Match on its ETag
//! runs/<run_id>/events/<first>.jsonl   events from index <first> (12 digits), created with If-None-Match
//! runs/<run_id>/snapshot.json          snapshot of a paused run
//! runs/<run_id>/scores.json            evaluation scores (feature `scores`)
//! coord/<key>                          coordination object: version line, then the value
//! ```
//!
//! Every change of a record is a compare and swap on its ETag, retried after
//! reading the record again, so two servers can claim, resume and finish runs
//! of the same store without a lock. Event chunks are never replaced once a
//! record counts them: a retried batch finds its chunk and compares it.
//!
//! A server that starts does not know which `running` runs belong to servers
//! still running (other Lambda instances), so it marks a run interrupted only
//! when its record has not changed for [`DEFAULT_STALE_RUN_MS`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use futures::StreamExt;
use futures::future::BoxFuture;
use graphrt_core::Snapshot;
use graphrt_core::coordination::{CoordinationStore, Versioned, validate_key, validate_value};
use graphrt_core::store::{
    RunFilter, RunPage, RunRecord, RunStatus, RunStore, StoreError, StoreResult, StoredEvent, append_mismatch,
    check_epoch, page_of, plan_append, sort_newest_first, validate_run_id,
};

use super::memory::{DEFAULT_MAX_EVENTS, MAX_PAGE, check_version, claim, mark_interrupted, purgeable};
use super::objects::{Condition, Object, ObjectError, Objects, Read, get};

/// A `running` run whose record did not change for this long belongs to no
/// live server (Lambda stops a request after 15 minutes).
pub const DEFAULT_STALE_RUN_MS: u64 = 60 * 60 * 1000;
/// Overrides [`DEFAULT_STALE_RUN_MS`], in seconds.
pub const STALE_RUN_ENV: &str = "GRAPHRT_STORE_STALE_RUN_SECS";
/// Compare and swap attempts before giving up on a busy record.
const MAX_ATTEMPTS: usize = 64;
/// Records read at the same time while listing.
const LIST_CONCURRENCY: usize = 16;
const RECORD: &str = "record.json";
const SNAPSHOT: &str = "snapshot.json";
#[cfg(feature = "scores")]
const SCORES: &str = "scores.json";

/// Run store kept as shared objects.
#[derive(Debug, Clone)]
pub struct SharedStore {
    objects: Arc<dyn Objects>,
    max_events: usize,
    stale_after_ms: u64,
    /// Records already read while listing, by key, with their ETag.
    cache: Arc<Mutex<HashMap<String, (String, RunRecord)>>>,
    /// Look for stale runs after startup instead of during it (see [`SharedStore::recover_in_background`]).
    background_recovery: bool,
}

fn object_error(error: ObjectError) -> StoreError {
    match error {
        ObjectError::Precondition => StoreError::Precondition("the object changed concurrently".to_owned()),
        ObjectError::Io(message) => StoreError::Io(message),
    }
}

fn busy(run_id: &str) -> StoreError {
    StoreError::Io(format!("run `{run_id}` kept changing while it was written: try again"))
}

fn encode<T: serde::Serialize>(value: &T) -> StoreResult<Vec<u8>> {
    serde_json::to_vec(value).map_err(|e| StoreError::Corrupt(e.to_string()))
}

fn record_key(run_id: &str) -> String {
    format!("runs/{run_id}/{RECORD}")
}

fn events_prefix(run_id: &str) -> String {
    format!("runs/{run_id}/events/")
}

fn chunk_key(run_id: &str, first: u64) -> String {
    format!("runs/{run_id}/events/{first:012}.jsonl")
}

fn chunk_bytes(events: &[StoredEvent]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for event in events {
        bytes.extend_from_slice(event.get().as_bytes());
        bytes.push(b'\n');
    }
    bytes
}

fn parse_chunk(bytes: &[u8], key: &str) -> StoreResult<Vec<StoredEvent>> {
    let text = std::str::from_utf8(bytes).map_err(|_| StoreError::Corrupt(format!("{key}: not UTF-8")))?;
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::value::RawValue::from_string(line.to_owned())
                .map_err(|e| StoreError::Corrupt(format!("{key}: {e}")))
        })
        .collect()
}

/// Event counters are maintained by the store: keep the stored ones.
fn keep_counters(record: &mut RunRecord, old: &RunRecord) {
    record.event_count = record.event_count.max(old.event_count);
    record.events_truncated |= old.events_truncated;
}

impl SharedStore {
    /// A store over `objects`.
    pub fn new(objects: Arc<dyn Objects>) -> Self {
        let stale_after_ms = std::env::var(STALE_RUN_ENV)
            .ok()
            .and_then(|secs| secs.trim().parse::<u64>().ok())
            .map_or(DEFAULT_STALE_RUN_MS, |secs| secs.saturating_mul(1000));
        Self {
            objects,
            max_events: DEFAULT_MAX_EVENTS,
            stale_after_ms,
            cache: Arc::default(),
            background_recovery: false,
        }
    }

    /// Sweep stale runs in a background task when the server starts, so a
    /// cold start (a new Lambda instance) does not wait for every record of
    /// the store to be read. `recover_interrupted` then answers 0 at once.
    pub fn recover_in_background(mut self) -> Self {
        self.background_recovery = true;
        self
    }

    /// Marks `running` runs whose record did not change for the stale time as interrupted.
    async fn sweep_stale(&self, now_ms: u64) -> StoreResult<usize> {
        let cutoff = now_ms.saturating_sub(self.stale_after_ms);
        let stale: Vec<String> = self
            .all_records()
            .await?
            .into_iter()
            .filter(|record| record.status == RunStatus::Running && record.updated_ms < cutoff)
            .map(|record| record.run_id)
            .collect();
        let mut marked = 0;
        for run_id in stale {
            let record = self
                .update_record(&run_id, |record| {
                    Ok(record.status == RunStatus::Running
                        && record.updated_ms < cutoff
                        && mark_interrupted(record, now_ms))
                })
                .await;
            if record.is_ok_and(|record| record.status == RunStatus::Failed) {
                marked += 1;
            }
        }
        Ok(marked)
    }

    /// Keep at most `max` events per run.
    pub fn max_events(mut self, max: usize) -> Self {
        self.max_events = max;
        self
    }

    /// Where the objects are, for the startup log.
    pub fn describe(&self) -> String {
        self.objects.describe()
    }

    /// The objects this store keeps, for the other stores of the same location.
    pub fn objects(&self) -> Arc<dyn Objects> {
        Arc::clone(&self.objects)
    }

    async fn read_record(&self, run_id: &str) -> StoreResult<Option<(RunRecord, String)>> {
        validate_run_id(run_id)?;
        let key = record_key(run_id);
        let Some(Object { bytes, etag }) = get(self.objects.as_ref(), &key).await.map_err(object_error)? else {
            return Ok(None);
        };
        let record = serde_json::from_slice(&bytes).map_err(|e| StoreError::Corrupt(format!("{key}: {e}")))?;
        Ok(Some((record, etag)))
    }

    async fn require_record(&self, run_id: &str) -> StoreResult<(RunRecord, String)> {
        self.read_record(run_id).await?.ok_or_else(|| StoreError::NotFound(run_id.to_owned()))
    }

    async fn write_record(&self, record: &RunRecord, condition: Condition) -> Result<String, ObjectError> {
        let bytes = encode(record).map_err(|e| ObjectError::Io(e.to_string()))?;
        self.objects.write(&record_key(&record.run_id), bytes, condition).await
    }

    /// Read, change and compare and swap the record of `run_id` until no
    /// other writer came in between. `change` returns whether to write.
    async fn update_record(
        &self,
        run_id: &str,
        mut change: impl FnMut(&mut RunRecord) -> StoreResult<bool>,
    ) -> StoreResult<RunRecord> {
        for _ in 0..MAX_ATTEMPTS {
            let (record, etag) = self.require_record(run_id).await?;
            let mut next = record.clone();
            if !change(&mut next)? {
                return Ok(next);
            }
            match self.write_record(&next, Condition::Matches(etag)).await {
                Ok(_) => return Ok(next),
                Err(ObjectError::Precondition) => continue,
                Err(error) => return Err(object_error(error)),
            }
        }
        Err(busy(run_id))
    }

    /// The stored events of `run_id` from index `from`, at most up to `limit`.
    async fn read_events(&self, run_id: &str, from: u64, limit: u64) -> StoreResult<Vec<StoredEvent>> {
        let prefix = events_prefix(run_id);
        let mut starts: Vec<(u64, String)> = self
            .objects
            .keys(&prefix)
            .await
            .map_err(object_error)?
            .into_iter()
            .filter_map(|(key, _)| {
                let start = key.strip_prefix(&prefix)?.strip_suffix(".jsonl")?.parse::<u64>().ok()?;
                Some((start, key))
            })
            .filter(|(start, _)| *start < limit)
            .collect();
        starts.sort();
        let mut events = Vec::new();
        for (position, (start, key)) in starts.iter().enumerate() {
            let next_start = starts.get(position + 1).map_or(u64::MAX, |(next, _)| *next);
            if next_start <= from {
                continue;
            }
            let Some(object) = get(self.objects.as_ref(), key).await.map_err(object_error)? else { continue };
            for (offset, event) in parse_chunk(&object.bytes, key)?.into_iter().enumerate() {
                let index = start.saturating_add(offset as u64);
                if index >= from && index < limit {
                    events.push(event);
                }
            }
        }
        Ok(events)
    }

    async fn append(&self, run_id: &str, first_index: u64, events: &[StoredEvent]) -> StoreResult<u64> {
        for _ in 0..MAX_ATTEMPTS {
            let (record, etag) = self.require_record(run_id).await?;
            let count = record.event_count;
            let plan = plan_append(run_id, count, first_index, events.len())?;
            let (retried, new) = events.split_at(plan.overlap);
            if !retried.is_empty() {
                let stored_limit = count.min(self.max_events as u64);
                let stored = self.read_events(run_id, first_index, stored_limit).await?;
                if let Some(offset) = stored.iter().zip(retried).position(|(stored, event)| stored.get() != event.get())
                {
                    return Err(append_mismatch(run_id, first_index + offset as u64));
                }
            }
            let room = self.max_events.saturating_sub(usize::try_from(count).unwrap_or(usize::MAX));
            let to_write: Vec<StoredEvent> = new.iter().take(room).cloned().collect();
            if !to_write.is_empty() && !self.write_chunk(run_id, count, &to_write, &etag).await? {
                continue;
            }
            let truncated = new.len() > room;
            if plan.new_count == count && !(truncated && !record.events_truncated) {
                return Ok(count);
            }
            let mut next = record.clone();
            next.event_count = plan.new_count;
            next.events_truncated |= truncated;
            match self.write_record(&next, Condition::Matches(etag)).await {
                Ok(_) => return Ok(plan.new_count),
                Err(ObjectError::Precondition) => continue,
                Err(error) => return Err(object_error(error)),
            }
        }
        Err(busy(run_id))
    }

    /// Create the chunk of events at index `first`. A chunk already there is
    /// fine when it holds the same events (a retry), and replaced when the
    /// record still does not count it (left by a writer that stopped before
    /// counting it). Returns `false` when the record moved meanwhile.
    async fn write_chunk(
        &self,
        run_id: &str,
        first: u64,
        events: &[StoredEvent],
        record_etag: &str,
    ) -> StoreResult<bool> {
        let key = chunk_key(run_id, first);
        let bytes = chunk_bytes(events);
        match self.objects.write(&key, bytes.clone(), Condition::Absent).await {
            Ok(_) => return Ok(true),
            Err(ObjectError::Precondition) => {}
            Err(error) => return Err(object_error(error)),
        }
        let Some(existing) = get(self.objects.as_ref(), &key).await.map_err(object_error)? else {
            return Ok(false);
        };
        if existing.bytes == bytes {
            return Ok(true);
        }
        match self.read_record(run_id).await? {
            Some((_, etag)) if etag == record_etag => {}
            _ => return Ok(false),
        }
        match self.objects.write(&key, bytes, Condition::Matches(existing.etag)).await {
            Ok(_) => Ok(true),
            Err(ObjectError::Precondition) => Ok(false),
            Err(error) => Err(object_error(error)),
        }
    }

    /// Every record, read again only when its ETag changed since the last listing.
    async fn all_records(&self) -> StoreResult<Vec<RunRecord>> {
        let keys: Vec<(String, String)> = self
            .objects
            .keys("runs/")
            .await
            .map_err(object_error)?
            .into_iter()
            .filter(|(key, _)| key.ends_with(&format!("/{RECORD}")) && key.matches('/').count() == 2)
            .collect();
        let cached: HashMap<String, (String, RunRecord)> = {
            let cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
            keys.iter().filter_map(|(key, _)| cache.get(key).map(|entry| (key.clone(), entry.clone()))).collect()
        };
        let objects = Arc::clone(&self.objects);
        let reads = futures::stream::iter(keys.into_iter().map(|(key, etag)| {
            let objects = Arc::clone(&objects);
            let known = cached.get(&key).filter(|(cached_etag, _)| *cached_etag == etag).cloned();
            async move {
                if let Some((etag, record)) = known {
                    return Ok::<_, StoreError>(Some((key, etag, record)));
                }
                let Some(object) = get(objects.as_ref(), &key).await.map_err(object_error)? else { return Ok(None) };
                match serde_json::from_slice::<RunRecord>(&object.bytes) {
                    Ok(record) => Ok(Some((key, object.etag, record))),
                    Err(e) => {
                        // One damaged record must not hide every other run.
                        eprintln!("warning: skipping run store entry `{key}`: {e}");
                        Ok(None)
                    }
                }
            }
        }))
        .buffer_unordered(LIST_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
        let mut records = Vec::new();
        let mut fresh = HashMap::new();
        for read in reads {
            if let Some((key, etag, record)) = read? {
                fresh.insert(key, (etag, record.clone()));
                records.push(record);
            }
        }
        *self.cache.lock().unwrap_or_else(PoisonError::into_inner) = fresh;
        Ok(records)
    }

    async fn delete_run(&self, run_id: &str) -> StoreResult<bool> {
        validate_run_id(run_id)?;
        let keys = self.objects.keys(&format!("runs/{run_id}/")).await.map_err(object_error)?;
        let existed = keys.iter().any(|(key, _)| *key == record_key(run_id));
        // The record goes last: a run is listed until everything of it is gone.
        for (key, _) in keys.iter().filter(|(key, _)| *key != record_key(run_id)) {
            self.objects.remove(key).await.map_err(object_error)?;
        }
        if existed {
            self.objects.remove(&record_key(run_id)).await.map_err(object_error)?;
        }
        Ok(existed)
    }
}

impl RunStore for SharedStore {
    fn create(&self, record: RunRecord) -> BoxFuture<'_, StoreResult<()>> {
        Box::pin(async move {
            validate_run_id(&record.run_id)?;
            match self.write_record(&record, Condition::Absent).await {
                Ok(_) => Ok(()),
                Err(ObjectError::Precondition) => {
                    Err(StoreError::Conflict(format!("run `{}` already exists", record.run_id)))
                }
                Err(error) => Err(object_error(error)),
            }
        })
    }

    fn put(&self, record: RunRecord) -> BoxFuture<'_, StoreResult<()>> {
        Box::pin(async move {
            for _ in 0..MAX_ATTEMPTS {
                let mut next = record.clone();
                let condition = match self.read_record(&record.run_id).await? {
                    Some((old, etag)) => {
                        keep_counters(&mut next, &old);
                        Condition::Matches(etag)
                    }
                    None => Condition::Absent,
                };
                match self.write_record(&next, condition).await {
                    Ok(_) => return Ok(()),
                    Err(ObjectError::Precondition) => continue,
                    Err(error) => return Err(object_error(error)),
                }
            }
            Err(busy(&record.run_id))
        })
    }

    fn put_if(&self, record: RunRecord, expected_epoch: u64) -> BoxFuture<'_, StoreResult<()>> {
        Box::pin(async move {
            let run_id = record.run_id.clone();
            self.update_record(&run_id, |stored| {
                check_epoch(stored, expected_epoch)?;
                let old = stored.clone();
                *stored = record.clone();
                keep_counters(stored, &old);
                Ok(true)
            })
            .await
            .map(|_| ())
        })
    }

    fn get<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<Option<RunRecord>>> {
        Box::pin(async move { Ok(self.read_record(run_id).await?.map(|(record, _)| record)) })
    }

    fn list(&self, filter: RunFilter) -> BoxFuture<'_, StoreResult<RunPage>> {
        Box::pin(async move {
            let mut records = self.all_records().await?;
            sort_newest_first(&mut records);
            Ok(page_of(records, &filter, MAX_PAGE))
        })
    }

    fn delete<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<bool>> {
        Box::pin(self.delete_run(run_id))
    }

    fn append_events<'a>(
        &'a self,
        run_id: &'a str,
        first_index: u64,
        events: &'a [StoredEvent],
    ) -> BoxFuture<'a, StoreResult<u64>> {
        Box::pin(self.append(run_id, first_index, events))
    }

    fn events<'a>(&'a self, run_id: &'a str, from: u64) -> BoxFuture<'a, StoreResult<Vec<StoredEvent>>> {
        Box::pin(async move {
            let (record, _) = self.require_record(run_id).await?;
            let limit = record.event_count.min(self.max_events as u64);
            if from >= limit {
                return Ok(Vec::new());
            }
            self.read_events(run_id, from, limit).await
        })
    }

    fn save_snapshot<'a>(&'a self, run_id: &'a str, snapshot: &'a Snapshot) -> BoxFuture<'a, StoreResult<()>> {
        Box::pin(async move {
            self.require_record(run_id).await?;
            let bytes = encode(snapshot)?;
            let key = format!("runs/{run_id}/{SNAPSHOT}");
            self.objects.write(&key, bytes, Condition::Always).await.map(|_| ()).map_err(object_error)
        })
    }

    fn load_snapshot<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<Option<Snapshot>>> {
        Box::pin(async move {
            validate_run_id(run_id)?;
            let key = format!("runs/{run_id}/{SNAPSHOT}");
            match get(self.objects.as_ref(), &key).await.map_err(object_error)? {
                Some(object) => serde_json::from_slice(&object.bytes)
                    .map(Some)
                    .map_err(|e| StoreError::Corrupt(format!("{key}: {e}"))),
                None => Ok(None),
            }
        })
    }

    fn claim_resume<'a>(&'a self, run_id: &'a str, now_ms: u64) -> BoxFuture<'a, StoreResult<RunRecord>> {
        Box::pin(async move { self.update_record(run_id, |record| claim(record, now_ms).map(|_| true)).await })
    }

    fn recover_interrupted(&self, now_ms: u64) -> BoxFuture<'_, StoreResult<usize>> {
        Box::pin(async move {
            if !self.background_recovery {
                return self.sweep_stale(now_ms).await;
            }
            let store = self.clone();
            graphrt_core::task::spawn(async move {
                match store.sweep_stale(now_ms).await {
                    Ok(0) => {}
                    Ok(marked) => eprintln!("marked {marked} stale running run(s) as interrupted"),
                    Err(error) => eprintln!("warning: cannot look for stale runs: {error}"),
                }
            });
            Ok(0)
        })
    }

    fn purge_before(&self, cutoff_ms: u64) -> BoxFuture<'_, StoreResult<usize>> {
        Box::pin(async move {
            let old: Vec<String> = self
                .all_records()
                .await?
                .into_iter()
                .filter(|record| purgeable(record, cutoff_ms))
                .map(|record| record.run_id)
                .collect();
            let mut purged = 0;
            for run_id in old {
                if self.delete_run(&run_id).await? {
                    purged += 1;
                }
            }
            Ok(purged)
        })
    }
}

#[cfg(feature = "scores")]
impl crate::scores::ScoreStore for SharedStore {
    fn put_scores<'a>(&'a self, run_id: &'a str, scores: Vec<crate::scores::Score>) -> BoxFuture<'a, StoreResult<()>> {
        Box::pin(async move {
            self.require_record(run_id).await?;
            let key = format!("runs/{run_id}/{SCORES}");
            for _ in 0..MAX_ATTEMPTS {
                let (mut stored, condition) = match get(self.objects.as_ref(), &key).await.map_err(object_error)? {
                    Some(object) => (read_scores(&object.bytes, &key)?, Condition::Matches(object.etag)),
                    None => (Vec::new(), Condition::Absent),
                };
                crate::scores::merge_scores(&mut stored, scores.clone(), run_id)?;
                let bytes = encode(&crate::scores::scores_json(&stored))?;
                match self.objects.write(&key, bytes, condition).await {
                    Ok(_) => return Ok(()),
                    Err(ObjectError::Precondition) => continue,
                    Err(error) => return Err(object_error(error)),
                }
            }
            Err(busy(run_id))
        })
    }

    fn scores<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<Vec<crate::scores::Score>>> {
        Box::pin(async move {
            self.require_record(run_id).await?;
            let key = format!("runs/{run_id}/{SCORES}");
            match get(self.objects.as_ref(), &key).await.map_err(object_error)? {
                Some(object) => read_scores(&object.bytes, &key),
                None => Ok(Vec::new()),
            }
        })
    }
}

#[cfg(feature = "scores")]
fn read_scores(bytes: &[u8], key: &str) -> StoreResult<Vec<crate::scores::Score>> {
    let corrupt = |e: &dyn std::fmt::Display| StoreError::Corrupt(format!("{key}: {e}"));
    let list: Vec<serde_json::Value> = serde_json::from_slice(bytes).map_err(|e| corrupt(&e))?;
    list.iter().map(|s| crate::scores::Score::from_json(s).ok_or_else(|| corrupt(&"invalid score"))).collect()
}

/// A coordination object's bytes: its version on the first line, then the value.
fn object_bytes(version: u64, value: &[u8]) -> Vec<u8> {
    let mut bytes = format!("{version}\n").into_bytes();
    bytes.extend_from_slice(value);
    bytes
}

fn parse_object(bytes: &[u8], key: &str) -> StoreResult<Versioned> {
    let split = bytes.iter().position(|byte| *byte == b'\n').ok_or_else(|| StoreError::Corrupt(key.to_owned()))?;
    let version = std::str::from_utf8(bytes.get(..split).unwrap_or_default())
        .ok()
        .and_then(|text| text.parse().ok())
        .ok_or_else(|| StoreError::Corrupt(key.to_owned()))?;
    Ok(Versioned { value: bytes.get(split + 1..).unwrap_or_default().to_vec(), version })
}

fn coord_key(key: &str) -> String {
    format!("coord/{key}")
}

impl SharedStore {
    async fn read_object(&self, key: &str) -> StoreResult<Option<(Versioned, String)>> {
        let full = coord_key(key);
        match self.objects.read(&full, None).await.map_err(object_error)? {
            Read::Object(object) => Ok(Some((parse_object(&object.bytes, key)?, object.etag))),
            Read::Unchanged | Read::Missing => Ok(None),
        }
    }
}

impl CoordinationStore for SharedStore {
    fn create_if_absent<'a>(&'a self, key: &'a str, value: Vec<u8>) -> BoxFuture<'a, StoreResult<u64>> {
        Box::pin(async move {
            validate_key(key)?;
            validate_value(&value)?;
            match self.objects.write(&coord_key(key), object_bytes(1, &value), Condition::Absent).await {
                Ok(_) => Ok(1),
                Err(ObjectError::Precondition) => Err(StoreError::Conflict(format!("key `{key}` already exists"))),
                Err(error) => Err(object_error(error)),
            }
        })
    }

    fn compare_and_swap<'a>(
        &'a self,
        key: &'a str,
        expected_version: u64,
        value: Vec<u8>,
    ) -> BoxFuture<'a, StoreResult<u64>> {
        Box::pin(async move {
            validate_key(key)?;
            validate_value(&value)?;
            let (current, etag) = self.read_object(key).await?.ok_or_else(|| StoreError::NotFound(key.to_owned()))?;
            check_version(key, current.version, expected_version)?;
            let version = expected_version.saturating_add(1);
            match self.objects.write(&coord_key(key), object_bytes(version, &value), Condition::Matches(etag)).await {
                Ok(_) => Ok(version),
                Err(ObjectError::Precondition) => {
                    Err(StoreError::Precondition(format!("key `{key}` changed, expected version {expected_version}")))
                }
                Err(error) => Err(object_error(error)),
            }
        })
    }

    fn get_object<'a>(&'a self, key: &'a str) -> BoxFuture<'a, StoreResult<Option<Versioned>>> {
        Box::pin(async move {
            validate_key(key)?;
            Ok(self.read_object(key).await?.map(|(object, _)| object))
        })
    }

    fn list_objects<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, StoreResult<Vec<(String, Versioned)>>> {
        Box::pin(async move {
            let keys = self.objects.keys(&coord_key(prefix)).await.map_err(object_error)?;
            let mut out = Vec::new();
            for (full, _) in keys {
                let Some(key) = full.strip_prefix("coord/") else { continue };
                if let Some((object, _)) = self.read_object(key).await? {
                    out.push((key.to_owned(), object));
                }
            }
            Ok(out)
        })
    }

    fn delete_object<'a>(&'a self, key: &'a str, expected_version: u64) -> BoxFuture<'a, StoreResult<bool>> {
        Box::pin(async move {
            validate_key(key)?;
            let Some((current, _)) = self.read_object(key).await? else { return Ok(false) };
            check_version(key, current.version, expected_version)?;
            self.objects.remove(&coord_key(key)).await.map_err(object_error)?;
            Ok(true)
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::store::objects::MemoryObjects;

    fn store() -> SharedStore {
        SharedStore::new(Arc::new(MemoryObjects::default()))
    }

    #[tokio::test]
    async fn passes_the_store_conformance_suite() {
        let store = store();
        graphrt_core::store_conformance::run_store_suite(&store).await;
        graphrt_core::store_conformance::append_retry_property(&store, 50).await;
        graphrt_core::store_conformance::coordination_suite(&store).await;
    }

    #[tokio::test]
    async fn a_running_run_of_another_live_server_is_not_marked_interrupted() {
        let store = store();
        let now = 10 * DEFAULT_STALE_RUN_MS;
        store.create(RunRecord::running("fresh", "wf", "fp", now - 1000)).await.unwrap();
        let mut old = RunRecord::running("old", "wf", "fp", 0);
        old.updated_ms = now - DEFAULT_STALE_RUN_MS - 1;
        store.create(old).await.unwrap();
        assert_eq!(store.recover_interrupted(now).await.unwrap(), 1);
        assert_eq!(store.get("fresh").await.unwrap().unwrap().status, RunStatus::Running);
        assert_eq!(store.get("old").await.unwrap().unwrap().status, RunStatus::Failed);
    }

    #[tokio::test]
    async fn two_stores_on_the_same_objects_claim_a_paused_run_once() {
        let objects: Arc<dyn Objects> = Arc::new(MemoryObjects::default());
        let (first, second) = (SharedStore::new(Arc::clone(&objects)), SharedStore::new(objects));
        let mut paused = RunRecord::running("p", "wf", "fp", 1);
        paused.status = RunStatus::Paused;
        first.create(paused).await.unwrap();
        let (a, b) = tokio::join!(first.claim_resume("p", 2), second.claim_resume("p", 2));
        assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1, "{a:?} {b:?}");
    }

    #[tokio::test]
    async fn listing_reads_only_changed_records_again() {
        let store = store();
        for id in ["a", "b"] {
            store.create(RunRecord::running(id, "wf", "fp", 1)).await.unwrap();
        }
        assert_eq!(store.list(RunFilter::default()).await.unwrap().runs.len(), 2);
        assert_eq!(store.cache.lock().unwrap().len(), 2);
        store.delete("a").await.unwrap();
        let page = store.list(RunFilter::default()).await.unwrap();
        assert_eq!(page.runs.iter().map(|run| run.run_id.as_str()).collect::<Vec<_>>(), ["b"]);
    }
}
