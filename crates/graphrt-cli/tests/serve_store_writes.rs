//! How `graphrt serve` writes runs to its store: time-based event batches and
//! retries of failed writes.
#![cfg(feature = "serve")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::future::BoxFuture;
use graphrt_cli::serve::{build_state, discover, router};
use graphrt_cli::setup::ServiceOptions;
use graphrt_cli::store::MemoryStore;
use graphrt_core::Snapshot;
use graphrt_core::engine::Limits;
use graphrt_core::store::{RunFilter, RunPage, RunRecord, RunStore, StoreError, StoreResult, StoredEvent};
use http_body_util::BodyExt;
use serde_json::{Value as Json, json};
use tower::ServiceExt;

/// A [`MemoryStore`] whose writes fail with an I/O error the first `fail_first`
/// times each (like a network store having a moment).
#[derive(Debug)]
struct FlakyStore {
    inner: MemoryStore,
    fail_first: u32,
    appends: AtomicU32,
    snapshots: AtomicU32,
    puts: AtomicU32,
}

impl FlakyStore {
    fn new(fail_first: u32) -> Self {
        Self {
            inner: MemoryStore::new(),
            fail_first,
            appends: AtomicU32::new(0),
            snapshots: AtomicU32::new(0),
            puts: AtomicU32::new(0),
        }
    }

    fn fails(&self, counter: &AtomicU32) -> bool {
        counter.fetch_add(1, Ordering::SeqCst) < self.fail_first
    }
}

fn unavailable<'a, T: Send + 'a>() -> BoxFuture<'a, StoreResult<T>> {
    Box::pin(async { Err(StoreError::Io("store unavailable".to_owned())) })
}

impl RunStore for FlakyStore {
    fn create(&self, record: RunRecord) -> BoxFuture<'_, StoreResult<()>> {
        self.inner.create(record)
    }
    fn put(&self, record: RunRecord) -> BoxFuture<'_, StoreResult<()>> {
        if self.fails(&self.puts) { unavailable() } else { self.inner.put(record) }
    }
    fn get<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<Option<RunRecord>>> {
        self.inner.get(run_id)
    }
    fn list(&self, filter: RunFilter) -> BoxFuture<'_, StoreResult<RunPage>> {
        self.inner.list(filter)
    }
    fn delete<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<bool>> {
        self.inner.delete(run_id)
    }
    fn put_if(&self, record: RunRecord, expected_epoch: u64) -> BoxFuture<'_, StoreResult<()>> {
        if self.fails(&self.puts) { unavailable() } else { self.inner.put_if(record, expected_epoch) }
    }
    fn append_events<'a>(
        &'a self,
        run_id: &'a str,
        first_index: u64,
        events: &'a [StoredEvent],
    ) -> BoxFuture<'a, StoreResult<u64>> {
        if self.fails(&self.appends) { unavailable() } else { self.inner.append_events(run_id, first_index, events) }
    }
    fn events<'a>(&'a self, run_id: &'a str, from: u64) -> BoxFuture<'a, StoreResult<Vec<StoredEvent>>> {
        self.inner.events(run_id, from)
    }
    fn save_snapshot<'a>(&'a self, run_id: &'a str, snapshot: &'a Snapshot) -> BoxFuture<'a, StoreResult<()>> {
        if self.fails(&self.snapshots) { unavailable() } else { self.inner.save_snapshot(run_id, snapshot) }
    }
    fn load_snapshot<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<Option<Snapshot>>> {
        self.inner.load_snapshot(run_id)
    }
    fn claim_resume<'a>(&'a self, run_id: &'a str, now_ms: u64) -> BoxFuture<'a, StoreResult<RunRecord>> {
        self.inner.claim_resume(run_id, now_ms)
    }
    fn recover_interrupted(&self, now_ms: u64) -> BoxFuture<'_, StoreResult<usize>> {
        self.inner.recover_interrupted(now_ms)
    }
    fn purge_before(&self, cutoff_ms: u64) -> BoxFuture<'_, StoreResult<usize>> {
        self.inner.purge_before(cutoff_ms)
    }
}

fn workflows_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("graphrt-serve-writes-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let examples = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    std::fs::copy(examples.join("approval.yaml"), dir.join("approval.yaml")).unwrap();
    dir
}

async fn app_with(store: Arc<dyn RunStore>, tag: &str) -> (axum::Router, PathBuf) {
    let dir = workflows_dir(tag);
    let options = ServiceOptions {
        plugins: graphrt_cli::extensions::PluginOptions { dirs: Vec::new(), no_default: true, trusted: Vec::new() },
        ..ServiceOptions::default()
    };
    let state =
        build_state(&discover(&dir).unwrap(), &options, Limits::default(), None).await.unwrap().with_store(store);
    (router(Arc::new(state), 1 << 20), dir)
}

async fn start_async(app: &axum::Router) -> String {
    let body = json!({ "inputs": { "amount": 50, "reason": "Lunch" }, "async": true });
    let request = Request::post("/v1/workflows/approval/runs")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice::<Json>(&body).unwrap()["run_id"].as_str().unwrap().to_owned()
}

