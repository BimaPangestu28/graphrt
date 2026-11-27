//! Write latency of the run store, measured where `graphrt serve` uses it.
//!
//! The shutdown drain must preempt early enough for every active run to store
//! its final state before the deadline. That cost depends on the store, not on
//! the runs: [`super::FileStore`] serialises every write behind one store lock
//! and syncs several files per write, so on a disk busy with other work ten
//! runs stopping at once take ten times a slow write each. [`TimedStore`] wraps
//! the server's store and keeps a decaying maximum of its recent write
//! durations ([`WriteLatency`]), which [`crate::drain`] turns into its reserve.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use graphrt_core::Snapshot;
use graphrt_core::store::{FinalWrite, RunFilter, RunPage, RunRecord, RunStore, StoreResult, StoredEvent};

/// Decaying maximum of recent store write durations.
///
/// Each write sets the estimate to `max(sample, estimate − estimate / 8)`: one
/// slow write raises it at once, a run of fast ones lowers it gradually. It
/// never decays while no write happens, so an idle server keeps its last view
/// of the disk.
#[derive(Debug, Default)]
pub struct WriteLatency {
    micros: AtomicU64,
}

impl WriteLatency {
    /// Record one write that took `elapsed`.
    pub fn record(&self, elapsed: Duration) {
        let sample = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        // `fetch_update` is deprecated in favour of `try_update` on newer toolchains, but
        // `try_update` is not available on the 1.89 MSRV.
        #[allow(deprecated)]
        let _updated = self
            .micros
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| Some(sample.max(current - current / 8)));
    }

    /// The current estimate of one write.
    pub fn estimate(&self) -> Duration {
        Duration::from_micros(self.micros.load(Ordering::Acquire))
    }
}

/// A [`RunStore`] that records the duration of every write in a [`WriteLatency`].
#[derive(Debug)]
pub struct TimedStore {
    inner: Arc<dyn RunStore>,
    latency: Arc<WriteLatency>,
}

impl TimedStore {
    /// Wrap `inner`, recording into `latency`.
    pub fn new(inner: Arc<dyn RunStore>, latency: Arc<WriteLatency>) -> Self {
        Self { inner, latency }
    }

    fn timed<'a, T: Send + 'a>(&'a self, write: BoxFuture<'a, StoreResult<T>>) -> BoxFuture<'a, StoreResult<T>> {
        Box::pin(async move {
            let started = Instant::now();
            let result = write.await;
            self.latency.record(started.elapsed());
            result
        })
    }
}

impl RunStore for TimedStore {
    fn create(&self, record: RunRecord) -> BoxFuture<'_, StoreResult<()>> {
        self.timed(self.inner.create(record))
    }

    fn put(&self, record: RunRecord) -> BoxFuture<'_, StoreResult<()>> {
        self.timed(self.inner.put(record))
    }

    fn put_if(&self, record: RunRecord, expected_epoch: u64) -> BoxFuture<'_, StoreResult<()>> {
        self.timed(self.inner.put_if(record, expected_epoch))
    }

    fn get<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<Option<RunRecord>>> {
        self.inner.get(run_id)
    }

    fn list(&self, filter: RunFilter) -> BoxFuture<'_, StoreResult<RunPage>> {
        self.inner.list(filter)
    }

    fn delete<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<bool>> {
        self.timed(self.inner.delete(run_id))
    }

    fn append_events<'a>(
        &'a self,
        run_id: &'a str,
        first_index: u64,
        events: &'a [StoredEvent],
    ) -> BoxFuture<'a, StoreResult<u64>> {
        self.timed(self.inner.append_events(run_id, first_index, events))
    }

    fn events<'a>(&'a self, run_id: &'a str, from: u64) -> BoxFuture<'a, StoreResult<Vec<StoredEvent>>> {
        self.inner.events(run_id, from)
    }

    fn save_snapshot<'a>(&'a self, run_id: &'a str, snapshot: &'a Snapshot) -> BoxFuture<'a, StoreResult<()>> {
        self.timed(self.inner.save_snapshot(run_id, snapshot))
    }

    fn load_snapshot<'a>(&'a self, run_id: &'a str) -> BoxFuture<'a, StoreResult<Option<Snapshot>>> {
        self.inner.load_snapshot(run_id)
    }

    fn claim_resume<'a>(&'a self, run_id: &'a str, now_ms: u64) -> BoxFuture<'a, StoreResult<RunRecord>> {
        self.timed(self.inner.claim_resume(run_id, now_ms))
    }

    fn claim_dispatch<'a>(
        &'a self,
        run_id: &'a str,
        expected_epoch: u64,
        owner: Option<String>,
        now_ms: u64,
    ) -> BoxFuture<'a, StoreResult<RunRecord>> {
        self.timed(self.inner.claim_dispatch(run_id, expected_epoch, owner, now_ms))
    }

    fn recover_interrupted(&self, now_ms: u64) -> BoxFuture<'_, StoreResult<usize>> {
        self.inner.recover_interrupted(now_ms)
    }

    fn finish<'a>(&'a self, write: FinalWrite<'a>) -> BoxFuture<'a, StoreResult<()>> {
        self.timed(self.inner.finish(write))
    }

    fn purge_before(&self, cutoff_ms: u64) -> BoxFuture<'_, StoreResult<usize>> {
        self.inner.purge_before(cutoff_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_estimate_jumps_to_a_slow_write_and_decays_over_fast_ones() {
        let latency = WriteLatency::default();
        assert_eq!(latency.estimate(), Duration::ZERO);
        latency.record(Duration::from_millis(80));
        assert_eq!(latency.estimate(), Duration::from_millis(80));
        latency.record(Duration::from_millis(1));
        assert_eq!(latency.estimate(), Duration::from_millis(70));
        for _ in 0..60 {
            latency.record(Duration::from_micros(100));
        }
        assert_eq!(latency.estimate(), Duration::from_micros(100));
        latency.record(Duration::from_millis(30));
        assert_eq!(latency.estimate(), Duration::from_millis(30));
    }

    #[tokio::test]
    async fn the_wrapper_keeps_the_store_contract() {
        let latency = Arc::new(WriteLatency::default());
        let store = TimedStore::new(Arc::new(super::super::MemoryStore::new()), Arc::clone(&latency));
        graphrt_core::store_conformance::run_store_suite(&store).await;
        graphrt_core::store_conformance::append_retry_property(&store, 20).await;
        assert!(latency.estimate() < Duration::from_secs(1), "an in-memory store writes in microseconds");
    }
}
