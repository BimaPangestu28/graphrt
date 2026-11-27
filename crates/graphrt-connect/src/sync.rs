//! One sync of one source: list, skip what did not change, index what is new
//! or changed, remove what is gone, record the outcome.
//!
//! - An item listed with the version it had last time is not fetched at all.
//!   Without a version, its content hash decides whether it is indexed again.
//! - Items are fetched and indexed a few at a time ([`SyncOptions::concurrency`]).
//! - Items missing from a listing are removed only when the listing is
//!   complete: a listing cut short by a limit or an unreadable sub folder
//!   never deletes anything, and a failed listing changes nothing.
//! - An item that cannot be fetched or indexed keeps its previous passages.
//! - The source's lock (in the [`SyncState`]) is held for the whole sync, so
//!   two processes sharing a state file never sync the same source at once.

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::future::BoxFuture;
use sha2::{Digest, Sha256};

use crate::config::SourceConfig;
use crate::error::{ConnectError, Result};
use crate::state::{ItemState, SourceStatus, StatusError, SyncOutcome, SyncReport, SyncState};
use crate::types::{Connector, ItemContent, RemoteItem};

/// Warnings kept per sync (the count of the rest is reported).
const MAX_WARNINGS: usize = 20;

/// A document handed to the [`DocumentSink`].
#[derive(Clone, Debug)]
pub struct SyncDocument<'a> {
    pub source: &'a SourceConfig,
    pub item: &'a RemoteItem,
    /// The knowledge base document id: see [`document_id`].
    pub document_id: String,
    pub content: ItemContent,
}

/// Where synced documents go: a graphrt-rag knowledge base
/// ([`crate::KnowledgeSink`]) or the host's own indexing pipeline.
pub trait DocumentSink: Send + Sync + std::fmt::Debug {
    /// Index (or re-index) one document. Returns its chunk count.
    fn upsert<'a>(&'a self, document: SyncDocument<'a>) -> BoxFuture<'a, Result<u64>>;
    /// Remove a document that is gone from its source.
    fn remove<'a>(&'a self, source: &'a SourceConfig, document_id: &'a str) -> BoxFuture<'a, Result<()>>;
}

/// Options of one sync.
#[derive(Clone, Debug)]
pub struct SyncOptions {
    /// Items fetched and indexed at the same time.
    pub concurrency: usize,
    /// Who holds the lock (`desktop`, `serve@host:pid`).
    pub holder: String,
    /// How long the lock is valid without renewal; it is renewed while the sync runs.
    pub lock_ttl: Duration,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self { concurrency: 4, holder: format!("graphrt:{}", std::process::id()), lock_ttl: Duration::from_secs(600) }
    }
}

/// Seconds since 1970.
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
}

/// The knowledge base document id of an item: `<source>:<item>` when the item
/// id is short and plain, `<source>:<hash>` otherwise (URLs, paths), so ids
/// are safe in HTTP paths and stable across syncs.
pub fn document_id(source_id: &str, item_id: &str) -> String {
    let plain = !item_id.is_empty()
        && item_id.len() <= 96
        && item_id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if plain {
        format!("{source_id}:{item_id}")
    } else {
        let digest = Sha256::digest(item_id.as_bytes());
        let hex: String = digest.iter().take(12).map(|byte| format!("{byte:02x}")).collect();
        format!("{source_id}:{hex}")
    }
}

/// SHA-256 of content, hex.
pub fn content_hash(content: &ItemContent) -> String {
    Sha256::digest(content.hash_input()).iter().map(|byte| format!("{byte:02x}")).collect()
}

enum ItemOutcome {
    Unchanged,
    Indexed { chunks: u64 },
    Failed { message: String },
}

/// Holds the source lock, renewing it while the sync runs, and releases it at the end.
struct LockGuard<'a> {
    state: &'a dyn SyncState,
    source_id: &'a str,
    holder: &'a str,
    ttl: i64,
    renewed: Mutex<Instant>,
}

