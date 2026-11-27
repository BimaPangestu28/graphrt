//! Durable run records for hosts that serve many runs (an HTTP server, a job queue).
//!
//! A [`RunStore`] keeps, per run: a [`RunRecord`] (status, outputs, pause
//! reasons...), the run's events as JSON, and the [`Snapshot`] of a paused run, so
//! a run can be resumed by id after the client went away or the host restarted.
//!
//! Nothing secret is persisted: event values serialize with secrets masked, and
//! snapshots never contain secret plaintext (see [`crate::snapshot`]).
//!
//! # Writes that can be retried
//!
//! A host that writes through a network (a gateway in front of agents, an
//! object store) must be able to retry any write without corrupting the run:
//!
//! - [`RunStore::append_events`] takes the index of the first event of the
//!   batch. A batch that is already stored (a retry after a lost reply) is
//!   accepted without storing it twice; a batch that would leave a gap, or that
//!   differs from the events already stored at those indices, is refused.
//! - [`RunStore::put_if`] replaces a record only while its [`RunRecord::epoch`]
//!   is still the expected one, so a stale writer (a zombie replica, an
//!   execution that was taken over) cannot overwrite a newer one.
//! - Every claim ([`RunStore::claim_resume`], [`RunStore::claim_dispatch`])
//!   increments the epoch.

use std::fmt;

use futures::future::BoxFuture;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::event::{Event, PauseReason, Usage};
use crate::snapshot::Snapshot;
use crate::value::Value;

/// Lifecycle status of a stored run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RunStatus {
    /// Accepted but not dispatched to an executor yet (a gateway's queue).
    Queued,
    Running,
    Paused,
    Succeeded,
    Failed,
    Aborted,
}

impl RunStatus {
    /// Whether the run reached a final state.
    pub fn is_terminal(self) -> bool {
        matches!(self, RunStatus::Succeeded | RunStatus::Failed | RunStatus::Aborted)
    }

    /// Parse the wire name (`running`, `paused`, ...).
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "queued" => Some(RunStatus::Queued),
            "running" => Some(RunStatus::Running),
            "paused" => Some(RunStatus::Paused),
            "succeeded" => Some(RunStatus::Succeeded),
            "failed" => Some(RunStatus::Failed),
            "aborted" => Some(RunStatus::Aborted),
            _ => None,
        }
    }
}

/// Persisted summary of one run.
///
/// The struct is `#[non_exhaustive]`: build one with [`RunRecord::running`] or
/// [`RunRecord::queued`] and set fields afterwards.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RunRecord {
    pub run_id: String,
    /// Name the workflow is served under.
    pub workflow: String,
    /// Fingerprint of the workflow graph (resume requires the same graph).
    pub fingerprint: String,
    pub status: RunStatus,
    pub created_ms: u64,
    pub updated_ms: u64,
    /// Why the run is paused (empty unless `status == paused`).
    #[serde(default)]
    pub pause_reasons: Vec<PauseReason>,
    #[serde(default)]
    pub outputs: IndexMap<String, Value>,
    /// Ordered answer text assembled from response nodes.
    #[serde(default)]
    pub answer: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub error_type: Option<String>,
    #[serde(default)]
    pub usage: Usage,
    /// Index of the run's next event: how many events were appended so far. Stores
    /// keep at most their per-run cap of them (see `events_truncated`); the count
    /// keeps growing past it so event indices (and SSE `id:`s) stay stable.
    #[serde(default)]
    pub event_count: u64,
    /// Whether events were dropped because the store's per-run cap was reached.
    #[serde(default)]
    pub events_truncated: bool,
    /// Run inputs as the start node accepted them, secrets masked. Only kept when
    /// the host opts in (`graphrt serve --store-inputs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<IndexMap<String, Value>>,
    /// OpenTelemetry trace id (32 hex digits) of the latest execution, when traced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Span id (16 hex digits) of that execution's root `invoke_workflow` span, so
    /// later spans (online evaluation scores) can join the same trace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,
    /// Caller-supplied labels for filtering and evaluation (`RunRequest.metadata`).
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub metadata: IndexMap<String, String>,
    /// Caller's session / conversation id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Caller's end-user id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Whether `user_id` is the verified subject of an end-user token (the
    /// host checked its signature) rather than a label the caller sent. Only
    /// a verified owner may act on the run as that user (rate it, for example).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub user_verified: bool,
    /// Caller-supplied tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Deployment the run belongs to (the namespace of a gateway's store); `None`
    /// on a standalone server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment: Option<String>,
    /// Immutable version of that deployment the run executes (resume needs the
    /// same workflow graph).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_version: Option<String>,
    /// Execution epoch: 0 when created, incremented by every claim (dispatch,
    /// resume). Conditional writes ([`RunStore::put_if`]) compare it, and an
    /// executor refuses work for an epoch other than the one it was given.
    #[serde(default)]
    pub epoch: u64,
    /// Who executes the current epoch (a replica id), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

