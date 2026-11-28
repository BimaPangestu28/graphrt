//! Run stores for `graphrt serve`: in-memory (default), a durable directory
//! store, and objects several servers share (`--store s3://bucket/prefix`).

mod file;
pub mod live;
mod location;
mod memory;
#[cfg(feature = "store-s3")]
pub mod objects;
#[cfg(feature = "store-s3")]
pub mod shared;
pub mod timed;

use std::path::PathBuf;
use std::sync::Arc;

pub use file::FileStore;
use graphrt_core::store::RunStore;
pub use location::StoreLocation;
pub use memory::{DEFAULT_MAX_EVENTS, DEFAULT_MAX_RUNS, MAX_PAGE, MemoryStore, status_name};
#[cfg(feature = "store-s3")]
pub use shared::SharedStore;

use crate::error::CliError;

/// Environment variable naming the store (a directory or `s3://bucket/prefix`) when `--store` is not given.
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

/// The location of `--store` (`dir`), else of `$GRAPHRT_STORE`.
///
/// @throws CliError (usage) - An `s3://` location that cannot be used.
pub fn location(dir: Option<&PathBuf>) -> Result<Option<StoreLocation>, CliError> {
    let env_dir = std::env::var(STORE_ENV).ok().filter(|v| !v.is_empty()).map(PathBuf::from);
    let Some(path) = dir.cloned().or(env_dir) else { return Ok(None) };
    let text = path.to_string_lossy().into_owned();
    location::parse_location(&text, |name| std::env::var(name).ok().filter(|value| !value.is_empty()))
        .map(Some)
        .map_err(|why| CliError::usage(format!("--store `{text}`: {why}")))
}

/// [`open`], with the score store of the same backend.
pub fn open_stores(dir: Option<&PathBuf>, max_runs: usize) -> Result<Stores, CliError> {
    match location(dir)? {
        Some(StoreLocation::Dir(dir)) => Ok(Stores::of(Arc::new(FileStore::open(&dir)?))),
        #[cfg(feature = "store-s3")]
        Some(StoreLocation::Objects(objects)) => {
            Ok(Stores::of(Arc::new(SharedStore::new(objects).recover_in_background())))
        }
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