impl LockGuard<'_> {
    fn renew(&self) {
        let Ok(mut renewed) = self.renewed.lock() else { return };
        if renewed.elapsed() < Duration::from_secs(u64::try_from(self.ttl / 3).unwrap_or(60)) {
            return;
        }
        *renewed = Instant::now();
        let now = unix_now();
        // A failed renewal only shortens the lock; the sync itself is still correct.
        let _renewed = self.state.try_lock(self.source_id, self.holder, now, now + self.ttl);
    }
}

impl Drop for LockGuard<'_> {
    fn drop(&mut self) {
        let _released = self.state.unlock(self.source_id, self.holder);
    }
}

/// Sync one source into `sink`.
///
/// Problems with the source (listing failed, items failed) are recorded in
/// the returned (and stored) [`SourceStatus`]; `Err` is returned only when the
/// lock is taken (`ConnectError::Busy`) or the sync state cannot be written.
pub async fn sync_source(
    source: &SourceConfig,
    connector: &dyn Connector,
    sink: &dyn DocumentSink,
    state: &dyn SyncState,
    options: &SyncOptions,
) -> Result<SourceStatus> {
    let started = Instant::now();
    let now = unix_now();
    let ttl = i64::try_from(options.lock_ttl.as_secs()).unwrap_or(600).max(30);
    if !state.try_lock(&source.id, &options.holder, now, now + ttl)? {
        return Err(ConnectError::Busy(format!("`{}` is already syncing", source.display_name())));
    }
    let lock =
        LockGuard { state, source_id: &source.id, holder: &options.holder, ttl, renewed: Mutex::new(Instant::now()) };
    let previous = state.status(&source.id)?.unwrap_or_default();
    let mut status = SourceStatus {
        source_id: source.id.clone(),
        outcome: SyncOutcome::Running,
        last_started_at: Some(now),
        ..previous.clone()
    };
    state.put_status(&status)?;

    let listing = match connector.list().await {
        Ok(listing) => listing,
        Err(error) => {
            status.outcome = SyncOutcome::Failed;
            status.error = Some(StatusError { kind: error.kind().to_owned(), message: error.to_string() });
            status.warnings = Vec::new();
            status.report = SyncReport::default();
            status.last_finished_at = Some(unix_now());
            status.duration_ms = Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
            state.put_status(&status)?;
            drop(lock);
            return Ok(status);
        }
    };
    let known = state.items(&source.id)?;
    let mut report = SyncReport::default();
    let mut warnings: Vec<String> = listing.warnings.iter().take(MAX_WARNINGS).cloned().collect();
    let mut first_error: Option<ConnectError> = None;
    let mut seen: HashSet<&str> = HashSet::new();
    let mut work: Vec<&RemoteItem> = Vec::new();
    for item in &listing.items {
        if !seen.insert(item.id.as_str()) {
            continue;
        }
        let unchanged = item.version.is_some()
            && known.get(&item.id).is_some_and(|old| old.version == item.version && old.title == item.title);
        if unchanged {
            report.unchanged += 1;
        } else {
            work.push(item);
        }
    }

    let concurrency = options.concurrency.clamp(1, 16);
    // Boxed so the stream's type does not carry the closures' lifetimes (keeps the future `Send`).
    let lock_ref = &lock;
    let known_ref = &known;
    let tasks: Vec<BoxFuture<'_, (&RemoteItem, Result<ItemOutcome>)>> = work
        .into_iter()
        .map(|item| -> BoxFuture<'_, (&RemoteItem, Result<ItemOutcome>)> {
            Box::pin(async move {
                let outcome = sync_item(source, connector, sink, state, item, known_ref.get(&item.id)).await;
                lock_ref.renew();
                (item, outcome)
            })
        })
        .collect();
    let mut results = futures::stream::iter(tasks).buffer_unordered(concurrency);
    while let Some((item, outcome)) = results.next().await {
        match outcome {
            Ok(ItemOutcome::Unchanged) => report.unchanged += 1,
            Ok(ItemOutcome::Indexed { chunks }) => {
                report.indexed += 1;
                report.chunks += chunks;
            }
            Ok(ItemOutcome::Failed { message }) => {
                report.failed += 1;
                if warnings.len() < MAX_WARNINGS {
                    warnings.push(format!("{}: {message}", item.title));
                }
            }
            Err(error) => {
                // The sync state itself failed: stop, keep the index as it is.
                report.failed += 1;
                first_error.get_or_insert(error);
                break;
            }
        }
    }
    drop(results);

    if first_error.is_none() {
        if listing.complete {
            for gone in known.keys().filter(|id| !seen.contains(id.as_str())) {
                let id = document_id(&source.id, gone);
                match sink.remove(source, &id).await {
                    Ok(()) => {
                        state.remove_item(&source.id, gone)?;
                        report.removed += 1;
                    }
                    Err(error) => {
                        if warnings.len() < MAX_WARNINGS {
                            warnings.push(format!("could not remove a deleted item: {error}"));
                        }
                    }
                }
            }
        } else if known.keys().any(|id| !seen.contains(id.as_str())) {
            warnings.push("the listing was incomplete, so nothing was removed this time".into());
        }
    }

    status.report = report;
    status.document_count = u64::try_from(state.items(&source.id)?.len()).unwrap_or(u64::MAX);
    status.last_finished_at = Some(unix_now());
    status.duration_ms = Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
    let all_failed = report.failed > 0 && report.indexed == 0 && report.unchanged == 0;
    if let Some(error) = first_error {
        status.outcome = SyncOutcome::Failed;
        status.error = Some(StatusError { kind: error.kind().to_owned(), message: error.to_string() });
    } else if all_failed {
        status.outcome = SyncOutcome::Failed;
        let message = warnings.first().cloned().unwrap_or_else(|| "no item could be indexed".to_owned());
        status.error = Some(StatusError { kind: "items_failed".into(), message });
    } else {
        status.outcome = if report.failed == 0 && warnings.is_empty() {
            SyncOutcome::Succeeded
        } else {
            SyncOutcome::PartlySucceeded
        };
        status.error = None;
        status.last_success_at = status.last_finished_at;
    }
    status.warnings = warnings;
    state.put_status(&status)?;
    drop(lock);
    Ok(status)
}

