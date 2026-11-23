//! `gateway.yaml`: listener, store, limits and deployments. Secrets are never
//! in the file, only names of environment variables (`*_env`) and SHA-256
//! digests of API keys.
//!
//! ```yaml
//! listen: 127.0.0.1:8700
//! store: /var/lib/graphrt/gateway.db
//! backup: { dir: /backup/graphrt, interval_secs: 300, keep: 12 }
//! admin_token_env: GRAPHRT_GATEWAY_ADMIN_TOKEN
//! per_ip: { per_second: 20, burst: 40 }
//! socket_dir: /run/graphrt
//! ha: { node_id: gw-a, takeover: auto }      # active/passive, see docs/gateway-ha.md
//! deployments:
//!   - name: shop
//!     keys_env: SHOP_GATEWAY_KEYS          # kid:hex[,kid:hex], shared with the agents
//!     api_keys: [3f2a...]                  # sha256 hex of each key
//!     jwt: { issuer: https://auth.example, audience: shop, jwks_url: https://auth.example/.well-known/jwks.json }
//!     rate_limit: { per_second: 2, burst: 10 }
//!     input_headers: [{ header: authorization, input: user_jwt, strip_bearer: true }]
//!     pool: { max_active_runs: 8, min_replicas: 0, idle_timeout_secs: 300 }
//!     quotas: { max_replicas: 4, max_queued: 256, max_wakes_per_hour: 600 }
//!     callback_secret_env: SHOP_CALLBACK_SECRET
//!     hooks: [{ id: github, scheme: github, secret_env: GH_SECRET, idempotency_header: x-github-delivery }]
//!     crons: [{ id: nightly, schedule: "0 3 * * *", timezone: Europe/Berlin, catchup: latest, jitter_secs: 120, prewarm_secs: 30 }]
//!     warm: { predictive: true, lead_secs: 60, max_warm_replica_secs_per_day: 7200 }
//! wake_storm: { max_parallel_wakes: 16, prewarm_jitter_ms: 1000 }
//! ```

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use crate::auth::JwtConfig;
use crate::cron::Catchup;
use crate::deployment::InputHeader;
use crate::limiter::RateLimit;
use crate::pool::PoolConfig;
use crate::quota::Quotas;
use crate::webhook::HookConfig;

/// A rate limit in the file.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct RateLimitConfig {
    /// Sustained requests per second.
    pub per_second: f64,
    /// Burst.
    pub burst: u32,
}

impl From<RateLimitConfig> for RateLimit {
    fn from(config: RateLimitConfig) -> Self {
        Self { per_second: config.per_second, burst: config.burst }
    }
}

/// Backup settings.
#[derive(Debug, Clone, Deserialize)]
pub struct BackupConfig {
    /// Target directory.
    pub dir: PathBuf,
    /// Seconds between backups (default 300).
    #[serde(default = "default_backup_interval")]
    pub interval_secs: u64,
    /// Backups kept (default 12).
    #[serde(default = "default_backup_keep")]
    pub keep: usize,
}

fn default_backup_interval() -> u64 {
    300
}
fn default_backup_keep() -> usize {
    12
}

/// Pool settings in the file.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PoolFileConfig {
    /// Runs per replica.
    pub max_active_runs: u32,
    /// Warm replicas.
    pub min_replicas: u32,
    /// Idle seconds before release.
    pub idle_timeout_secs: u64,
    /// Seconds a request waits for a replica.
    pub queue_timeout_secs: u64,
    /// Seconds a wake may take.
    pub wake_timeout_secs: u64,
}

impl Default for PoolFileConfig {
    fn default() -> Self {
        let defaults = PoolConfig::default();
        Self {
            max_active_runs: defaults.max_active_runs,
            min_replicas: defaults.min_replicas,
            idle_timeout_secs: defaults.idle_timeout.as_secs(),
            queue_timeout_secs: defaults.queue_timeout.as_secs(),
            wake_timeout_secs: defaults.wake_timeout.as_secs(),
        }
    }
}

/// A cron schedule in the file.
#[derive(Debug, Clone, Deserialize)]
pub struct CronConfig {
    /// Schedule id.
    pub id: String,
    /// Five-field expression or `@daily`...
    pub schedule: String,
    /// IANA zone or `UTC` / `+07:00` (default UTC).
    #[serde(default = "default_zone")]
    pub timezone: String,
    /// Catch-up policy.
    #[serde(default)]
    pub catchup: Catchup,
    /// Fires replayed by `catchup: all`.
    #[serde(default = "default_max_catchup")]
    pub max_catchup: usize,
    /// Deterministic jitter bound.
    #[serde(default)]
    pub jitter_secs: u64,
    /// Wake this long before the fire.
    #[serde(default)]
    pub prewarm_secs: u64,
    /// Run inputs.
    #[serde(default)]
    pub inputs: serde_json::Map<String, serde_json::Value>,
}

