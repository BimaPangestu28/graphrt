//! `graphrt gateway import --from <serve store dir>`: copies every run of an
//! existing store (the `FileStore` of `graphrt serve --store`) into the
//! gateway's store, namespacing it under a deployment.
//!
//! Source-agnostic: it reads through the [`RunStore`] trait, so the CLI opens
//! the `FileStore` and hands it in. Records, events and snapshots are copied;
//! runs that already exist in the target are skipped (re-running an import is
//! safe). `running` runs are imported as `failed` (interrupted): their
//! executor is gone.

use graphrt_core::store::{INTERRUPTED_BY_RESTART, RunFilter, RunStatus, RunStore, StoreError};

/// Result of an import.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportSummary {
    /// Runs copied.
    pub imported: usize,
    /// Runs already present in the target.
    pub skipped: usize,
    /// Events copied.
    pub events: u64,
}

/// Copies all runs of `source` into `target` under `deployment`.
///
/// # Errors
/// The first store error other than an existing run.
pub async fn import_runs(source: &dyn RunStore, target: &dyn RunStore, deployment: &str) -> Result<ImportSummary, StoreError> {
    let mut summary = ImportSummary::default();
    let mut cursor = None;
    loop {
        let page = source.list(RunFilter { limit: 500, cursor: cursor.clone(), ..RunFilter::default() }).await?;
        for mut record in page.runs {
            let run_id = record.run_id.clone();
            let events = source.events(&run_id, 0).await?;
            let snapshot = source.load_snapshot(&run_id).await?;
            if record.status == RunStatus::Running {
                record.status = RunStatus::Failed;
                record.error = Some(INTERRUPTED_BY_RESTART.to_owned());
                record.error_type = Some("interrupted".to_owned());
            }
            if record.deployment.is_none() {
                record.deployment = Some(deployment.to_owned());
            }
            let first_index = record.event_count.saturating_sub(events.len() as u64);
            record.event_count = first_index;
            match target.create(record).await {
                Ok(()) => {}
                Err(StoreError::Conflict(_)) => {
                    summary.skipped += 1;
                    continue;
                }
                Err(error) => return Err(error),
            }
            if !events.is_empty() {
                target.append_events(&run_id, first_index, &events).await?;
                summary.events += events.len() as u64;
            }
            if let Some(snapshot) = snapshot {
                target.save_snapshot(&run_id, &snapshot).await?;
            }
            summary.imported += 1;
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(summary),
        }
    }
}

#[cfg(test)]
mod tests {
    use graphrt_core::store::RunRecord;

    use super::*;
    use crate::store::SqliteStore;

    #[tokio::test]
    async fn copies_records_events_and_is_rerunnable() {
        let source = SqliteStore::in_memory().unwrap();
        let target = SqliteStore::in_memory().unwrap();
        source.create(RunRecord::running("r1", "wf", "fp", 1)).await.unwrap();
        let events = graphrt_core::store_conformance::numbered_events(0..3);
        source.append_events("r1", 0, &events).await.unwrap();
        let mut done = RunRecord::running("r2", "wf", "fp", 2);
        done.status = RunStatus::Succeeded;
        source.create(done).await.unwrap();

        let summary = import_runs(&source, &target, "shop").await.unwrap();
        assert_eq!(summary, ImportSummary { imported: 2, skipped: 0, events: 3 });
        let imported = target.get("r1").await.unwrap().unwrap();
        assert_eq!(imported.status, RunStatus::Failed);
        assert_eq!(imported.deployment.as_deref(), Some("shop"));
        assert_eq!(imported.event_count, 3);
        assert_eq!(target.events("r1", 0).await.unwrap().len(), 3);
        let again = import_runs(&source, &target, "shop").await.unwrap();
        assert_eq!(again.skipped, 2);
    }
}