async fn sync_item(
    source: &SourceConfig,
    connector: &dyn Connector,
    sink: &dyn DocumentSink,
    state: &dyn SyncState,
    item: &RemoteItem,
    previous: Option<&ItemState>,
) -> Result<ItemOutcome> {
    let content = match &item.content {
        Some(content) => content.clone(),
        None => match connector.fetch(item).await {
            Ok(content) => content,
            Err(error) => return Ok(ItemOutcome::Failed { message: error.to_string() }),
        },
    };
    let hash = content_hash(&content);
    let record = ItemState {
        item_id: item.id.clone(),
        version: item.version.clone(),
        content_hash: hash.clone(),
        title: item.title.clone(),
    };
    if previous.is_some_and(|old| old.content_hash == hash && old.title == item.title) {
        // Same content under a new version (a touched file): remember the version only.
        state.put_item(&source.id, &record)?;
        return Ok(ItemOutcome::Unchanged);
    }
    let document = SyncDocument { source, item, document_id: document_id(&source.id, &item.id), content };
    match sink.upsert(document).await {
        Ok(chunks) => {
            state.put_item(&source.id, &record)?;
            Ok(ItemOutcome::Indexed { chunks })
        }
        // The previous passages stay searchable; the next sync tries again.
        Err(error) => Ok(ItemOutcome::Failed { message: error.to_string() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_ids_are_path_safe() {
        assert_eq!(document_id("drive", "1AbC_d-9"), "drive:1AbC_d-9");
        let hashed = document_id("site", "https://graphrt.com/docs?a=1");
        assert!(hashed.starts_with("site:") && hashed.len() == "site:".len() + 24, "{hashed}");
        assert_eq!(hashed, document_id("site", "https://graphrt.com/docs?a=1"));
    }
}
