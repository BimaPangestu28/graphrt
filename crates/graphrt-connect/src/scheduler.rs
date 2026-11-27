//! [`Scheduler`]: syncs sources when their schedule says so, and on request.
//!
//! A background task wakes every [`TICK`] (or sooner when asked), starts the
//! syncs that are due, at most [`Scheduler::with_max_parallel`] at once, and
//! never two syncs of the same source. Syncs run as their own tasks, so
//! workflows keep running while a source syncs. The lock in the shared
//! [`SyncState`] keeps a second process (the desktop app next to
//! `graphrt serve`) from syncing the same source at the same time: a sync it
//! holds is skipped here and the source is due again only if the other
//! process's sync did not bring it up to date.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use indexmap::IndexMap;
use serde::Serialize;
use tokio::sync::{Notify, Semaphore};

use crate::config::SourceConfig;
use crate::error::{ConnectError, Result};
use crate::schedule::SyncSchedule;
use crate::state::{SourceStatus, SyncOutcome, SyncState};
use crate::sync::{DocumentSink, SyncOptions, sync_source, unix_now};
use crate::types::Connector;

/// Longest sleep between two checks for due sources.
pub const TICK: Duration = Duration::from_secs(30);

struct Entry {
    config: SourceConfig,
    connector: std::result::Result<Arc<dyn Connector>, ConnectError>,
}

struct Inner {
    sources: RwLock<IndexMap<String, Entry>>,
    sink: Arc<dyn DocumentSink>,
    state: Arc<dyn SyncState>,
    options: SyncOptions,
    running: Mutex<HashSet<String>>,
    parallel: Semaphore,
    wake: Notify,
}

/// Schedules and runs the syncs of a set of sources. Cheap to clone (shared).
#[derive(Clone)]
pub struct Scheduler {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scheduler").field("sources", &self.read().keys().collect::<Vec<_>>()).finish()
    }
}

/// A source as shown by UIs and the HTTP API.
#[derive(Clone, Debug, Serialize)]
pub struct SourceView {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub dataset: String,
    pub schedule: SyncSchedule,
    /// Unix seconds of the next scheduled sync (`None` for manual sources).
    pub next_sync_at: Option<i64>,
    pub syncing: bool,
    pub status: SourceStatus,
    /// Why the connector could not be built (a missing secret, a bad setting).
    pub config_error: Option<String>,
}

impl Scheduler {
    pub fn new(sink: Arc<dyn DocumentSink>, state: Arc<dyn SyncState>, options: SyncOptions) -> Self {
        Self {
            inner: Arc::new(Inner {
                sources: RwLock::new(IndexMap::new()),
                sink,
                state,
                options,
                running: Mutex::new(HashSet::new()),
                parallel: Semaphore::new(2),
                wake: Notify::new(),
            }),
        }
    }

