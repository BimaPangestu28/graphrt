//! Run stores for `graphrt serve`: in-memory (default) and a durable directory store.

mod file;
pub mod live;
mod memory;
pub mod timed;

use std::path::PathBuf;
use std::sync::Arc;

pub use file::FileStore;
use graphrt_core::store::RunStore;
pub use memory::{DEFAULT_MAX_EVENTS, DEFAULT_MAX_RUNS, MAX_PAGE, MemoryStore, status_name};

use crate::error::CliError;

/// Environment variable naming the store directory when `--store` is not given.
pub const STORE_ENV: &str = "GRAPHRT_STORE";

/// The stores of a server: runs and, with feature `scores`, their evaluation
/// scores (the same store object, so scores live and die with their run).
#[derive(Clone, Debug)]
pub struct Stores {
    pub runs: Arc<dyn RunStore>,
    #[cfg(feature = "scores")]
    pub scores: Arc<dyn crate::scores::ScoreStore>,
}

/// Open the store selected by `--store` / `$GRAPHRT_STORE` (memory when neither is
/// set). `max_runs` caps the in-memory store (0 = unlimited); the directory store
/// is bounded by `--store-ttl-days` instead.
pub fn open(dir: Option<&PathBuf>, max_runs: usize) -> Result<Arc<dyn RunStore>, CliError> {
    open_stores(dir, max_runs).map(|stores| stores.runs)
}

/// [`open`], with the score store of the same backend.
pub fn open_stores(dir: Option<&PathBuf>, max_runs: usize) -> Result<Stores, CliError> {
    let env_dir = std::env::var(STORE_ENV).ok().filter(|v| !v.is_empty()).map(PathBuf::from);
    match dir.cloned().or(env_dir) {
        Some(dir) => Ok(Stores::of(Arc::new(FileStore::open(&dir)?))),
        None => Ok(Stores::of(Arc::new(MemoryStore::new().max_runs(max_runs)))),
    }
}

impl Stores {
    /// Both stores backed by `store`.
    #[cfg(feature = "scores")]
    pub fn of<S: RunStore + crate::scores::ScoreStore + 'static>(store: Arc<S>) -> Self {
        Self { runs: store.clone(), scores: store }
    }

    /// The run store `store`.
    #[cfg(not(feature = "scores"))]
    pub fn of<S: RunStore + 'static>(store: Arc<S>) -> Self {
        Self { runs: store }
    }
}

/// Stored events from JSON texts, for store tests.
#[cfg(test)]
pub(crate) fn raw(texts: &[&str]) -> Vec<graphrt_core::store::StoredEvent> {
    texts.iter().filter_map(|text| serde_json::value::RawValue::from_string((*text).to_owned()).ok()).collect()
}

/// The JSON texts of stored events, for store tests.
#[cfg(test)]
pub(crate) fn texts(events: &[graphrt_core::store::StoredEvent]) -> Vec<&str> {
    events.iter().map(|event| event.get()).collect()
}
