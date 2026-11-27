//! `graphrt serve`: an HTTP API with SSE streaming.
//!
//! | Method | Path | Body | Response |
//! | --- | --- | --- | --- |
//! | GET | `/healthz` | – | `ok` (never requires auth) |
//! | GET | `/readyz` | – | `ready`, or `503 draining` during a shutdown drain (never requires auth) |
//! | GET | `/v1/workflows` | – | workflow names, inputs and env requirements |
//! | POST | `/v1/workflows/{name}/runs` | `{inputs, sys, conversation, stream, async}` | SSE stream, JSON result, or `202` + run id when `async` |
//! | GET | `/v1/runs` | query `workflow`, `status`, `limit`, `cursor` | stored runs, newest first |
//! | GET | `/v1/runs/{run_id}` | – | run record (status, outputs, pause reasons and expected resume inputs) |
//! | GET | `/v1/runs/{run_id}/events` | query `from`, `stream=false`, `responses=true` | SSE replay (+ live tail while running), or a JSON array |
//! | POST | `/v1/runs/{run_id}/abort` | – | `202` if the run was active |
//! | POST | `/v1/runs/{run_id}/resume` | `{inputs, secrets, stream, async}` (`snapshot` optional) | SSE stream or JSON result |
//! | DELETE | `/v1/runs/{run_id}` | – | `204`; `409` while the run executes |
//! | GET | `/v1/nodes` | – | node catalog (descriptors with config JSON Schemas) |
//! | POST | `/v1/validate` | workflow document, or `{document: "<yaml/json text>"}` | `{valid, errors, warnings}` |
//! | GET | `/v1/runs/{run_id}/scores` | – | evaluation scores (feature `scores`, see [`crate::scores`]) |
//! | POST | `/v1/runs/{run_id}/feedback` | `{name, value, comment?, user?}` | `201` + the stored `human` score |
//! | GET, POST | `/v1/conversations` | `{workflow, input?, title?}` | conversations kept by the server (`--conversations`, see [`crate::conversations`]) |
//! | GET, DELETE | `/v1/conversations/{id}` | – | one conversation with its messages |
//! | GET, POST | `/v1/devices` | `{name, scopes?, expires_in_days?, url?}` | paired devices and their tokens (`--devices`, see [`crate::devices`]) |
//! | GET | `/v1/approvals` | query `workflow`, `limit`, `cursor` | runs that wait for a person (`--approvals`, see [`crate::approvals`]) |
//! | POST | `/v1/approvals/{run_id}` | `{inputs}` | `202`: the run continues with the answer |
//! | GET, POST, DELETE | `/v1/push`, `/v1/push/subscriptions` | `{endpoint, keys}` | Web Push subscriptions (`--push`, see [`crate::notify`]) |
//! | GET | `/monitor/` | – | the monitoring page (`--monitor`), with `/v1/monitor/config` |
//! | GET | `/chat/`, `/chat/{workflow}` | – | the chat page (`--chat`), with `/chat/embed.js`, `/v1/chat/config` and `/v1/chat/guest` |
//! | GET, POST | `/v1/channels/{name}/webhook` | a Telegram update or a WhatsApp delivery | messaging channels (`--channel`, see [`crate::channels`]) |
//! | GET | `/console/`, `/v1/console/...` | – | the admin console (`--console`, see `docs/console.md`): sign in, registry, secrets, roles, audit log |
//! | GET | `/v1/hooks` | – | webhooks configured with `--webhooks` |
//! | POST | `/v1/hooks/{hook}` | any JSON (signature-checked when configured) | `202` + run id; see [`crate::webhooks`] |
//!
//! SSE events use the engine event `type` as the event name and its JSON as
//! data, plus `response` events carrying the ordered answer text
//! (`{node_id, text, is_final}`). Each carries its index in the run's event
//! log as `id:` ([`crate::sse_ids`]), so `/events` resumes after a client's
//! `Last-Event-ID`.
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
//! - [`startup`]: the `graphrt serve` command, its shutdown drain included.

#[cfg(any(feature = "web-chat", feature = "web-monitor"))]
mod assets;
mod auth;
mod build;
#[cfg(feature = "web-chat")]
mod chat;
#[cfg(feature = "console")]
pub(crate) mod console;
#[cfg(feature = "cors")]
mod cors;
mod delivery;
mod env_flags;
mod errors;
mod events;
mod execute;
#[cfg(feature = "files")]
mod files_api;
#[cfg(feature = "web-chat")]
mod guests;
mod local_host;
#[cfg(feature = "web-monitor")]
mod monitor;
mod resume;
mod router;
mod runs;
mod sse;
mod start;
mod startup;
mod state;
mod workflows;

pub use auth::AuthenticatedUser;
#[cfg(feature = "scores")]
pub(crate) use auth::Caller;
#[cfg(feature = "devices")]
pub(crate) use auth::Operator;
pub(crate) use auth::constant_time_eq;
pub use build::{build_state, discover};
#[cfg(feature = "web-chat")]
pub use chat::{Chat, ChatContext, ChatWorkflow};
#[cfg(feature = "cors")]
pub use cors::Cors;
#[cfg(feature = "trust-gateway")]
pub(crate) use delivery::{EventScope, StreamGuard};
pub(crate) use errors::error_response;
#[cfg(any(feature = "scores", feature = "trust-gateway", feature = "approvals"))]
pub(crate) use errors::store_error;
pub use events::EventsQuery;
#[cfg(feature = "trust-gateway")]
pub(crate) use execute::to_values;
#[cfg(feature = "channels")]
pub(crate) use execute::{ChannelTurn, TurnRefused, start_channel_turn};
pub(crate) use execute::{Launch, launch_detached, traceparent};
#[cfg(feature = "web-chat")]
pub use guests::{GuestTokenError, Guests, IssuedToken};
#[cfg(feature = "web-monitor")]
pub use monitor::Monitor;
pub use resume::{ResumeRequest, resume_drained_runs};
#[cfg(feature = "approvals")]
pub(crate) use resume::{resume_detached, stop_paused};
pub use router::{DEFAULT_MAX_UPLOAD_BYTES, router, router_with_uploads};
pub use runs::ListQuery;
#[cfg(feature = "trust-gateway")]
pub(crate) use sse::sse_stream;
pub use start::RunRequest;
pub use startup::serve;
#[cfg(any(feature = "skills", feature = "learning"))]
pub(crate) use state::MODEL_ANSWER_TIMEOUT;
pub use state::{AppState, DEFAULT_MAX_ACTIVE_RUNS, ServedWorkflow};
pub use workflows::validation_report;
