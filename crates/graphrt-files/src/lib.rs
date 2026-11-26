//! # graphrt-files
//!
//! File workspaces for graphrt agents.
//!
//! An agent gets a [`Workspace`]: a place to keep files that it can list,
//! read, search and write through the [`FileTools`] tool provider, and that it
//! can never leave. The pieces:
//!
//! | Piece | What it does |
//! | --- | --- |
//! | [`WorkspacePath`] | A checked relative path. No `..`, no absolute paths, no Windows tricks. |
//! | [`FileStore`] | The storage backend: [`DirStore`] (a directory), [`MemoryStore`], or your own. |
//! | [`Workspace`] | Limits, quota, per-user scopes and type checks on top of any store. |
//! | [`sniff`] | The media type of a file from its bytes, never from its name. |
//! | [`FileTools`] | The `files` tools for `agent` and `tool` nodes. |
//! | [`WorkspaceResolver`] | Lets other nodes (document extractor, vision) read workspace files. |
//!
//! Nothing here is on unless a host registers it: a runtime without a
//! workspace has no file tools and pays nothing for this crate.
//!
//! ```
//! # let runtime = tokio::runtime::Builder::new_current_thread().build()?;
//! # runtime.block_on(async {
//! use std::sync::Arc;
//! use graphrt_core::tool::{ToolCallContext, ToolRegistry, ToolRuntime};
//! use graphrt_files::{FileTools, Workspace, WorkspaceLimits};
//!
//! let workspace = Workspace::in_memory(WorkspaceLimits::default());
//! let tools = FileTools::new(workspace);
//! let written = tools
//!     .call("write", serde_json::json!({"path": "notes/todo.md", "content": "- ship it\n"}), ToolCallContext::empty())
//!     .await?;
//! assert!(!written.is_error);
//!
//! // Paths that leave the workspace are refused before any storage is touched.
//! let escape = tools.call("read", serde_json::json!({"path": "../../etc/passwd"}), ToolCallContext::empty()).await?;
//! assert!(escape.is_error);
//!
//! let mut registry = ToolRegistry::new();
//! registry.add("files", Arc::new(tools)); // agent nodes reference `files/*`
//! # Ok::<(), graphrt_core::tool::ToolError>(())
//! # })?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod dir;
mod memory;
mod path;
mod resolver;
mod sniff;
mod store;
mod text;
mod tools;
mod workspace;

pub use dir::DirStore;
pub use memory::MemoryStore;
pub use path::{MAX_COMPONENT_BYTES, MAX_PATH_BYTES, MAX_PATH_DEPTH, PathError, WorkspacePath};
pub use resolver::WorkspaceResolver;
pub use sniff::{MediaClass, MediaType, SNIFF_BYTES, sniff};
pub use store::{Entry, FileStore, Listing, StoreError, hex};
pub use tools::{FileTools, FileToolsConfig, PROVIDER, SCOPE_CONTEXT_KEY};
pub use workspace::{FilesError, REFERENCE_SCHEME, StoredFile, Usage, Workspace, WorkspaceLimits};

/// Run blocking or CPU-heavy work off the async workers: on Tokio's blocking
/// pool when the `tokio` feature is on and a runtime is present, inline
/// otherwise. `Err` carries the message of a task that panicked or was cancelled.
pub async fn blocking<T, F>(work: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    #[cfg(feature = "tokio")]
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        return handle.spawn_blocking(work).await.map_err(|e| format!("background task failed: {e}"));
    }
    Ok(work())
}