fn default_zone() -> String {
    "UTC".to_owned()
}
fn default_max_catchup() -> usize {
    1
}

/// One deployment.
#[derive(Debug, Clone, Deserialize)]
pub struct DeploymentConfig {
    /// Name.
    pub name: String,
    /// Variable with the agent HMAC keys.
    pub keys_env: String,
    /// SHA-256 hex digests of API keys.
    #[serde(default)]
    pub api_keys: Vec<String>,
    /// End-user JWTs.
    #[serde(default)]
    pub jwt: Option<JwtConfig>,
    /// Per-principal limit.
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    /// Header inputs.
    #[serde(default)]
    pub input_headers: Vec<InputHeader>,
    /// Pool.
    #[serde(default)]
    pub pool: PoolFileConfig,
    /// Quotas.
    #[serde(default)]
    pub quotas: Quotas,
    /// Variable with the callback signing secret.
    #[serde(default)]
    pub callback_secret_env: Option<String>,
    /// Webhooks.
    #[serde(default)]
    pub hooks: Vec<HookConfig>,
    /// Cron schedules.
    #[serde(default)]
    pub crons: Vec<CronConfig>,
    /// Accept requests without credentials (only `--dev`).
    #[serde(default)]
    pub open: bool,
    /// Name of the entry under `providers:` (default `local`).
    #[serde(default)]
    pub provider: Option<String>,
    /// Predictive prewarm and keep-warm budget.
    #[serde(default)]
    pub warm: crate::warm::WarmConfig,
    /// Agent keys given programmatically (`--dev`) instead of `keys_env`.
    #[serde(skip)]
    pub inline_keys: Option<String>,
}

impl DeploymentConfig {
    /// The pool configuration.
    pub fn pool_config(&self) -> PoolConfig {
        PoolConfig {
            max_active_runs: self.pool.max_active_runs.max(1),
            min_replicas: self.pool.min_replicas,
            idle_timeout: Duration::from_secs(self.pool.idle_timeout_secs),
            queue_timeout: Duration::from_secs(self.pool.queue_timeout_secs.max(1)),
            wake_timeout: Duration::from_secs(self.pool.wake_timeout_secs.max(1)),
            quotas: self.quotas,
            warm_budget: self
                .warm
                .max_warm_replica_secs_per_day
                .map(|secs| std::sync::Arc::new(crate::warm::WarmBudget::new(secs))),
            wake_gate: None,
        }
    }
}

/// The whole file.
#[derive(Debug, Clone, Deserialize)]
pub struct GatewayConfig {
    /// `host:port`.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// SQLite file (`:memory:` for tests).
    #[serde(default = "default_store")]
    pub store: PathBuf,
    /// Backups.
    #[serde(default)]
    pub backup: Option<BackupConfig>,
    /// Variable with the admin token (`graphrt deploy`).
    #[serde(default)]
    pub admin_token_env: Option<String>,
    /// Per-IP limit.
    #[serde(default)]
    pub per_ip: Option<RateLimitConfig>,
    /// Directory of local-process sockets; replaced by a short private one
    /// under `/tmp` when socket paths in it would not fit `sun_path`.
    #[serde(default = "default_socket_dir")]
    pub socket_dir: PathBuf,
    /// Grace of local replicas (SIGTERM → SIGKILL).
    #[serde(default = "default_grace")]
    pub local_grace_secs: u64,
    /// Days finished runs (and webhook markers) are kept; at least 3 (Stripe retries).
    #[serde(default = "default_ttl_days")]
    pub store_ttl_days: u64,
    /// Seconds an agent may take to report an orphan after a lost stream.
    #[serde(default = "default_reclaim")]
    pub reclaim_grace_secs: u64,
    /// Active/passive on a shared volume (F3); absent: a single gateway.
    #[serde(default)]
    pub ha: Option<crate::ha::HaConfig>,
    /// Named providers (operator configuration; see [`crate::providers`]).
    #[serde(default)]
    pub providers: std::collections::BTreeMap<String, crate::providers::ProviderConfig>,
    /// Gateway-wide wake-storm protection.
    #[serde(default)]
    pub wake_storm: crate::warm::WakeStormConfig,
    /// Deployments.
    #[serde(default)]
    pub deployments: Vec<DeploymentConfig>,
    /// Admin token given programmatically (`--dev`) instead of `admin_token_env`.
    #[serde(skip)]
    pub inline_admin_token: Option<String>,
}

fn default_listen() -> String {
    "127.0.0.1:8700".to_owned()
}
fn default_store() -> PathBuf {
    PathBuf::from("gateway.db")
}
fn default_socket_dir() -> PathBuf {
    std::env::temp_dir().join("graphrt-gateway")
}
fn default_grace() -> u64 {
    30
}
fn default_ttl_days() -> u64 {
    7
}
fn default_reclaim() -> u64 {
    60
}

