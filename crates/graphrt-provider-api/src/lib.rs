//! # graphrt-provider-api
//!
//! The contract between the graphrt gateway and the platforms it wakes agents
//! on (plan §6). Every provider (local-process, Kubernetes, Cloud Run, Azure
//! Container Apps, ECS Fargate, Lambda, Fly) lives in its own crate or module
//! behind its own cargo feature and implements:
//!
//! - [`Provider`]: run-time `wake` / `release` / `list`, with [`Capabilities`]
//!   that tell the gateway how to drain and when to preempt;
//! - [`DeployDriver`]: deploy-time `apply` of a [`DeploymentSpec`].
//!
//! The crate also carries the one HTTP layer providers share ([`http`]: a
//! token bucket per provider API, jittered retry of 429/5xx, token caching with
//! one refresh on 401; the reqwest client is behind feature `http`) and, behind
//! feature `conformance`, the C1–C13 suite with a scripted fake HTTP server
//! and an in-memory reference provider ([`conformance`]).
//!
//! No cloud SDK is used anywhere: REST and auth are hand-rolled per provider.
//!
//! ```
//! use graphrt_provider_api::{Addressing, ProviderError, ProviderErrorKind};
//!
//! assert_eq!(ProviderError::kind_for_status(429), ProviderErrorKind::RateLimited);
//! assert!(ProviderErrorKind::Transient.is_retryable());
//! assert_ne!(Addressing::Replica, Addressing::Url);
//! ```

#[cfg(feature = "conformance")]
pub mod conformance;
mod error;
pub mod http;
mod provider;
mod types;

pub use error::{DeployError, DeployErrorKind, ProviderError, ProviderErrorKind};
pub use provider::{BoxFuture, DeployDriver, Provider};
pub use types::{
    Addressing, Capabilities, DeploymentRef, DeploymentSpec, Endpoint, EndpointAddress, ReleaseMode, ReplicaPhase,
    ReplicaRef, ReplicaState, Resources, Scaler, SecretRef,
};