    /// Syncs of different sources running at once (default 2).
    ///
    /// Call before the scheduler is shared or started.
    pub fn with_max_parallel(self, max: usize) -> Self {
        let extra = max.clamp(1, 16).saturating_sub(self.inner.parallel.available_permits());
        self.inner.parallel.add_permits(extra);
        self
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, IndexMap<String, Entry>> {
        self.inner.sources.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn running(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.inner.running.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Add (or replace) a source with its connector, or the error that kept
    /// the connector from being built (shown on the source, retried never).
    pub fn add(&self, config: SourceConfig, connector: Result<Box<dyn Connector>>) {
        let entry = Entry { config, connector: connector.map(Arc::from) };
        self.inner.sources.write().unwrap_or_else(PoisonError::into_inner).insert(entry.config.id.clone(), entry);
        self.inner.wake.notify_one();
    }

    /// Remove a source (its documents stay in the knowledge base).
    pub fn remove(&self, source_id: &str) -> bool {
        self.inner.sources.write().unwrap_or_else(PoisonError::into_inner).shift_remove(source_id).is_some()
    }

    pub fn contains(&self, source_id: &str) -> bool {
        self.read().contains_key(source_id)
    }

    /// Every source with its status and next sync time.
    pub fn sources(&self) -> Vec<SourceView> {
        let now = unix_now();
        let running = self.running().clone();
        self.read()
            .values()
            .map(|entry| {
                let status =
                    self.inner.state.status(&entry.config.id).ok().flatten().unwrap_or_else(|| SourceStatus {
                        source_id: entry.config.id.clone(),
                        ..SourceStatus::default()
                    });
                SourceView {
                    id: entry.config.id.clone(),
                    name: entry.config.display_name().to_owned(),
                    kind: entry.config.kind.clone(),
                    dataset: entry.config.dataset.clone(),
                    schedule: entry.config.schedule.clone(),
                    next_sync_at: entry.config.schedule.next_due(status.last_finished_at, now).map(|due| due.max(now)),
                    syncing: running.contains(&entry.config.id) || status.outcome == SyncOutcome::Running,
                    status,
                    config_error: entry.connector.as_ref().err().map(ToString::to_string),
                }
            })
            .collect()
    }

    /// One source's view.
    pub fn source(&self, source_id: &str) -> Option<SourceView> {
        self.sources().into_iter().find(|view| view.id == source_id)
    }

    fn entry(&self, source_id: &str) -> Result<(SourceConfig, Arc<dyn Connector>)> {
        let sources = self.read();
        let entry =
            sources.get(source_id).ok_or_else(|| ConnectError::NotFound(format!("unknown source `{source_id}`")))?;
        let connector = entry.connector.clone()?;
        Ok((entry.config.clone(), connector))
    }

    /// Sync one source now and wait for the result.
    ///
    /// @throws `ConnectError::NotFound` for an unknown source, `Busy` while it
    /// syncs (here or in another process), the connector's build error, or a
    /// sync state failure.
    pub async fn sync_now(&self, source_id: &str) -> Result<SourceStatus> {
        let (config, connector) = self.entry(source_id)?;
        if !self.running().insert(source_id.to_owned()) {
            return Err(ConnectError::Busy(format!("`{}` is already syncing", config.display_name())));
        }
        let _done = RunningGuard { scheduler: self, source_id };
        let _turn = self
            .inner
            .parallel
            .acquire()
            .await
            .map_err(|_| ConnectError::Busy("the scheduler is shutting down".into()))?;
        sync_source(
            &config,
            connector.as_ref(),
            self.inner.sink.as_ref(),
            self.inner.state.as_ref(),
            &self.inner.options,
        )
        .await
    }

    /// Start a sync of one source in the background.
    ///
    /// @throws `ConnectError::NotFound`, `Busy` or the connector's build error, at once.
    pub fn trigger(&self, source_id: &str) -> Result<()> {
        let (config, _) = self.entry(source_id)?;
        if self.running().contains(source_id) {
            return Err(ConnectError::Busy(format!("`{}` is already syncing", config.display_name())));
        }
        let scheduler = self.clone();
        let id = source_id.to_owned();
        tokio::spawn(async move {
            if let Err(error) = scheduler.sync_now(&id).await
                && !matches!(error, ConnectError::Busy(_))
            {
                eprintln!("knowledge sync of `{id}` failed: {error}");
            }
        });
        Ok(())
    }

    /// Start the syncs due at `now`. Returns the ids started.
    pub fn tick(&self, now: i64) -> Vec<String> {
        let due: Vec<String> = {
            let running = self.running();
            self.read()
                .values()
                .filter(|entry| entry.connector.is_ok() && !running.contains(&entry.config.id))
                .filter(|entry| {
                    let last =
                        self.inner.state.status(&entry.config.id).ok().flatten().and_then(|s| s.last_finished_at);
                    entry.config.schedule.is_due(last, now)
                })
                .map(|entry| entry.config.id.clone())
                .collect()
        };
        for id in &due {
            // Errors (busy in another process) are recorded or retried on the next tick.
            let _started = self.trigger(id);
        }
        due
    }

    /// Seconds until the next source is due (capped at [`TICK`]).
    fn sleep_for(&self, now: i64) -> Duration {
        let next = self
            .read()
            .values()
            .filter_map(|entry| {
                let last = self.inner.state.status(&entry.config.id).ok().flatten().and_then(|s| s.last_finished_at);
                entry.config.schedule.next_due(last, now)
            })
            .min();
        match next {
            Some(due) if due > now => Duration::from_secs(u64::try_from(due - now).unwrap_or(1)).min(TICK),
            Some(_) => Duration::from_secs(1),
            None => TICK,
        }
    }

    /// Run the schedule until the returned handle is aborted (or the runtime stops).
    pub fn start(&self) -> tokio::task::JoinHandle<()> {
        let scheduler = self.clone();
        tokio::spawn(async move {
            loop {
                let now = unix_now();
                scheduler.tick(now);
                let pause = scheduler.sleep_for(now).max(Duration::from_secs(1));
                let sleep = std::pin::pin!(tokio::time::sleep(pause));
                let woken = std::pin::pin!(scheduler.inner.wake.notified());
                let _first = futures::future::select(sleep, woken).await;
            }
        })
    }
}

struct RunningGuard<'a> {
    scheduler: &'a Scheduler,
    source_id: &'a str,
}

impl Drop for RunningGuard<'_> {
    fn drop(&mut self) {
        self.scheduler.running().remove(self.source_id);
    }
}
