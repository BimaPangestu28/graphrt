//! Kubernetes provider of the graphrt gateway.
//!
//! - [`K8sProvider`]: run-time `wake` / `release` / `list` over the
//!   `deployments/{name}/scale` subresource, `pod-deletion-cost` victims and
//!   EndpointSlice readiness watches.
//! - [`K8sDeployDriver`]: deploy-time server-side apply of a version's
//!   Deployment and Service, with Secret references only.
//! - [`auth`]: in-cluster service account, kubeconfig (token, token file,
//!   client certificate, exec plugin) and the [`auth::TokenSource`] hook for
//!   EKS / GKE / AKS cluster tokens.
//!
//! Hand-rolled REST on the reqwest + rustls stack graphrt already links: no
//! `kube`, no `k8s-openapi`, no `h2` (plan §9).
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use graphrt_provider_k8s::{ClientOptions, DeploymentRef, K8sProvider, K8sProviderConfig, KubeClient, Provider};
//!
//! let cluster = graphrt_provider_k8s::auth::infer_cluster_config()?;
//! let namespace = cluster.namespace.clone().unwrap_or_else(|| "default".into());
//! let provider = K8sProvider::new(KubeClient::new(&cluster, ClientOptions::default())?, K8sProviderConfig::new(namespace));
//! let endpoints = provider.wake(&DeploymentRef::new("support-bot", "v3"), 1).await?;
//! # drop(endpoints);
//! # Ok(())
//! # }
//! ```

pub mod api;
pub mod auth;
mod client;
pub mod deploy;
#[cfg(feature = "fake-api-server")]
pub mod fake;
pub mod names;
mod provider;
pub mod readiness;
pub mod resources;

pub use api::{
    Addressing, Capabilities, DeployDriver, DeployError, DeploymentRef, DeploymentSpec, Endpoint, Provider,
    ProviderError, ReleaseMode, ReplicaPhase, ReplicaRef, ReplicaState, Scaler, SecretRef,
};
pub use client::{ClientOptions, KubeClient, KubeError, RequestBody, WatchEvent, WatchStream};
pub use deploy::K8sDeployDriver;
pub use provider::{GATEWAY_FIELD_MANAGER, K8sProvider, K8sProviderConfig};
