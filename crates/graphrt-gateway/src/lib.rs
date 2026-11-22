//! # graphrt-gateway
//!
//! The always-on front of scale-to-zero graphrt agents (plan §2): the only
//! writer of the run store, holding every run's stream while agents
//! (`graphrt serve --trust-gateway`) stay stateless.

pub mod auth;
pub mod cron;
pub mod hex;
pub mod limiter;
pub mod quota;
pub mod signature;
pub mod store;
pub mod tz;
pub mod pool;

/// Installs rustls' ring crypto provider for the process (idempotent). The
/// gateway's HTTP clients (JWKS, callbacks, provider APIs) need it.
pub fn install_crypto_provider() {
    let _already_installed = rustls::crypto::ring::default_provider().install_default();
}
