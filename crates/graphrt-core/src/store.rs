//! Durable run records for hosts that serve many runs (an HTTP server, a job queue).
//!
//! A [`RunStore`] keeps, per run: a [`RunRecord`] (status, outputs, pause
//! reasons...), the run's events as JSON, and the [`Snapshot`] of a paused run, so
//! a run can be resumed by id after the client went away or the host restarted.
//!
//! Nothing secret is persisted: event values serialize with secrets masked, and
//! snapshots never contain secret plaintext (see [`crate::snapshot`]).

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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    /// Events stored so far (may stop growing at the store's cap).
    #[serde(default)]
    pub event_count: u64,
    /// Whether events were dropped because the store's per-run cap was reached.
    #[serde(default)]
    pub events_truncated: bool,
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
        }
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
    #[error("run store I/O error: {0}")]
    Io(String),
    #[error("corrupt run store entry: {0}")]
    Corrupt(String),
}

/// Result type of [`RunStore`] operations.
pub type StoreResult<T> = Result<T, StoreError>;

/// Durable storage of run records, events and snapshots.
///
/// Implementations must make [`RunStore::claim_resume`] atomic: of two concurrent
/// claims on the same paused run, exactly one succeeds.
pub trait RunStore: Send + Sync + fmt::Debug {
    /// Insert a new record. Fails with [`StoreError::Conflict`] if the id exists.
    fn create(&self, record: RunRecord) -> BoxFuture<'_, StoreResult<()>>;
    /// Insert or replace a record.
    fn put(&self, record: RunRecord) -> BoxFuture<'_, StoreResult<()>>;
    fn get<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<Option<RunRecord>>>;
    fn list(&self, filter: RunFilter) -> BoxFuture<'_, StoreResult<RunPage>>;
    /// Remove a run and everything stored for it. Returns whether it existed.
    fn delete<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<bool>>;
    /// Append events (already serialized by [`event_to_stored`], secrets masked).
    /// Stores may cap the number of events kept per run; they then set
    /// `events_truncated`.
    fn append_events<'a>(&'a self, run_id: &'a str, events: Vec<StoredEvent>) -> BoxFuture<'a, StoreResult<()>>;
    /// Stored events, starting at index `from`.
    fn events<'a>(&'a self, run_id: &'a str, from: u64) -> BoxFuture<'a, StoreResult<Vec<StoredEvent>>>;
    fn save_snapshot<'a>(&'a self, run_id: &'a str, snapshot: &'a Snapshot) -> BoxFuture<'a, StoreResult<()>>;
    fn load_snapshot<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<Option<Snapshot>>>;
    /// Atomically move a `paused` run to `running` and return the updated record.
    /// Fails with [`StoreError::Conflict`] when the run is not paused (including when
    /// another request claimed it first).
    fn claim_resume<'a>(&'a self, run_id: &'a str, now_ms: u64) -> BoxFuture<'a, StoreResult<RunRecord>>;
    /// After a host restart: mark every `running` run as `failed` ("interrupted by
    /// restart"). Paused runs stay resumable. Returns how many runs were marked.
    fn recover_interrupted(&self, now_ms: u64) -> BoxFuture<'_, StoreResult<usize>>;
    /// Delete runs (any status except `running`) last updated before `cutoff_ms`.
    fn purge_before(&self, cutoff_ms: u64) -> BoxFuture<'_, StoreResult<usize>>;
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
    serde_json::from_str::<TypeOnly<'_>>(event.get()).ok().and_then(|t| t.kind)
}

/// Serialize an event for storage. Secrets are masked by the value serializer; the
/// snapshot carried by `run_paused` is dropped (stores keep it separately).
pub fn event_to_json(event: &Event) -> serde_json::Value {
    let mut json = serde_json::to_value(event).unwrap_or(serde_json::Value::Null);
    if let Some(obj) = json.as_object_mut() {
        if obj.get("type").and_then(serde_json::Value::as_str) == Some("run_paused") {
            obj.remove("snapshot");
        }
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
    records.sort_by(|a, b| b.created_ms.cmp(&a.created_ms).then_with(|| b.run_id.cmp(&a.run_id)));
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
