//! Cloud Run: the gateway's run-time [`CloudRunProvider`] and the deploy-time
//! `CloudRunDeployDriver` (feature `deploy-cloud-run`, Cloud Run Admin API v2). See `docs/gateway-cloud-run.md`.

mod admin;
mod config;
#[cfg(feature = "deploy-cloud-run")]
mod deploy;
pub mod naming;
mod provider;
#[cfg(feature = "deploy-cloud-run")]
pub mod service_spec;
mod status;

#[cfg(feature = "direct-cloud-run")]
pub(crate) use config::is_allowed_endpoint;
pub use config::{
    CloudRunConfig, DEFAULT_ADMIN_ENDPOINT, DEFAULT_SECRET_MANAGER_ENDPOINT, InvokeAuth, MAX_REQUEST_TIMEOUT_SECS,
};
#[cfg(feature = "deploy-cloud-run")]
pub use deploy::CloudRunDeployDriver;
pub use provider::{CloudRunProvider, INVOKE_AUTH_HEADER, SHUTDOWN_GRACE_SECS, URL_REPLICA_ID, WAKE_P95_TARGET_MS};
pub use status::CloudRunServiceStatus;

/// Seconds before Cloud Run's request cap at which the gateway preempts a run
/// and re-dispatches it (plan §5: preempt at `cap - 60 s`).
pub const PLANNED_PREEMPT_MARGIN_SECS: u64 = 60;
