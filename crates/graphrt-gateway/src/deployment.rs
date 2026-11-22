//! A deployment at run time: credentials, signing keys, provider, and its
//! immutable versions with one replica pool each (decision 6: a superseded
//! version keeps a pool with `min_replicas = 0` for 7 days so its paused runs
//! can still resume; afterwards a resume is `410 version_retired`).

use std::sync::{Arc, RwLock};
use std::time::Duration;

use graphrt_provider_api::{DeploymentRef, Provider};
use serde::{Deserialize, Serialize};

use crate::auth::DeploymentAuth;
use crate::limiter::Gcra;
use crate::pool::{Pool, PoolConfig};
use crate::signature::{GatewayKey, Verifier};

/// How long a superseded version stays resumable.
pub const VERSION_RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);

/// A deployed version, persisted at `d/<deployment>/versions/<version>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    /// Immutable version id.
    pub version: String,
    /// Workflow name served by the agent.
    pub workflow: String,
    /// Fingerprint of the workflow graph.
    pub fingerprint: String,
    /// Provider target (`DeploymentRef::target`).
    pub target: String,
    /// Agent command (local-process provider).
    #[serde(default)]
    pub command: Vec<String>,
    /// Start inputs the workflow declares `type: secret`. They are stripped
    /// from the request the store keeps for a restart (never written).
    /// `None` (registered by an older `graphrt deploy`): every input counts as
    /// one, so nothing a version did not declare is ever persisted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_inputs: Option<Vec<String>>,
    /// When it became current.
    pub activated_ms: u64,
    /// When a newer version replaced it.
    #[serde(default)]
    pub superseded_ms: Option<u64>,
}

impl VersionInfo {
    /// The provider reference of this version.
    pub fn reference(&self, deployment: &str) -> DeploymentRef {
        DeploymentRef::new(deployment, &self.version, &self.target)
    }
}

/// A version and its pool.
#[derive(Debug, Clone)]
pub struct VersionEntry {
    /// Metadata.
    pub info: VersionInfo,
    /// Replicas.
    pub pool: Arc<Pool>,
}

/// Why a version cannot take a run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VersionError {
    /// Nothing deployed yet.
    #[error("deployment has no version yet")]
    NothingDeployed,
    /// Unknown version.
    #[error("unknown deployment version `{0}`")]
    Unknown(String),
    /// Superseded more than 7 days ago.
    #[error("deployment version `{0}` was retired")]
    Retired(String),
}

/// Header → input mapping (e.g. `authorization` → `user_jwt`), kept in memory only.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct InputHeader {
    /// Request header name (lowercase).
    pub header: String,
    /// Input field it fills.
    pub input: String,
    /// Strip a `Bearer ` prefix.
    #[serde(default)]
    pub strip_bearer: bool,
}

/// A deployment.
pub struct Deployment {
    /// Name (namespace of run ids, keys, `aud`).
    pub name: String,
    /// Client credentials.
    pub auth: DeploymentAuth,
    /// HMAC keys shared with this deployment's agents (first signs).
    pub keys: Vec<GatewayKey>,
    verifier: Verifier,
    /// Run-time provider.
    pub provider: Arc<dyn Provider>,
    /// Pool settings of every version.
    pub pool_config: PoolConfig,
    /// Per-user (JWT `sub`) / per-key limit.
    pub principal_limiter: Option<Gcra<String>>,
    /// Header inputs.
    pub input_headers: Vec<InputHeader>,
    /// Whether requests without credentials are accepted (`--dev` only).
    pub open: bool,
    /// Frees platform resources of versions past the 7-day pool (Cloud Run, ACA, Fly).
    pub retirer: Option<Arc<dyn crate::retire::VersionRetirer>>,
    /// Retired versions whose platform retirement failed (retried hourly).
    pub pending_retirement: std::sync::Mutex<Vec<DeploymentRef>>,
    versions: RwLock<Vec<VersionEntry>>,
}

impl std::fmt::Debug for Deployment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Deployment").field("name", &self.name).finish_non_exhaustive()
    }
}

impl Deployment {
    /// Builds a deployment with no version yet.
    ///
    /// # Panics
    /// Never: `keys` is checked non-empty by the caller (config validation).
    pub fn new(
        name: &str,
        auth: DeploymentAuth,
        keys: Vec<GatewayKey>,
        provider: Arc<dyn Provider>,
        pool_config: PoolConfig,
    ) -> Self {
        Self {
            name: name.to_owned(),
            auth,
            verifier: Verifier::new(keys.clone()),
            keys,
            provider,
            pool_config,
            principal_limiter: None,
            input_headers: Vec::new(),
            open: false,
            retirer: None,
            pending_retirement: std::sync::Mutex::new(Vec::new()),
            versions: RwLock::new(Vec::new()),
        }
    }

