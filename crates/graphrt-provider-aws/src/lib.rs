//! AWS providers for the graphrt gateway, without the AWS SDK.
//!
//! | Feature | What it adds |
//! | --- | --- |
//! | `auth-aws` | SigV4, the credential chain (env → container endpoint → STS web identity → IMDSv2), presigned EKS tokens, the signed REST client |
//! | `provider-ecs` | [`ecs::EcsProvider`]: ECS Fargate, one task per replica, gateway-scheduled (`RunTask` / `StopTask` / `DescribeTasks` / `ListTasks`) |
//! | `provider-lambda` | [`lambda::LambdaProvider`]: short synchronous runs through `InvokeWithResponseStream` + Lambda Web Adapter |
//! | `deploy-ecs` / `deploy-lambda` (`deploy`) | `deploy::ecs::EcsDeployDriver` / `deploy::lambda::LambdaDeployDriver`: the `DeployDriver`s of `graphrt deploy`, not linked by the gateway |
//!
//! App Runner is not supported: it is closed to new customers since 30 April 2026.
//!
//! Both providers implement [`graphrt_provider_api::Provider`]; see
//! `docs/gateway-aws.md` for setup, IAM policies and limits.

pub use graphrt_provider_api as api;
#[cfg(feature = "auth-aws")]
pub mod auth;
#[cfg(any(feature = "deploy-ecs", feature = "deploy-lambda"))]
pub mod deploy;
#[cfg(feature = "provider-ecs")]
pub mod ecs;
#[cfg(feature = "provider-lambda")]
pub mod lambda;
#[cfg(feature = "auth-aws")]
pub mod rest;