impl RunRecord {
    /// A new `running` record.
    pub fn running(
        run_id: impl Into<String>,
        workflow: impl Into<String>,
        fingerprint: impl Into<String>,
        now_ms: u64,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            workflow: workflow.into(),
            fingerprint: fingerprint.into(),
            status: RunStatus::Running,
            created_ms: now_ms,
            updated_ms: now_ms,
            pause_reasons: Vec::new(),
            outputs: IndexMap::new(),
            answer: String::new(),
            error: None,
            error_type: None,
            usage: Usage::default(),
            event_count: 0,
            events_truncated: false,
            inputs: None,
            trace_id: None,
            span_id: None,
            metadata: IndexMap::new(),
            session_id: None,
            user_id: None,
            user_verified: false,
            tags: Vec::new(),
            deployment: None,
            deployment_version: None,
            epoch: 0,
            owner: None,
        }
    }

    /// A new `queued` record (epoch 0) of `deployment`, waiting to be dispatched
    /// with [`RunStore::claim_dispatch`].
    pub fn queued(
        run_id: impl Into<String>,
        workflow: impl Into<String>,
        fingerprint: impl Into<String>,
        deployment: Option<String>,
        now_ms: u64,
    ) -> Self {
        let mut record = Self::running(run_id, workflow, fingerprint, now_ms);
        record.status = RunStatus::Queued;
        record.deployment = deployment;
        record
    }

    /// Apply a terminal event (`RunSucceeded`, `RunFailed`, `RunAborted`, `RunPaused`).
    /// Returns the snapshot of a paused run so the caller can persist it.
    pub fn apply_terminal(&mut self, event: &Event, now_ms: u64) -> Option<Snapshot> {
        self.updated_ms = now_ms;
        match event {
            Event::RunSucceeded { outputs, usage, .. } => {
                self.status = RunStatus::Succeeded;
                self.outputs.clone_from(outputs);
                self.usage = *usage;
                self.pause_reasons.clear();
                None
            }
            Event::RunFailed { error, error_type, usage, .. } => {
                self.status = RunStatus::Failed;
                self.error = Some(error.clone());
                self.error_type = Some(error_type.clone());
                self.usage = *usage;
                self.pause_reasons.clear();
                None
            }
            Event::RunAborted { reason, .. } => {
                self.status = RunStatus::Aborted;
                self.error = Some(reason.clone());
                self.pause_reasons.clear();
                None
            }
            Event::RunPaused { reasons, snapshot } => {
                self.status = RunStatus::Paused;
                self.pause_reasons.clone_from(reasons);
                self.usage = snapshot.usage;
                Some((**snapshot).clone())
            }
            _ => None,
        }
    }
}

/// Query for [`RunStore::list`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunFilter {
    pub workflow: Option<String>,
    pub status: Option<RunStatus>,
    /// Maximum records returned (stores cap this; 0 means the store default).
    pub limit: usize,
    /// Continue after this run id (the `next_cursor` of a previous page).
    pub cursor: Option<String>,
}

/// One page of [`RunStore::list`], newest first.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RunPage {
    pub runs: Vec<RunRecord>,
    pub next_cursor: Option<String>,
}

