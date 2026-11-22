//! # graphrt-provider-fly
//!
//! Fly Machines provider of the graphrt gateway (plan §5/§6), behind the
//! `provider-fly` feature of the binary.
//!
//! - [`FlyProvider`] (run-time, gateway credentials: a per-app deploy token):
//!   a pool of pre-created Machines per deployment version, woken with
//!   `start` (suspended snapshots first), released with `suspend` falling back
//!   to `stop`, addressed over 6PN (`http://[fdaa:…]:8080`).
//! - [`FlyDeployDriver`] (deploy-time, `graphrt deploy --target fly`): creates
//!   the app, writes Fly app secrets, and creates the stopped pool with
//!   `stop_config` = SIGTERM + `kill_timeout` (≤ 300 s) for the agent's drain.
//!
//! No Fly SDK: a small REST client ([`MachinesClient`]) over the shared
//! provider HTTP layer (reqwest + rustls the binary already links).
//!
//! ```
//! use graphrt_provider_fly::{FlyProviderConfig, FlyToken, TokenOrigin};
//!
//! let token = FlyToken::parse("FlyV1 fm2_example").unwrap();
//! let config = FlyProviderConfig::single_app("chat-agents", TokenOrigin::Fixed(token));
//! assert!(config.validate().is_ok());
//! ```

mod client;
mod config;
#[cfg(feature = "deploy")]
mod deploy;
mod error;
pub mod model;
mod provider;
mod token;

pub use client::{ClientPolicy, INTERNAL_API_BASE_URL, MAX_WAIT_SECS, MachinesClient, PUBLIC_API_BASE_URL};
pub use config::{FlyProviderConfig, MAX_KILL_TIMEOUT_SECS, MAX_SUSPEND_MEMORY_MB};
#[cfg(feature = "deploy")]
pub use deploy::{EnvLookup, FlyDeployConfig, FlyDeployDriver, METADATA_FINGERPRINT, suspend_supported};
pub use error::classify_fly_response;
pub use provider::FlyProvider;
pub use token::{FLY_API_TOKEN_ENV, FlyToken, TokenOrigin};
