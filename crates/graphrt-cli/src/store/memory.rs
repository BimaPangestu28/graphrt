//! In-process [`RunStore`]. Contents are lost when the process exits.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use futures::future::{BoxFuture, ready};
use graphrt_core::Snapshot;
use graphrt_core::store::{
    INTERRUPTED_BY_RESTART, RunFilter, RunPage, RunRecord, RunStatus, RunStore, StoreError, StoreResult, StoredEvent,
    page_of, sort_newest_first, validate_run_id,
};

/// Default per-run event cap.
pub const DEFAULT_MAX_EVENTS: usize = 5_000;
/// Default cap on runs kept in memory (`graphrt serve --max-runs`). A finished
/// run with its events takes roughly 10-20 KiB, so the default stays around
/// 10-20 MiB however long the server runs.
pub const DEFAULT_MAX_RUNS: usize = 1_000;
/// Largest page returned by `list`.
pub const MAX_PAGE: usize = 500;

#[derive(Debug, Default)]
struct Entry {
    record: Option<RunRecord>,
    events: Vec<StoredEvent>,
    snapshot: Option<Snapshot>,
}

/// Run store kept in memory.
#[derive(Debug)]
pub struct MemoryStore {
    runs: Mutex<HashMap<String, Entry>>,
    max_events: usize,
    /// 0 = unlimited.
    max_runs: usize,
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStore {
    pub fn new() -> Self {
        Self { runs: Mutex::new(HashMap::new()), max_events: DEFAULT_MAX_EVENTS, max_runs: DEFAULT_MAX_RUNS }
    }

    /// Keep at most `max` runs (0 = unlimited). When a new run would exceed it,
    /// the oldest finished runs are dropped; running and paused runs are always
    /// kept, so they can finish or be resumed.
    pub fn max_runs(mut self, max: usize) -> Self {
        self.max_runs = max;
        self
    }

    /// Keep at most `max` events per run.
    pub fn max_events(mut self, max: usize) -> Self {
        self.max_events = max;
        self
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        // Every mutation below replaces whole values, so a panic while the lock was
        // held cannot leave a half-written entry behind.
        self.runs.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Drop the oldest finished runs once `runs` exceeds `max_runs`, down to 90 %
/// of the cap so the sort is not repeated on every new run.
fn evict_finished(runs: &mut HashMap<String, Entry>, max_runs: usize) {
    if max_runs == 0 || runs.len() <= max_runs {
        return;
    }
    let target = max_runs - max_runs / 10;
    let mut finished: Vec<(u64, String)> = runs
        .iter()
        .filter_map(|(id, e)| e.record.as_ref().filter(|r| r.status.is_terminal()).map(|r| (r.updated_ms, id.clone())))
        .collect();
    finished.sort_unstable();
    let excess = runs.len().saturating_sub(target);
    for (_, id) in finished.into_iter().take(excess) {
        runs.remove(&id);
    }
}

fn existing<'m>(runs: &'m mut HashMap<String, Entry>, run_id: &str) -> StoreResult<&'m mut Entry> {
    runs.get_mut(run_id).filter(|e| e.record.is_some()).ok_or_else(|| StoreError::NotFound(run_id.to_owned()))
}

impl RunStore for MemoryStore {
    fn create(&self, record: RunRecord) -> BoxFuture<'_, StoreResult<()>> {
        let result = validate_run_id(&record.run_id).and_then(|()| {
            let mut runs = self.lock();
            if runs.get(&record.run_id).is_some_and(|e| e.record.is_some()) {
                return Err(StoreError::Conflict(format!("run `{}` already exists", record.run_id)));
            }
            runs.insert(record.run_id.clone(), Entry { record: Some(record), ..Entry::default() });
            evict_finished(&mut runs, self.max_runs);
            Ok(())
        });
        Box::pin(ready(result))
    }