/// Store failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    #[error("run `{0}` not found")]
    NotFound(String),
    /// The operation conflicts with the run's current state (already exists,
    /// not paused, already being resumed).
    #[error("{0}")]
    Conflict(String),
    #[error("invalid run id `{0}`")]
    InvalidId(String),
    /// A conditional write found another epoch than the expected one (another
    /// writer got there first).
    #[error("{0}")]
    Precondition(String),
    #[error("run store I/O error: {0}")]
    Io(String),
    #[error("corrupt run store entry: {0}")]
    Corrupt(String),
}

/// Result type of [`RunStore`] operations.
pub type StoreResult<T> = Result<T, StoreError>;

/// Durable storage of run records, events and snapshots.
///
/// Implementations must make [`RunStore::claim_resume`] and [`RunStore::put_if`]
/// atomic: of two concurrent claims (or conditional writes) on the same run,
/// exactly one succeeds. [`RunStore::append_events`] must be idempotent per
/// index range (see the module docs). The `store-conformance` feature of this
/// crate ships an abstract test suite every implementation should pass.
pub trait RunStore: Send + Sync + fmt::Debug {
    /// Insert a new record. Fails with [`StoreError::Conflict`] if the id exists.
    fn create(&self, record: RunRecord) -> BoxFuture<'_, StoreResult<()>>;
    /// Insert or replace a record, unconditionally. Hosts with more than one
    /// writer use [`RunStore::put_if`] instead.
    fn put(&self, record: RunRecord) -> BoxFuture<'_, StoreResult<()>>;
    /// Replace the record of an existing run only while its stored `epoch` is
    /// `expected_epoch`. Fails with [`StoreError::Precondition`] otherwise, and
    /// with [`StoreError::NotFound`] when the run does not exist. The store keeps
    /// its own event counters, as with `put`.
    fn put_if(&self, record: RunRecord, expected_epoch: u64) -> BoxFuture<'_, StoreResult<()>>;
    fn get<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<Option<RunRecord>>>;
    fn list(&self, filter: RunFilter) -> BoxFuture<'_, StoreResult<RunPage>>;
    /// Remove a run and everything stored for it. Returns whether it existed.
    fn delete<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<bool>>;
    /// Append events (already serialized by [`event_to_stored`], secrets masked)
    /// whose first one has index `first_index`, and return the run's new
    /// [`RunRecord::event_count`].
    ///
    /// Idempotent: events at indices already stored must equal the stored ones
    /// and are skipped (a retried batch is not stored twice). Fails with
    /// [`StoreError::Conflict`] when `first_index` is past the run's event count
    /// (a gap) or an overlapping event differs. Stores may cap the number of
    /// events kept per run; they then set `events_truncated` and keep counting.
    fn append_events<'a>(
        &'a self,
        run_id: &'a str,
        first_index: u64,
        events: &'a [StoredEvent],
    ) -> BoxFuture<'a, StoreResult<u64>>;
    /// Stored events, starting at index `from`.
    fn events<'a>(&'a self, run_id: &'a str, from: u64) -> BoxFuture<'a, StoreResult<Vec<StoredEvent>>>;
    fn save_snapshot<'a>(&'a self, run_id: &'a str, snapshot: &'a Snapshot) -> BoxFuture<'a, StoreResult<()>>;
    fn load_snapshot<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<Option<Snapshot>>>;
    /// Atomically move a `paused` run to `running`, increment its epoch and
    /// return the updated record. Fails with [`StoreError::Conflict`] when the
    /// run is not paused (including when another request claimed it first).
    fn claim_resume<'a>(&'a self, run_id: &'a str, now_ms: u64) -> BoxFuture<'a, StoreResult<RunRecord>>;
    /// Compare-and-set a `queued` or `paused` run at `expected_epoch` to
    /// `running` at `expected_epoch + 1`, executed by `owner`, before the run is
    /// handed to that executor. Fails with [`StoreError::Precondition`] when the
    /// epoch moved, [`StoreError::Conflict`] when the run is in another status.
    ///
    /// The default implementation is `get` + [`RunStore::put_if`], which is
    /// atomic as long as `put_if` is.
    fn claim_dispatch<'a>(
        &'a self,
        run_id: &'a str,
        expected_epoch: u64,
        owner: Option<String>,
        now_ms: u64,
    ) -> BoxFuture<'a, StoreResult<RunRecord>> {
        Box::pin(async move {
            let mut record = self.get(run_id).await?.ok_or_else(|| StoreError::NotFound(run_id.to_owned()))?;
            if !matches!(record.status, RunStatus::Queued | RunStatus::Paused) {
                return Err(StoreError::Conflict(format!("run `{run_id}` is not queued or paused")));
            }
            check_epoch(&record, expected_epoch)?;
            record.status = RunStatus::Running;
            record.epoch = expected_epoch.saturating_add(1);
            record.owner = owner;
            record.updated_ms = now_ms;
            self.put_if(record.clone(), expected_epoch).await?;
            Ok(record)
        })
    }
    /// After a host restart: mark every `running` run as `failed` ("interrupted by
    /// restart"). Paused and queued runs are kept. Returns how many runs were marked.
    fn recover_interrupted(&self, now_ms: u64) -> BoxFuture<'_, StoreResult<usize>>;
    /// Delete runs (any status except `running` and `queued`) last updated before
    /// `cutoff_ms`.
    fn purge_before(&self, cutoff_ms: u64) -> BoxFuture<'_, StoreResult<usize>>;
    /// Store what an execution leaves when it ends, in one write: its last
    /// events, its snapshot when it paused, and its record (as
    /// [`RunStore::put_if`] at [`FinalWrite::expected_epoch`]). Idempotent like
    /// its parts, so a failed one can be retried.
    ///
    /// The default implementation is `append_events` (when there are events),
    /// `save_snapshot` (when there is one) and `put_if`, in that order. A store
    /// that serialises its writes (one lock) overrides it to take the lock once:
    /// when a shutdown stops many runs at once, each run's final state is then
    /// complete after one write, instead of every run waiting until three writes
    /// of each of them went through.
    fn finish<'a>(&'a self, write: FinalWrite<'a>) -> BoxFuture<'a, StoreResult<()>> {
        Box::pin(async move {
            let run_id = write.record.run_id.as_str();
            if !write.events.is_empty() {
                self.append_events(run_id, write.first_index, write.events).await?;
            }
            if let Some(snapshot) = write.snapshot {
                self.save_snapshot(run_id, snapshot).await?;
            }
            self.put_if(write.record.clone(), write.expected_epoch).await
        })
    }
}

