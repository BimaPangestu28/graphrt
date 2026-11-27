//! # graphrt-collab
//!
//! Real time collaboration on graphrt agent projects.
//!
//! - [`doc`]: the shared document of one project, a Yjs compatible CRDT that
//!   merges concurrent and offline edits of the graph, the prompts and the
//!   layout without conflicts.
//! - [`protocol`]: the messages of a sync session over a WebSocket (the Yjs
//!   sync protocol, plus Graphrt control messages).
//! - `server` (cargo feature `server`): the collaboration server, hosted
//!   (many teams) or self hosted (one company) from the same binary.
//! - `client` (cargo feature `client`): sign in, workspaces and live sync for apps.
//!
//! Nothing here is linked into the runtime unless a build asks for it: the
//! crate is only a dependency of the CLI's `collab` feature and of apps.

#[cfg(feature = "client")]
pub mod client;
pub mod doc;
mod error;
pub mod protocol;
#[cfg(feature = "server")]
pub mod server;

pub use doc::ProjectDoc;
pub use error::{CollabError, Result};