    fn put(&self, record: RunRecord) -> BoxFuture<'_, StoreResult<()>> {
        let result = validate_run_id(&record.run_id).map(|()| {
            let mut runs = self.lock();
            let entry = runs.entry(record.run_id.clone()).or_default();
            // Keep the event counters the store maintains.
            let (count, truncated) = entry.record.as_ref().map_or((0, false), |r| (r.event_count, r.events_truncated));
            let mut record = record;
            record.event_count = record.event_count.max(count);
            record.events_truncated |= truncated;
            entry.record = Some(record);
        });
        Box::pin(ready(result))
    }

    fn get<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<Option<RunRecord>>> {
        let result = validate_run_id(run_id).map(|()| self.lock().get(run_id).and_then(|e| e.record.clone()));
        Box::pin(ready(result))
    }

    fn list(&self, filter: RunFilter) -> BoxFuture<'_, StoreResult<RunPage>> {
        let mut records: Vec<RunRecord> = self.lock().values().filter_map(|e| e.record.clone()).collect();
        sort_newest_first(&mut records);
        Box::pin(ready(Ok(page_of(records, &filter, MAX_PAGE))))
    }

    fn delete<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<bool>> {
        let result = validate_run_id(run_id).map(|()| self.lock().remove(run_id).is_some_and(|e| e.record.is_some()));
        Box::pin(ready(result))
    }

    fn append_events<'a>(&'a self, run_id: &'a str, events: Vec<StoredEvent>) -> BoxFuture<'a, StoreResult<()>> {
        let max = self.max_events;
        let result = validate_run_id(run_id).and_then(|()| {
            let mut runs = self.lock();
            let entry = existing(&mut runs, run_id)?;
            let room = max.saturating_sub(entry.events.len());
            let dropped = events.len() > room;
            entry.events.extend(events.into_iter().take(room));
            let count = u64::try_from(entry.events.len()).unwrap_or(u64::MAX);
            if let Some(record) = entry.record.as_mut() {
                record.event_count = count;
                record.events_truncated |= dropped;
            }
            Ok(())
        });
        Box::pin(ready(result))
    }

    fn events<'a>(&'a self, run_id: &'a str, from: u64) -> BoxFuture<'a, StoreResult<Vec<StoredEvent>>> {
        let result = validate_run_id(run_id).and_then(|()| {
            let mut runs = self.lock();
            let entry = existing(&mut runs, run_id)?;
            let skip = usize::try_from(from).unwrap_or(usize::MAX);
            Ok(entry.events.iter().skip(skip).cloned().collect())
        });
        Box::pin(ready(result))
    }

    fn save_snapshot<'a>(&'a self, run_id: &'a str, snapshot: &'a Snapshot) -> BoxFuture<'a, StoreResult<()>> {
        let result = validate_run_id(run_id).and_then(|()| {
            let mut runs = self.lock();
            existing(&mut runs, run_id)?.snapshot = Some(snapshot.clone());
            Ok(())
        });
        Box::pin(ready(result))
    }

    fn load_snapshot<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<Option<Snapshot>>> {
        let result = validate_run_id(run_id).map(|()| self.lock().get(run_id).and_then(|e| e.snapshot.clone()));
        Box::pin(ready(result))
    }

    fn claim_resume<'a>(&'a self, run_id: &'a str, now_ms: u64) -> BoxFuture<'a, StoreResult<RunRecord>> {
        let result = validate_run_id(run_id).and_then(|()| {
            let mut runs = self.lock();
            let entry = existing(&mut runs, run_id)?;
            let record = entry.record.as_mut().ok_or_else(|| StoreError::NotFound(run_id.to_owned()))?;
            claim(record, now_ms)
        });
        Box::pin(ready(result))
    }

    fn recover_interrupted(&self, now_ms: u64) -> BoxFuture<'_, StoreResult<usize>> {
        let mut runs = self.lock();
        let mut marked = 0;
        for record in runs.values_mut().filter_map(|e| e.record.as_mut()) {
            if mark_interrupted(record, now_ms) {
                marked += 1;
            }
        }
        Box::pin(ready(Ok(marked)))
    }

    fn purge_before(&self, cutoff_ms: u64) -> BoxFuture<'_, StoreResult<usize>> {
        let mut runs = self.lock();
        let before = runs.len();
        runs.retain(|_, e| e.record.as_ref().is_none_or(|r| !purgeable(r, cutoff_ms)));
        Box::pin(ready(Ok(before - runs.len())))
    }
}

