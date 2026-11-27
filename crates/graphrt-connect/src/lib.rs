//! # graphrt-connect
//!
//! Knowledge connectors: keep a graphrt knowledge base in sync with the places
//! a team keeps its knowledge, incrementally and on a schedule.
//!
//! - [`Connector`]: lists the items of a source and fetches their content.
//!   Built in (one cargo feature each): `web` (pages, crawls, sitemaps),
//!   `api` (REST / JSON), `google_drive`, `notion`, `sharepoint`.
//!   [`ConnectorRegistry::register`] adds more without changing this crate.
//! - [`sync_source`]: one incremental sync. Unchanged items are skipped by
//!   version (ETag, modified time, revision) or content hash, changed ones are
//!   indexed again, deleted ones are removed, failures keep old passages.
//! - [`DocumentSink`]: where documents go. [`KnowledgeSink`] (feature `rag`)
//!   uses graphrt-rag's own extraction, chunking and embedding.
//! - [`SyncState`]: item versions, last status and the sync lock.
//!   [`SqliteSyncState`] (feature `sqlite`) shares them between processes.
//! - [`SyncSchedule`] and [`Scheduler`]: manual, every N hours, daily at a
//!   time, or cron, run in the background without blocking workflows.
//!
//! Credentials are named in configs, never written there (see [`AuthConfig`]),
//! every request goes through graphrt-http's egress policy (no private or
//! cloud metadata addresses unless a source allows them), and downloads are
//! size and time limited.
//!
//! ```no_run
//! # async fn demo() -> Result<(), graphrt_connect::ConnectError> {
//! use std::sync::Arc;
//! use graphrt_connect::{
//!     BuildContext, ConnectorRegistry, KnowledgeSink, MemorySyncState, SourceConfig, SyncOptions, sync_source,
//! };
//! use graphrt_rag::{HashEmbedder, KnowledgeBase, MemoryStore};
//!
//! let kb = KnowledgeBase::new()
//!     .with_store_dataset("site", Arc::new(MemoryStore::new()), Arc::new(HashEmbedder::new(256)), None);
//! let source = SourceConfig::new("home", "web", serde_json::json!({ "urls": ["https://graphrt.com/"] }))
//!     .dataset("site");
//! let connector = ConnectorRegistry::with_builtins().build(&source, &BuildContext::new(&|_| None))?;
//! let status = sync_source(
//!     &source,
//!     connector.as_ref(),
//!     &KnowledgeSink::new(kb),
//!     &MemorySyncState::new(),
//!     &SyncOptions::default(),
//! )
//! .await?;
//! println!("{} pages indexed", status.report.indexed);
//! # Ok(()) }
//! ```

mod auth;
mod config;
pub mod connectors;
mod error;
mod http;
pub mod jsonpath;
mod schedule;
mod scheduler;
#[cfg(feature = "rag")]
mod sink;
mod state;
mod sync;
mod types;

pub use auth::{Auth, AuthConfig, OAuthTokens, OnRotate, RotatedRefreshToken, Secret, SecretLookup};
pub use config::SourceConfig;
pub use connectors::{BuildContext, ConnectorFactory, ConnectorRegistry, INDEXABLE_EXTENSIONS, is_indexable};
pub use error::{ConnectError, Result};
pub use http::{Limits, SourceHttp, egress_policy};
pub use schedule::{MIN_INTERVAL_SECS, SyncSchedule};
pub use scheduler::{Scheduler, SourceView, TICK};
#[cfg(feature = "rag")]
pub use sink::{KnowledgeSink, METADATA_ITEM, METADATA_SOURCE, METADATA_URL};
#[cfg(feature = "sqlite")]
pub use state::SqliteSyncState;
pub use state::{ItemState, MemorySyncState, SourceStatus, StatusError, SyncOutcome, SyncReport, SyncState};
pub use sync::{DocumentSink, SyncDocument, SyncOptions, content_hash, document_id, sync_source, unix_now};
pub use types::{Connector, ItemContent, Listing, RemoteItem};
