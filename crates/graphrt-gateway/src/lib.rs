//! # graphrt-gateway
//!
//! The always-on front of scale-to-zero graphrt agents (plan §2): the only
//! writer of the run store, holding every run's stream while agents
//! (`graphrt serve --trust-gateway`) stay stateless.

pub mod auth;
#[cfg(feature = "gw-cloud-run")]
pub mod cloud_run;
pub mod cron;
pub mod ha;
pub mod hex;
pub mod limiter;
pub mod pool;
pub mod providers;
pub mod quota;
pub mod retire;
pub mod signature;
pub mod store;
pub mod tz;
pub mod warm;

/// Installs rustls' ring crypto provider for the process (idempotent). The
/// gateway's HTTP clients (JWKS, callbacks, provider APIs) need it.
pub fn install_crypto_provider() {
    let _already_installed = rustls::crypto::ring::default_provider().install_default();
}
pub mod agent_client;
#[cfg(feature = "gw-local")]
pub mod app;
pub mod config;
pub mod deploy;
pub mod deployment;
#[cfg(feature = "gw-local")]
pub mod local;
pub mod metrics;
pub mod os;
pub mod outbox;
pub mod scheduler;
pub mod server;
pub mod sse;
pub mod stored_request;
pub mod stream_conn;
pub mod supervisor;
pub mod webhook;
