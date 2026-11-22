//! Operator configuration of the Fly provider (gateway side).
//!
//! Endpoints and tokens come from here, never from a deployment spec (plan §6,
//! SSRF): a spec only names the app (`DeploymentRef::target`), and only apps
//! listed in [`FlyProviderConfig::apps`] can be touched.

use std::collections::BTreeMap;
use std::time::Duration;

use graphrt_provider_api::ReleaseMode;

use crate::client::{ClientPolicy, PUBLIC_API_BASE_URL};
use crate::token::TokenOrigin;

/// Longest `kill_timeout` / `stop_config.timeout` Fly honours.
pub const MAX_KILL_TIMEOUT_SECS: u64 = 300;
/// Largest guest memory Fly can suspend (snapshot) today.
pub const MAX_SUSPEND_MEMORY_MB: u32 = 2048;

/// Settings of the run-time provider.
#[derive(Clone, Debug)]
pub struct FlyProviderConfig {
    /// [`PUBLIC_API_BASE_URL`], or `http://_api.internal:4280` when the gateway runs on Fly.
    pub api_base_url: String,
    /// Apps the gateway may wake, each with its own deploy token (least privilege).
    pub apps: BTreeMap<String, TokenOrigin>,
    /// Port `graphrt serve` listens on inside each Machine (`--addr [::]:<port>`).
    pub agent_port: u16,
    /// What `ReleaseMode::Suspend` / `ScaleIn` do: `Suspend` (default) or `Stop`.
    pub preferred_release: ReleaseMode,
    /// Grace of a `stop` (the Machine's `kill_timeout`), capped at 300 s.
    pub stop_timeout: Duration,
    /// Longest wait for a started Machine (several `/wait` calls when > 60 s).
    pub start_timeout: Duration,
    /// Upper bound of Machines per deployment version; beyond the pool created
    /// by `graphrt deploy`, `wake` clones a pool member's config. 0 = never create.
    pub max_machines_per_version: u32,
    /// Retries and rate limit of the Machines API client.
    pub client_policy: ClientPolicy,
}

impl FlyProviderConfig {
    /// A config for one app with the defaults below.
    ///
    /// @param app_name - Fly app of the deployment (String)
    /// @param token_origin - where its deploy token is read from (TokenOrigin)
    /// @returns Config with suspend preferred, 300 s kill timeout, port 8080, up to 10 Machines
    pub fn single_app(app_name: impl Into<String>, token_origin: TokenOrigin) -> Self {
        Self {
            api_base_url: PUBLIC_API_BASE_URL.to_owned(),
            apps: BTreeMap::from([(app_name.into(), token_origin)]),
            agent_port: 8080,
            preferred_release: ReleaseMode::Suspend,
            stop_timeout: Duration::from_secs(MAX_KILL_TIMEOUT_SECS),
            start_timeout: Duration::from_secs(60),
            max_machines_per_version: 10,
            client_policy: ClientPolicy::default(),
        }
    }

    /// Checks the config before any call is made.
    ///
    /// @returns A message naming the first problem
    pub fn validate(&self) -> Result<(), String> {
        if self.apps.is_empty() {
            return Err("Fly provider: no app configured".to_owned());
        }
        if self.agent_port == 0 {
            return Err("Fly provider: agent_port must not be 0".to_owned());
        }
        if self.stop_timeout.as_secs() > MAX_KILL_TIMEOUT_SECS {
            return Err(format!("Fly provider: stop_timeout is capped at {MAX_KILL_TIMEOUT_SECS} s by Fly"));
        }
        if self.preferred_release == ReleaseMode::ScaleIn {
            return Err("Fly provider: preferred_release must be Suspend or Stop".to_owned());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::FlyToken;

    fn origin() -> TokenOrigin {
        TokenOrigin::Fixed(FlyToken::parse("fm2_x").unwrap())
    }

    #[test]
    fn defaults_are_valid() {
        assert!(FlyProviderConfig::single_app("chat", origin()).validate().is_ok());
    }

    #[test]
    fn rejects_a_kill_timeout_fly_would_not_honour() {
        let mut config = FlyProviderConfig::single_app("chat", origin());
        config.stop_timeout = Duration::from_secs(301);
        assert!(config.validate().is_err());
    }
}