/// The final state of an execution, stored by [`RunStore::finish`].
#[derive(Clone, Copy, Debug)]
pub struct FinalWrite<'a> {
    /// Index of the first of `events` in the run's log.
    pub first_index: u64,
    /// The execution's events not stored yet (possibly none), as for
    /// [`RunStore::append_events`].
    pub events: &'a [StoredEvent],
    /// The snapshot of a paused run.
    pub snapshot: Option<&'a Snapshot>,
    /// The run's final record. The store keeps its own event counters.
    pub record: &'a RunRecord,
    /// The epoch the execution was claimed at: the write is refused with
    /// [`StoreError::Precondition`] when the stored record is at another one.
    pub expected_epoch: u64,
}

/// [`StoreError::Precondition`] unless `record` is at `expected_epoch`.
pub fn check_epoch(record: &RunRecord, expected_epoch: u64) -> StoreResult<()> {
    if record.epoch == expected_epoch {
        Ok(())
    } else {
        Err(StoreError::Precondition(format!(
            "run `{}` is at epoch {}, expected {expected_epoch}",
            record.run_id, record.epoch
        )))
    }
}

/// How a batch passed to [`RunStore::append_events`] lines up with the events a
/// store already holds (shared by implementations).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppendPlan {
    /// Leading events of the batch that are already stored (a retry); they must
    /// equal the stored ones at `first_index..first_index + overlap`.
    pub overlap: usize,
    /// The run's event count after the append.
    pub new_count: u64,
}