/// Paused -> running, or a conflict.
pub(super) fn claim(record: &mut RunRecord, now_ms: u64) -> StoreResult<RunRecord> {
    if record.status != RunStatus::Paused {
        return Err(StoreError::Conflict(format!(
            "run `{}` is {}, not paused (already resumed or finished)",
            record.run_id,
            status_name(record.status)
        )));
    }
    record.status = RunStatus::Running;
    record.updated_ms = now_ms;
    Ok(record.clone())
}

/// Wire name of a status.
pub fn status_name(status: RunStatus) -> String {
    serde_json::to_value(status).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
}

/// Running -> failed after a restart. Returns whether the record changed.
pub(super) fn mark_interrupted(record: &mut RunRecord, now_ms: u64) -> bool {
    if record.status != RunStatus::Running {
        return false;
    }
    record.status = RunStatus::Failed;
    record.error = Some(INTERRUPTED_BY_RESTART.to_owned());
    record.error_type = Some("interrupted".to_owned());
    record.updated_ms = now_ms;
    true
}

/// Whether TTL cleanup may delete this record (never a running one).
pub(super) fn purgeable(record: &RunRecord, cutoff_ms: u64) -> bool {
    record.status != RunStatus::Running && record.updated_ms < cutoff_ms
}

#[cfg(test)]
mod tests {
    use futures::executor::block_on;

    use super::*;

    #[test]
    fn oldest_finished_runs_are_evicted_and_active_runs_kept() {
        let store = MemoryStore::new().max_runs(10);
        let mut paused = RunRecord::running("paused", "wf", "fp", 0);
        paused.status = RunStatus::Paused;
        block_on(store.create(paused)).unwrap();
        block_on(store.create(RunRecord::running("running", "wf", "fp", 0))).unwrap();
        for i in 1..=20u64 {
            let mut done = RunRecord::running(format!("done{i:02}"), "wf", "fp", i);
            done.status = RunStatus::Succeeded;
            done.updated_ms = i;
            block_on(store.create(done)).unwrap();
            assert!(store.lock().len() <= 10, "cap exceeded after {i} runs");
        }
        let kept = block_on(store.list(RunFilter::default())).unwrap().runs;
        let ids: Vec<&str> = kept.iter().map(|r| r.run_id.as_str()).collect();
        assert!(ids.contains(&"paused") && ids.contains(&"running"), "{ids:?}");
        assert!(ids.contains(&"done20"), "newest finished run kept: {ids:?}");
        assert!(!ids.contains(&"done01"), "oldest finished run evicted: {ids:?}");
    }

    #[test]
    fn zero_max_runs_means_unlimited() {
        let store = MemoryStore::new().max_runs(0);
        for i in 0..50u64 {
            let mut done = RunRecord::running(format!("r{i}"), "wf", "fp", i);
            done.status = RunStatus::Succeeded;
            block_on(store.create(done)).unwrap();
        }
        assert_eq!(store.lock().len(), 50);
    }

    #[test]
    fn claim_is_exclusive_and_events_are_capped() {
        let store = MemoryStore::new().max_events(3);
        let mut rec = RunRecord::running("r1", "wf", "fp", 1);
        rec.status = RunStatus::Paused;
        block_on(store.create(rec)).unwrap();
        assert!(matches!(
            block_on(store.create(RunRecord::running("r1", "wf", "fp", 1))),
            Err(StoreError::Conflict(_))
        ));
        assert_eq!(block_on(store.claim_resume("r1", 2)).unwrap().status, RunStatus::Running);
        assert!(matches!(block_on(store.claim_resume("r1", 3)), Err(StoreError::Conflict(_))));

        block_on(store.append_events("r1", crate::store::raw(&["1", "2"]))).unwrap();
        block_on(store.append_events("r1", crate::store::raw(&["3", "4"]))).unwrap();
        let rec = block_on(store.get("r1")).unwrap().unwrap();
        assert_eq!(rec.event_count, 3);
        assert!(rec.events_truncated);
        assert_eq!(crate::store::texts(&block_on(store.events("r1", 1)).unwrap()), ["2", "3"]);

        assert_eq!(block_on(store.recover_interrupted(9)).unwrap(), 1);
        assert_eq!(block_on(store.get("r1")).unwrap().unwrap().status, RunStatus::Failed);
        assert_eq!(block_on(store.purge_before(100)).unwrap(), 1);
        assert!(block_on(store.get("r1")).unwrap().is_none());
    }
}
