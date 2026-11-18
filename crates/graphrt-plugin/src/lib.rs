//! # graphrt-plugin
//!
//! Plug-and-play extensions for graphrt. A plugin is a directory with a
//! `graphrt-plugin.json` (or `.yaml`) manifest that adds node kinds and tools,
//! implemented by:
//!
//! - **process**: an executable in any language speaking newline-delimited
//!   JSON-RPC 2.0 over stdio (started lazily, restarted after crashes);
//! - **http**: a remote service receiving JSON-RPC over `POST`;
//! - **wasm** (or any custom runtime): a [`PluginLoader`] registered on the host.
//!
//! ```no_run
//! # async fn demo() -> Result<(), graphrt_plugin::PluginError> {
//! use graphrt_core::{Registry, tool::ToolRegistry};
//! use graphrt_plugin::{PluginHost, default_plugin_paths};
//!
//! let mut host = PluginHost::new();
//! host.load_dirs(&default_plugin_paths())?;
//! let mut registry = Registry::with_builtins();
//! let mut tools = ToolRegistry::new();
//! host.register(&mut registry, &mut tools)?;
//! # Ok(()) }
//! ```
//!
//! The protocol and manifest format are specified in `docs/plugins.md`.

mod conn;
mod discover;
mod error;
mod host;
#[cfg(feature = "http")]
mod http;
mod manifest;
mod node;
#[cfg(feature = "process")]
mod process;
mod protocol;
mod render;
mod transport;

pub use conn::{Connection, NotifySink};
pub use discover::{
    PLUGIN_PATH_ENV, default_install_dir, default_plugin_paths, discover, install_from_dir, sha256_file,
    verify_checksums,
};
#[cfg(feature = "git")]
pub use discover::{install_from_git, install_from_git_subdir};
pub use error::{CallError, PluginError};
pub use host::{LoadedPlugin, PluginHost, PluginInfo, PluginLoader};
#[cfg(feature = "http")]
pub use http::HttpTransport;
pub use manifest::{
    HttpRuntime, Limits, LoadedManifest, MANIFEST_FILES, PROTOCOL_VERSION, Permissions, PluginManifest, ProcessRuntime,
    Runtime, WasmRuntime, is_valid_name, manifest_in,
};
#[cfg(feature = "process")]
pub use process::{DEFAULT_PASS_ENV, MAX_CONSECUTIVE_TIMEOUTS, ProcessTransport};
pub use protocol::Notification;
pub use render::{collect_selectors, render_config};
pub use transport::{HOST_FEATURES, IoTransport, Transport};