    /// Signing key.
    pub fn signing_key(&self) -> Option<&GatewayKey> {
        self.keys.first()
    }

    /// Verifier of agent → gateway requests.
    pub fn verifier(&self) -> &Verifier {
        &self.verifier
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Vec<VersionEntry>> {
        self.versions.read().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Adds a version and makes it current; the previous current one is
    /// marked superseded at `now_ms`. Re-adding a known version is a no-op.
    pub fn add_version(&self, mut info: VersionInfo, now_ms: u64) -> VersionEntry {
        let mut versions = self.versions.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = versions.iter().find(|entry| entry.info.version == info.version) {
            return existing.clone();
        }
        if info.superseded_ms.is_none() {
            for entry in versions.iter_mut().filter(|entry| entry.info.superseded_ms.is_none()) {
                entry.info.superseded_ms = Some(now_ms);
            }
        }
        if info.activated_ms == 0 {
            info.activated_ms = now_ms;
        }
        let mut config = self.pool_config.clone();
        if info.superseded_ms.is_some() {
            config.min_replicas = 0;
        }
        let entry = VersionEntry { pool: Pool::new(info.reference(&self.name), self.provider.clone(), config), info };
        versions.push(entry.clone());
        versions.sort_by_key(|entry| (entry.info.superseded_ms.is_none(), entry.info.activated_ms));
        entry
    }

    /// The current version.
    ///
    /// # Errors
    /// [`VersionError::NothingDeployed`].
    pub fn current(&self) -> Result<VersionEntry, VersionError> {
        self.read()
            .iter()
            .rev()
            .find(|entry| entry.info.superseded_ms.is_none())
            .cloned()
            .ok_or(VersionError::NothingDeployed)
    }

    /// `version` if it may still run at `now_ms` (7-day pool of old versions).
    ///
    /// # Errors
    /// [`VersionError::Unknown`] or [`VersionError::Retired`].
    pub fn version(&self, version: &str, now_ms: u64) -> Result<VersionEntry, VersionError> {
        let versions = self.read();
        let entry = versions
            .iter()
            .find(|entry| entry.info.version == version)
            .ok_or_else(|| VersionError::Unknown(version.to_owned()))?;
        if entry
            .info
            .superseded_ms
            .is_some_and(|superseded| now_ms.saturating_sub(superseded) > VERSION_RETENTION.as_millis() as u64)
        {
            return Err(VersionError::Retired(version.to_owned()));
        }
        Ok(entry.clone())
    }

    /// All versions, oldest first.
    pub fn versions(&self) -> Vec<VersionEntry> {
        self.read().clone()
    }

    /// Removes versions retired at `now_ms` and returns their pools (to close).
    pub fn take_retired(&self, now_ms: u64) -> Vec<VersionEntry> {
        let mut versions = self.versions.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        let retention = VERSION_RETENTION.as_millis() as u64;
        let (retired, kept): (Vec<_>, Vec<_>) = versions.drain(..).partition(|entry| {
            entry.info.superseded_ms.is_some_and(|superseded| now_ms.saturating_sub(superseded) > retention)
        });
        *versions = kept;
        retired
    }
}

#[cfg(test)]
mod tests {
    use graphrt_provider_api::conformance::MemoryProvider;

    use super::*;

    fn info(version: &str) -> VersionInfo {
        VersionInfo {
            version: version.into(),
            workflow: "wf".into(),
            fingerprint: format!("fp-{version}"),
            target: "mem".into(),
            command: Vec::new(),
            secret_inputs: None,
            activated_ms: 0,
            superseded_ms: None,
        }
    }

    #[test]
    fn old_versions_resume_for_seven_days() {
        let key = GatewayKey::new("k", &[1; 32]).unwrap();
        let deployment = Deployment::new(
            "shop",
            DeploymentAuth::default(),
            vec![key],
            Arc::new(MemoryProvider::new(Duration::ZERO)),
            PoolConfig::default(),
        );
        assert_eq!(deployment.current().unwrap_err(), VersionError::NothingDeployed);
        let day = 24 * 3600 * 1000;
        deployment.add_version(info("v1"), 0);
        deployment.add_version(info("v2"), day);
        assert_eq!(deployment.current().unwrap().info.version, "v2");
        assert!(deployment.version("v1", day * 7).is_ok(), "day 6 after supersede: resumable");
        assert_eq!(deployment.version("v1", day * 9).unwrap_err(), VersionError::Retired("v1".into()));
        assert_eq!(deployment.take_retired(day * 9).len(), 1);
        assert_eq!(deployment.version("v3", 0).unwrap_err(), VersionError::Unknown("v3".into()));
    }
}
