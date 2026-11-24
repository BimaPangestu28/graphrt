//! Shared test support: the fake Machines API (from the crate's `fake` feature)
//! and the rustls crypto provider reqwest needs.

#![allow(dead_code, unused_imports)]

pub use graphrt_provider_fly::fake::*;

/// Installs the rustls crypto provider reqwest needs (the binary does it in `main`).
pub fn install_crypto_provider() {
    let _already_installed = rustls::crypto::ring::default_provider().install_default();
}
