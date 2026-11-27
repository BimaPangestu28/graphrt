//! Long term memory for graphrt agents.
//!
//! [`MemoryStore`] keeps the notes an agent saves about a user or about its
//! own work, in files, isolated per tenant, agent and user. It implements
//! [`graphrt_core::memory::MemoryBank`], so a host registers it with
//! [`MemoryStore::into_service`] and every `agent` node with
//! `long_term_memory` can use it.
//!
//! Every write passes the gate in [`gate`]: secrets are masked, text that
//! reads like an instruction waits for review, and the trust of the writer
//! decides whether a note is used at once. Every change is in an audit log
//! that holds no note text. Recall is BM25 by keyword, with cosine similarity
//! fused in when the store has an embedder.
//!
//! The design, the limits and the threat model are in `docs/memory-design.md`.

pub mod gate;
#[cfg(feature = "graph")]
mod graph;
pub mod search;
mod store;

pub use graphrt_rag::Embedder;
pub use store::{Limits, ListFilter, MemoryStore, check_name, check_user};