/// Plan an append of `batch_len` events at `first_index` to a run holding
/// `event_count` events. Fails with [`StoreError::Conflict`] on a gap.
pub fn plan_append(run_id: &str, event_count: u64, first_index: u64, batch_len: usize) -> StoreResult<AppendPlan> {
    if first_index > event_count {
        return Err(StoreError::Conflict(format!(
            "run `{run_id}`: events must be appended at index {event_count}, not {first_index}"
        )));
    }
    let batch = u64::try_from(batch_len).unwrap_or(u64::MAX);
    let overlap = (event_count - first_index).min(batch);
    Ok(AppendPlan {
        overlap: usize::try_from(overlap).unwrap_or(usize::MAX),
        new_count: event_count.max(first_index.saturating_add(batch)),
    })
}

/// [`StoreError::Conflict`] for a retried event that differs from the stored one.
pub fn append_mismatch(run_id: &str, index: u64) -> StoreError {
    StoreError::Conflict(format!("run `{run_id}`: event {index} differs from the one already stored"))
}

/// Error message stored on runs interrupted by a host restart.
pub const INTERRUPTED_BY_RESTART: &str = "interrupted by restart";

/// An event serialized as one JSON object, the form in which stores keep events.
///
/// Holding the JSON text instead of a [`serde_json::Value`] tree saves building
/// a map per event on the way in and re-serializing it on the way out.
pub type StoredEvent = Box<serde_json::value::RawValue>;

/// Serialize an event for storage: the same JSON as [`event_to_json`], written
/// straight to text.
pub fn event_to_stored(event: &Event) -> StoredEvent {
    let text = match event {
        // Rare, and the snapshot has to be dropped: go through the tree.
        Event::RunPaused { .. } => serde_json::to_string(&event_to_json(event)),
        _ => serde_json::to_string(event),
    };
    text.ok()
        .and_then(|text| serde_json::value::RawValue::from_string(text).ok())
        .unwrap_or_else(|| serde_json::value::RawValue::NULL.to_owned())
}

/// The `type` field of a stored event (e.g. `node_started`), if it has one.
pub fn stored_event_type(event: &StoredEvent) -> Option<&str> {
    #[derive(Deserialize)]
    struct TypeOnly<'a> {
        #[serde(rename = "type", borrow)]
        kind: Option<&'a str>,
    }
    serde_json::from_slice::<TypeOnly<'_>>(event.get().as_bytes()).ok().and_then(|t| t.kind)
}

/// Serialize an event for storage. Secrets are masked by the value serializer; the
/// snapshot carried by `run_paused` is dropped (stores keep it separately).
pub fn event_to_json(event: &Event) -> serde_json::Value {
    let mut json = serde_json::to_value(event).unwrap_or(serde_json::Value::Null);
    if let Some(obj) = json.as_object_mut()
        && obj.get("type").and_then(serde_json::Value::as_str) == Some("run_paused")
    {
        obj.remove("snapshot");
    }
    json
}

/// Validate a run id used as a storage key: 1–128 chars of `[A-Za-z0-9_.-]`, not
/// starting with `.`.
pub fn validate_run_id(run_id: &str) -> StoreResult<()> {
    let ok = !run_id.is_empty()
        && run_id.len() <= 128
        && !run_id.starts_with('.')
        && run_id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok { Ok(()) } else { Err(StoreError::InvalidId(run_id.chars().take(64).collect())) }
}

/// Current Unix time in milliseconds (0 where no clock exists).
pub fn now_ms() -> u64 {
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        0
    }
    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }
}

/// Apply `filter` to records already sorted newest first (shared by implementations).
pub fn page_of(mut sorted: Vec<RunRecord>, filter: &RunFilter, max_limit: usize) -> RunPage {
    sorted.retain(|r| {
        filter.workflow.as_deref().is_none_or(|w| r.workflow == w) && filter.status.is_none_or(|s| r.status == s)
    });
    let start = match &filter.cursor {
        Some(cursor) => sorted.iter().position(|r| &r.run_id == cursor).map_or(sorted.len(), |i| i + 1),
        None => 0,
    };
    let limit = if filter.limit == 0 { max_limit } else { filter.limit.min(max_limit) };
    let runs: Vec<RunRecord> = sorted.into_iter().skip(start).take(limit.saturating_add(1)).collect();
    let mut runs = runs;
    let next_cursor = if runs.len() > limit {
        runs.truncate(limit);
        runs.last().map(|r| r.run_id.clone())
    } else {
        None
    };
    RunPage { runs, next_cursor }
}

