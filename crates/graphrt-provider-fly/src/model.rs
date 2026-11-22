//! The slice of the Fly Machines API schema the provider reads and writes.
//!
//! Only the fields graphrt needs are modelled; everything else Fly returns is
//! ignored on read (no `deny_unknown_fields`), so API additions do not break
//! the gateway. Field names follow `https://docs.machines.dev` (v1).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Metadata key holding the graphrt deployment name on every pooled Machine.
pub const METADATA_DEPLOYMENT: &str = "graphrt_deployment";
/// Metadata key holding the immutable deployment version.
pub const METADATA_VERSION: &str = "graphrt_version";
/// Metadata key marking a Machine as owned by the graphrt pool (`"1"`).
pub const METADATA_POOL: &str = "graphrt_pool";

/// Lifecycle states of a Machine, as reported by `GET /machines`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MachineState {
    /// Created with `skip_launch`, never booted.
    Created,
    Starting,
    Started,
    Stopping,
    Stopped,
    Suspending,
    Suspended,
    Replacing,
    Destroying,
    Destroyed,
    Failed,
    /// Any state this crate does not know yet.
    #[serde(other)]
    Unknown,
}

impl MachineState {
    /// Whether a `start` call can bring the Machine up (cold or from a snapshot).
    pub fn is_startable(self) -> bool {
        matches!(self, Self::Created | Self::Stopped | Self::Suspended)
    }

    /// Whether the Machine is on its way out and must not be picked.
    pub fn is_gone(self) -> bool {
        matches!(self, Self::Destroying | Self::Destroyed | Self::Replacing)
    }

    /// The name the API uses (`state=` of `/wait`).
    pub fn as_api_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Starting => "starting",
            Self::Started => "started",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Suspending => "suspending",
            Self::Suspended => "suspended",
            Self::Replacing => "replacing",
            Self::Destroying => "destroying",
            Self::Destroyed => "destroyed",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
}

/// A Machine as returned by the API.
#[derive(Clone, Debug, Deserialize)]
pub struct Machine {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub state: MachineState,
    #[serde(default)]
    pub region: String,
    /// 6PN address (`fdaa:...`), reachable from anything in the same Fly organization.
    #[serde(default)]
    pub private_ip: String,
    /// The Machine's config, kept as opaque JSON: the provider only reads its
    /// metadata, and a clone of a pool member must carry every field Fly
    /// returned (services, mounts, files...), not just the ones modelled here.
    #[serde(default)]
    pub config: Option<serde_json::Value>,
}

impl Machine {
    /// One metadata value of the Machine's config.
    pub fn metadata_value(&self, key: &str) -> Option<&str> {
        self.config.as_ref()?.get("metadata")?.get(key)?.as_str()
    }

    /// Whether this Machine belongs to the pool of `deployment` at `version`.
    pub fn belongs_to(&self, deployment: &str, version: &str) -> bool {
        self.metadata_value(METADATA_POOL) == Some("1")
            && self.metadata_value(METADATA_DEPLOYMENT) == Some(deployment)
            && self.metadata_value(METADATA_VERSION) == Some(version)
    }
}

/// `config` of a Machine `graphrt deploy` creates: what to run and how to stop it.
/// Only serialized; Machines read back keep their config as opaque JSON.
#[derive(Clone, Debug, Default, Serialize)]
pub struct MachineConfig {
    pub image: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guest: Option<GuestConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub init: Option<InitConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart: Option<RestartConfig>,
    /// Signal and grace (`kill_timeout`) Fly applies on `stop`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_config: Option<StopConfig>,
    pub auto_destroy: bool,
}

/// CPU and memory of a Machine.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct GuestConfig {
    /// `shared` or `performance`.
    pub cpu_kind: String,
    pub cpus: u32,
    pub memory_mb: u32,
}

/// Overrides of the image's entrypoint/command.
#[derive(Clone, Debug, Default, Serialize)]
pub struct InitConfig {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cmd: Vec<String>,
}

/// What Fly does when the process exits on its own.
#[derive(Clone, Debug, Serialize)]
pub struct RestartConfig {
    /// `no`, `always` or `on-failure`.
    pub policy: String,
}

/// `stop_config`: the signal sent on stop and how long Fly waits before SIGKILL.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct StopConfig {
    pub signal: String,
    /// Go duration, e.g. `"300s"`.
    pub timeout: String,
}

/// Body of `POST /apps/{app}/machines`.
#[derive(Clone, Debug, Serialize)]
pub struct CreateMachineRequest<'request> {
    pub name: String,
    #[serde(skip_serializing_if = "str::is_empty")]
    pub region: &'request str,
    /// A [`MachineConfig`] (deploy) or a pool member's config (clone), as JSON.
    pub config: &'request serde_json::Value,
    /// Create the Machine without booting it: a pooled replica costs only its rootfs.
    pub skip_launch: bool,
}

/// Body of `POST /apps/{app}/machines/{id}/stop`.
#[derive(Clone, Debug, Serialize)]
pub struct StopMachineRequest {
    pub signal: String,
    pub timeout: String,
}

/// Body of `POST /apps` (create app).
#[derive(Clone, Debug, Serialize)]
pub struct CreateAppRequest {
    pub app_name: String,
    pub org_slug: String,
    /// Custom 6PN network; empty = the organization's default network.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub network: String,
}

/// Body of `POST /apps/{app}/secrets` (bulk update; `None` deletes a secret).
#[derive(Debug, Serialize)]
pub struct UpdateSecretsRequest<'values> {
    pub values: BTreeMap<&'values str, Option<&'values str>>,
}

/// Response of `GET /apps/{app}/secrets` (names and digests, never values).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ListSecretsResponse {
    #[serde(default)]
    pub secrets: Vec<SecretSummary>,
}

/// One app secret without its value.
#[derive(Clone, Debug, Deserialize)]
pub struct SecretSummary {
    pub name: String,
}

/// Error body of the Machines API: `{"error": "...", "status": "..."}`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ApiErrorBody {
    #[serde(default)]
    pub error: String,
    /// Machine-readable code (`insufficient_capacity`, `name_taken`, ...), often absent.
    #[serde(default)]
    pub status: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_machine_and_ignores_unknown_fields() {
        let machine: Machine = serde_json::from_str(
            r#"{"id":"148e","name":"a","state":"suspended","region":"ams","private_ip":"fdaa:0:1::2",
                "instance_id":"01H","image_ref":{"x":1},"events":[],
                "config":{"image":"img","metadata":{"graphrt_pool":"1","graphrt_deployment":"chat","graphrt_version":"v3"},"auto_destroy":false}}"#,
        )
        .unwrap();
        assert_eq!(machine.state, MachineState::Suspended);
        assert!(machine.belongs_to("chat", "v3"));
        assert!(!machine.belongs_to("chat", "v2"));
    }

    #[test]
    fn unknown_state_does_not_fail_parsing() {
        let machine: Machine = serde_json::from_str(r#"{"id":"1","state":"hibernating"}"#).unwrap();
        assert_eq!(machine.state, MachineState::Unknown);
        assert!(!machine.state.is_startable());
    }
}
