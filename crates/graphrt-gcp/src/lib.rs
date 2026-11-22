//! Google Cloud for the graphrt gateway, without a Google SDK.
//!
//! - **`auth-gcp`** ([`auth`]): access tokens and ID tokens from the GCE/Cloud Run/GKE
//!   metadata server, or from a service-account key signed as an RS256 JWT with
//!   `ring` (already linked for TLS) and exchanged at the OAuth 2.0 token
//!   endpoint. Tokens are cached until `exp - 5 min`, refreshed once on a 401,
//!   and never logged.
//! - **`provider-cloud-run`** ([`cloud_run`], [`secret_manager`]): the Cloud Run
//!   [`Provider`](graphrt_provider_api::Provider) (`Addressing::Url`, scaled by the
//!   platform, min instances 0) and its
//!   [`DeployDriver`](graphrt_provider_api::DeployDriver) over the Cloud Run Admin
//!   API v2, with secrets injected as Secret Manager references. The deploy
//!   half (driver, Secret Manager) is the separate feature **`deploy-cloud-run`**,
//!   so the gateway build links only the run-time provider.
//!
//! Every endpoint (metadata server, token endpoint, Admin API, Secret Manager)
//! comes from the gateway operator's configuration, never from a deployment
//! spec, so a tenant cannot point the gateway's credentials at another host.

#[cfg(feature = "auth-gcp")]
pub mod auth;
#[cfg(feature = "provider-cloud-run")]
pub mod cloud_run;
#[cfg(feature = "auth-gcp")]
mod http_client;
#[cfg(feature = "provider-cloud-run")]
mod rest;
#[cfg(feature = "deploy-cloud-run")]
pub mod secret_manager;
#[cfg(feature = "auth-gcp")]
mod url_encoding;