/// Sort records newest first (ties broken by id, descending) for [`page_of`].
pub fn sort_newest_first(records: &mut [RunRecord]) {
    records.sort_unstable_by(|a, b| b.created_ms.cmp(&a.created_ms).then_with(|| b.run_id.cmp(&a.run_id)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: &str, wf: &str, created: u64, status: RunStatus) -> RunRecord {
        let mut r = RunRecord::running(id, wf, "fp", created);
        r.status = status;
        r
    }

    #[test]
    fn run_ids_are_validated() {
        assert!(validate_run_id("run-1a2b_3.c").is_ok());
        for bad in ["", "../x", ".hidden", "a/b", "a b", &"x".repeat(129)] {
            assert!(validate_run_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn paging_and_filtering() {
        let mut all = vec![
            rec("a", "w1", 1, RunStatus::Succeeded),
            rec("b", "w1", 2, RunStatus::Paused),
            rec("c", "w2", 3, RunStatus::Paused),
            rec("d", "w1", 4, RunStatus::Failed),
        ];
        sort_newest_first(&mut all);
        let page = page_of(all.clone(), &RunFilter { limit: 2, ..Default::default() }, 100);
        assert_eq!(page.runs.iter().map(|r| r.run_id.as_str()).collect::<Vec<_>>(), ["d", "c"]);
        assert_eq!(page.next_cursor.as_deref(), Some("c"));
        let next = page_of(all.clone(), &RunFilter { limit: 2, cursor: page.next_cursor, ..Default::default() }, 100);
        assert_eq!(next.runs.iter().map(|r| r.run_id.as_str()).collect::<Vec<_>>(), ["b", "a"]);
        assert_eq!(next.next_cursor, None);
        let paused = page_of(
            all.clone(),
            &RunFilter { status: Some(RunStatus::Paused), workflow: Some("w1".into()), ..Default::default() },
            100,
        );
        assert_eq!(paused.runs.len(), 1);
        assert_eq!(paused.runs[0].run_id, "b");
    }

    #[test]
    fn paused_event_json_drops_snapshot() {
        let snapshot = Snapshot {
            format: crate::snapshot::SNAPSHOT_FORMAT,
            run_id: "r".into(),
            graph_fingerprint: "fp".into(),
            elapsed_ms: 0,
            steps: 0,
            usage: Usage::default(),
            handled_errors: 0,
            root: crate::snapshot::FrameSnapshot::default(),
        };
        let event = Event::RunPaused { reasons: Vec::new(), snapshot: Box::new(snapshot) };
        let json = event_to_json(&event);
        assert_eq!(json["type"], "run_paused");
        assert!(json.get("snapshot").is_none());
        let stored = event_to_stored(&event);
        assert_eq!(serde_json::from_str::<serde_json::Value>(stored.get()).unwrap(), json);
        assert_eq!(stored_event_type(&stored), Some("run_paused"));
    }

    #[test]
    fn stored_event_text_matches_the_json_tree() {
        let mut outputs = IndexMap::new();
        outputs.insert("z".to_owned(), Value::from("last"));
        outputs.insert("a".to_owned(), Value::Secret(std::sync::Arc::from("sk-live")));
        outputs.insert("n".to_owned(), Value::from_json(serde_json::json!(1.5)));
        let event = Event::RunSucceeded { outputs, usage: Usage::default(), handled_errors: 0, elapsed_ms: 7 };
        let stored = event_to_stored(&event);
        assert_eq!(stored.get(), event_to_json(&event).to_string());
        assert!(!stored.get().contains("sk-live"));
        assert_eq!(stored_event_type(&stored), Some("run_succeeded"));
    }
}