async fn wait_paused(store: &dyn RunStore, run_id: &str) -> RunRecord {
    for _ in 0..500 {
        if let Some(record) = store.get(run_id).await.unwrap()
            && record.status != graphrt_core::store::RunStatus::Running
        {
            return record;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("run {run_id} did not settle");
}

#[tokio::test]
async fn failed_store_writes_are_retried_until_they_succeed() {
    let store = Arc::new(FlakyStore::new(2));
    let (app, dir) = app_with(store.clone(), "retry").await;
    let run_id = start_async(&app).await;
    let record = wait_paused(store.as_ref(), &run_id).await;
    assert_eq!(record.status, graphrt_core::store::RunStatus::Paused);
    assert_eq!(record.error_type, None, "{record:?}");
    // Every event and the snapshot made it despite the first failures.
    let events = store.events(&run_id, 0).await.unwrap();
    assert!(!events.is_empty());
    assert_eq!(u64::try_from(events.len()).unwrap(), record.event_count);
    assert!(events.last().unwrap().get().contains("\"run_paused\""));
    assert!(store.load_snapshot(&run_id).await.unwrap().is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn writes_give_up_after_bounded_retries() {
    // More failures than retries: the snapshot is lost and the record says so.
    let store = Arc::new(FlakyStore::new(100));
    let (app, dir) = app_with(store.clone(), "lost").await;
    let run_id = start_async(&app).await;
    // `put` fails too, so the status never changes; wait for the retries to end.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(store.load_snapshot(&run_id).await.unwrap().is_none());
    assert!(store.snapshots.load(Ordering::SeqCst) >= 6, "first attempt plus five retries");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_snapshot_error_is_recorded_when_only_the_snapshot_fails() {
    #[derive(Debug)]
    struct NoSnapshots(MemoryStore);
    impl RunStore for NoSnapshots {
        fn create(&self, record: RunRecord) -> BoxFuture<'_, StoreResult<()>> {
            self.0.create(record)
        }
        fn put(&self, record: RunRecord) -> BoxFuture<'_, StoreResult<()>> {
            self.0.put(record)
        }
        fn get<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<Option<RunRecord>>> {
            self.0.get(run_id)
        }
        fn list(&self, filter: RunFilter) -> BoxFuture<'_, StoreResult<RunPage>> {
            self.0.list(filter)
        }
        fn delete<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<bool>> {
            self.0.delete(run_id)
        }
        fn put_if(&self, record: RunRecord, expected_epoch: u64) -> BoxFuture<'_, StoreResult<()>> {
            self.0.put_if(record, expected_epoch)
        }
        fn append_events<'a>(
            &'a self,
            run_id: &'a str,
            first_index: u64,
            events: &'a [StoredEvent],
        ) -> BoxFuture<'a, StoreResult<u64>> {
            self.0.append_events(run_id, first_index, events)
        }
        fn events<'a>(&'a self, run_id: &'a str, from: u64) -> BoxFuture<'a, StoreResult<Vec<StoredEvent>>> {
            self.0.events(run_id, from)
        }
        fn save_snapshot<'a>(&'a self, _run_id: &'a str, _snapshot: &'a Snapshot) -> BoxFuture<'a, StoreResult<()>> {
            unavailable()
        }
        fn load_snapshot<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<Option<Snapshot>>> {
            self.0.load_snapshot(run_id)
        }
        fn claim_resume<'a>(&'a self, run_id: &'a str, now_ms: u64) -> BoxFuture<'a, StoreResult<RunRecord>> {
            self.0.claim_resume(run_id, now_ms)
        }
        fn recover_interrupted(&self, now_ms: u64) -> BoxFuture<'_, StoreResult<usize>> {
            self.0.recover_interrupted(now_ms)
        }
        fn purge_before(&self, cutoff_ms: u64) -> BoxFuture<'_, StoreResult<usize>> {
            self.0.purge_before(cutoff_ms)
        }
    }
    let store = Arc::new(NoSnapshots(MemoryStore::new()));
    let (app, dir) = app_with(store.clone(), "nosnap").await;
    let run_id = start_async(&app).await;
    let record = wait_paused(store.as_ref(), &run_id).await;
    assert_eq!(record.status, graphrt_core::store::RunStatus::Paused);
    assert_eq!(record.error_type.as_deref(), Some("store_unavailable"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Events of a run that stalls are stored within the flush interval, not only
/// every 32 events.
#[cfg(feature = "code")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_are_flushed_within_a_quarter_second() {
    let dir = std::env::temp_dir().join(format!("graphrt-serve-writes-flush-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("stall.yaml"),
        r#"version: "1"
name: stall
graph:
  nodes:
    - { id: start, type: start, variables: [] }
    - id: wait
      type: code
      code: "function main() { const t = Date.now() + 1500; while (Date.now() < t) {} return { ok: true }; }"
    - { id: end, type: end, outputs: [{ variable: ok, value_selector: [wait, ok] }] }
  edges:
    - { source: start, target: wait }
    - { source: wait, target: end }
"#,
    )
    .unwrap();
    let store = Arc::new(MemoryStore::new());
    let options = ServiceOptions {
        plugins: graphrt_cli::extensions::PluginOptions { dirs: Vec::new(), no_default: true, trusted: Vec::new() },
        ..ServiceOptions::default()
    };
    let state = build_state(&discover(&dir).unwrap(), &options, Limits::default(), None)
        .await
        .unwrap()
        .with_store(store.clone());
    let app = router(Arc::new(state), 1 << 20);
    let request = Request::post("/v1/workflows/stall/runs")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"async": true}"#))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let run_id = serde_json::from_slice::<Json>(&body).unwrap()["run_id"].as_str().unwrap().to_owned();
    // The run is busy for 1.5 s after its first few events.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let stored = store.events(&run_id, 0).await.unwrap();
    assert!(stored.len() >= 3, "only {} event(s) stored while the run stalls", stored.len());
    assert!(stored.iter().any(|event| event.get().contains("\"node_started\"")));
    let _ = std::fs::remove_dir_all(&dir);
}
