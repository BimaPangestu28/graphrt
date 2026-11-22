//! Azure provider family for the graphrt gateway (plan §5/§6).
//!
//! - [`auth`] (feature `auth-azure`): Entra ID tokens from managed identity,
//!   client credentials (secret or certificate) and workload identity
//!   federation, cached per scope.
//! - [`aca`] (feature `provider-aca`): the Azure Container Apps
//!   [`Provider`](api::Provider) (`Addressing::Url`, platform scale-to-zero),
//!   its [`DeployDriver`](api::DeployDriver) over ARM REST, revision labels for
//!   old deployment versions, and Key Vault secret references.
//!
//! No Azure SDK: a few REST calls over the `reqwest`/`rustls`/`ring` the
//! workspace already links. Endpoints (authority, ARM, Key Vault suffix) come
//! from the gateway operator's configuration, never from a deployment spec.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing))]

pub use graphrt_provider_api as api;

#[cfg(feature = "auth-azure")]
pub mod auth;
#[cfg(feature = "auth-azure")]
pub mod http;

#[cfg(feature = "provider-aca")]
pub mod aca;
#[cfg(feature = "provider-aca")]
pub mod arm;
#[cfg(feature = "deploy-aca")]
pub mod keyvault;
