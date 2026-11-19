//! Run stores for `graphrt serve`: in-memory (default) and a durable directory store.

mod file;
pub mod live;
mod memory;

use std::path::PathBuf;
use std::sync::Arc;

pub use file::FileStore;
use graphrt_core::store::RunStore;
pub use memory::{DEFAULT_MAX_EVENTS, DEFAULT_MAX_RUNS, MAX_PAGE, MemoryStore, status_name};

use crate::error::CliError;

/// Environment variable naming the store directory when `--store` is not given.
pub const STORE_ENV: &str = "GRAPHRT_STORE";

/// Open the store selected by `--store` / `$GRAPHRT_STORE` (memory when neither is
/// set). `max_runs` caps the in-memory store (0 = unlimited); the directory store
/// is bounded by `--store-ttl-days` instead.
pub fn open(dir: Option<&PathBuf>, max_runs: usize) -> Result<Arc<dyn RunStore>, CliError> {
    let env_dir = std::env::var(STORE_ENV).ok().filter(|v| !v.is_empty()).map(PathBuf::from);
    match dir.cloned().or(env_dir) {
        Some(dir) => Ok(Arc::new(FileStore::open(&dir)?)),
        None => Ok(Arc::new(MemoryStore::new().max_runs(max_runs))),
    }
}
