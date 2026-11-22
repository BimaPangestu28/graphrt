//! `graphrt serve`: an HTTP API with SSE streaming.
//!
//! | Method | Path | Body | Response |
//! | --- | --- | --- | --- |
//! | GET | `/healthz` | – | `ok` (never requires auth) |
//! | GET | `/v1/workflows` | – | workflow names, inputs and env requirements |
//! | POST | `/v1/workflows/{name}/runs` | `{inputs, sys, conversation, stream, async}` | SSE stream, JSON result, or `202` + run id when `async` |
//! | GET | `/v1/runs` | query `workflow`, `status`, `limit`, `cursor` | stored runs, newest first |
//! | GET | `/v1/runs/{run_id}` | – | run record (status, outputs, pause reasons and expected resume inputs) |
//! | GET | `/v1/runs/{run_id}/events` | query `from`, `stream=false` | SSE replay (+ live tail while running), or a JSON array |
//! | POST | `/v1/runs/{run_id}/abort` | – | `202` if the run was active |
//! | POST | `/v1/runs/{run_id}/resume` | `{inputs, secrets, stream, async}` (`snapshot` optional) | SSE stream or JSON result |
//! | DELETE | `/v1/runs/{run_id}` | – | `204`; `409` while the run executes |
//! | GET | `/v1/nodes` | – | node catalog (descriptors with config JSON Schemas) |
//! | POST | `/v1/validate` | workflow document, or `{document: "<yaml/json text>"}` | `{valid, errors, warnings}` |
//! | GET | `/v1/runs/{run_id}/scores` | – | evaluation scores (feature `scores`, see [`crate::scores`]) |
//! | POST | `/v1/runs/{run_id}/feedback` | `{name, value, comment?, user?}` | `201` + the stored `human` score |
//! | GET | `/v1/hooks` | – | webhooks configured with `--webhooks` |
//! | POST | `/v1/hooks/{hook}` | any JSON (signature-checked when configured) | `202` + run id; see [`crate::webhooks`] |
//!
//! SSE events use the engine event `type` as the event name and its JSON as
//! data, plus `response` events carrying the ordered answer text
//! (`{node_id, text, is_final}`).
//!
//! Runs are recorded in a run store (`--store <dir>` for a durable directory,
//! in-memory otherwise), so a paused run can be resumed by id. See `docs/serving.md`.
//!
//! Workflows are compiled once at startup. Environment variables declared by a
//! workflow are read from the server's environment (or `--env-file`); requests
//! cannot set them.
//!
//! Modules, one concern each:
//!
//! - [`state`]: [`AppState`] and its builders; [`build`]: loading the workflows.
//! - [`router`]: routes and layers; [`auth`] and [`local_host`]: the request checks.
//! - [`start`], [`resume`], [`runs`], [`events`], [`workflows`]: the handlers.
//! - [`execute`]: launching runs; [`delivery`] and [`sse`]: answering their callers.
//! - [`startup`]: the `graphrt serve` command.

mod auth;
mod build;
mod delivery;
mod errors;
mod events;
mod execute;
mod local_host;
mod resume;
mod router;
mod runs;
mod sse;
mod start;
mod startup;
mod state;
mod workflows;

pub use auth::AuthenticatedUser;
pub(crate) use auth::constant_time_eq;
pub use build::{build_state, discover};
pub(crate) use errors::error_response;
#[cfg(feature = "scores")]
pub(crate) use errors::store_error;
pub use events::EventsQuery;
pub(crate) use execute::{Launch, launch_detached};
pub use resume::ResumeRequest;
pub use router::{DEFAULT_MAX_UPLOAD_BYTES, router, router_with_uploads};
pub use runs::ListQuery;
pub use start::RunRequest;
pub use startup::serve;
pub use state::{AppState, DEFAULT_MAX_ACTIVE_RUNS, ServedWorkflow};
pub use workflows::validation_report;