impl GatewayConfig {
    /// Parses and validates YAML.
    ///
    /// # Errors
    /// A message naming the problem.
    pub fn from_yaml(text: &str) -> Result<Self, String> {
        let config: Self =
            graphrt_core::yaml::from_str(text, false).map_err(|error| format!("gateway config: {error}"))?;
        config.validate()?;
        Ok(config)
    }

    /// Checks invariants serde cannot.
    ///
    /// # Errors
    /// A message naming the problem.
    pub fn validate(&self) -> Result<(), String> {
        if self.store_ttl_days < 3 {
            return Err("store_ttl_days must be at least 3 (webhook senders retry for days)".to_owned());
        }
        if let Some(ha) = &self.ha {
            ha.validate(&self.store)?;
        }
        let mut names = std::collections::HashSet::new();
        for deployment in &self.deployments {
            if graphrt_core::coordination::validate_key(&deployment.name).is_err() || deployment.name.contains('/') {
                return Err(format!("invalid deployment name `{}`", deployment.name));
            }
            if let Some(provider) = &deployment.provider
                && provider != "local"
                && !self.providers.contains_key(provider)
            {
                return Err(format!("{}: provider `{provider}` is not defined under providers:", deployment.name));
            }
            if !names.insert(deployment.name.as_str()) {
                return Err(format!("deployment `{}` appears twice", deployment.name));
            }
            for mapping in &deployment.input_headers {
                let lower = mapping.header.to_ascii_lowercase();
                if lower.starts_with("graphrt-") || lower.starts_with("x-graphrt-") {
                    return Err(format!("{}: input header `{}` is reserved", deployment.name, mapping.header));
                }
            }
            if !deployment.open && deployment.api_keys.is_empty() && deployment.jwt.is_none() {
                return Err(format!(
                    "{}: configure api_keys or jwt (open deployments are only for --dev)",
                    deployment.name
                ));
            }
            if deployment.api_keys.iter().any(|hash| hash.len() != 64 || crate::hex::decode(hash).is_none()) {
                return Err(format!("{}: api_keys are SHA-256 hex digests (graphrt gateway key new)", deployment.name));
            }
            deployment.warm.validate().map_err(|error| format!("{}: {error}", deployment.name))?;
            for cron in &deployment.crons {
                crate::cron::Schedule::new(&cron.id, &cron.schedule, &cron.timezone)
                    .map_err(|error| format!("{}: cron {}: {error}", deployment.name, cron.id))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_example() {
        let yaml = r#"
listen: 127.0.0.1:0
store: ":memory:"
per_ip: { per_second: 20, burst: 40 }
deployments:
  - name: shop
    keys_env: SHOP_KEYS
    api_keys: ["0000000000000000000000000000000000000000000000000000000000000000"]
    input_headers: [{ header: authorization, input: user_jwt, strip_bearer: true }]
    pool: { max_active_runs: 4 }
    hooks: [{ id: github, scheme: github, secret_env: GH, idempotency_header: x-github-delivery }]
    crons: [{ id: nightly, schedule: "0 3 * * *", catchup: latest, jitter_secs: 60 }]
"#;
        let config = GatewayConfig::from_yaml(yaml).unwrap();
        assert_eq!(config.deployments[0].pool_config().max_active_runs, 4);
        assert_eq!(config.deployments[0].crons[0].catchup, Catchup::Latest);
        assert!(!config.deployments[0].warm.predictive, "predictive prewarm is off by default");
        assert!(config.deployments[0].pool_config().warm_budget.is_none(), "no budget by default");
        assert_eq!(config.wake_storm.max_parallel_wakes, 16);
    }

    #[test]
    fn refuses_unsafe_settings() {
        let open = "deployments: [{ name: shop, keys_env: K }]";
        assert!(GatewayConfig::from_yaml(open).unwrap_err().contains("api_keys or jwt"));
        let reserved = "deployments: [{ name: shop, keys_env: K, open: true, input_headers: [{ header: Graphrt-Epoch, input: x }] }]";
        assert!(GatewayConfig::from_yaml(reserved).unwrap_err().contains("reserved"));
        assert!(GatewayConfig::from_yaml("store_ttl_days: 1").is_err());
        let typed =
            "deployments:\n  - name: shop\n    keys_env: K\n    open: true\n    pool: { max_active_runs: many }\n";
        let error = GatewayConfig::from_yaml(typed).unwrap_err();
        assert!(error.starts_with("gateway config: deployments[0].pool.max_active_runs (line 5, column "), "{error}");
        assert!(error.contains("invalid type: string \"many\""), "{error}");
        let threshold = "deployments: [{ name: shop, keys_env: K, open: true, warm: { threshold: 2 } }]";
        assert!(GatewayConfig::from_yaml(threshold).unwrap_err().contains("warm.threshold"));
    }
}
