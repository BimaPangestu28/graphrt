//! Active/passive gateways on a shared volume (plan F3, store tier T1-AP).
//!
//! Two (or more) `graphrt gateway` processes run with the same configuration
//! and the same store path on a volume they can all reach with coherent
//! locks: one host, or two hosts behind a single-mounter replicated block
//! device (DRBD, a SAN LUN moved by Pacemaker). One is **active**: it holds the
//! [`lease`], opens the SQLite store and serves. The others are **passive**:
//! they only poll the lease (no store, no listener, a few MiB of RSS) and take
//! over when the active exits, releases, freezes (`takeover: auto`) or an
//! operator runs `graphrt gateway ha promote`.
//!
//! Safety (I8, never two actives committing writes) is the store [`fence`]:
//! promotion raises `meta.fence` to the new token and every write checks it,
//! inside the same SQLite transaction. The lease only decides liveness.
//!
//! After a takeover the new active does what a restarted T1 gateway does
//! ([`crate::supervisor::Gateway::recover`]): SQLite replays its WAL, runs
//! that were `running` wait for their agents' orphan reports (polled from the
//! provider's replicas, which the local-process provider adopts from their
//! pid files), cron resumes from its stored last fire (deterministic run ids:
//! a repeated fire is a duplicate) and webhook markers dedupe redeliveries.

pub mod fence;
pub mod lease;
#[cfg(feature = "gw-local")]
pub mod run;

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

pub use lease::{Identity, Takeover};

/// `ha:` section of `gateway.yaml`.
///
/// ```yaml
/// ha:
///   node_id: gw-a            # default <host>-<pid>
///   lease_dir: /srv/graphrt  # default: the store's directory
///   takeover: auto           # auto | manual
///   heartbeat_ms: 1000
///   stale_after_ms: 5000
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HaConfig {
    /// Node id in the lease and logs.
    #[serde(default)]
    pub node_id: Option<String>,
    /// Directory of the lease files (must be on the store's volume).
    #[serde(default)]
    pub lease_dir: Option<PathBuf>,
    /// Takeover policy for a live-looking but silent holder.
    #[serde(default)]
    pub takeover: Takeover,
    /// Heartbeat period of the active.
    #[serde(default = "default_heartbeat")]
    pub heartbeat_ms: u64,
    /// Unchanged heartbeat duration after which `auto` steals the lease.
    #[serde(default = "default_stale_after")]
    pub stale_after_ms: u64,
    /// Lease poll period of a passive.
    #[serde(default = "default_poll")]
    pub poll_ms: u64,
    /// `SIGKILL` a stale holder before stealing when it provably runs on this host.
    #[serde(default = "default_true")]
    pub kill_stale_holder: bool,
    /// `PRAGMA quick_check` the store before serving; restore the newest
    /// backup when it fails or the file is missing.
    #[serde(default = "default_true")]
    pub verify_store: bool,
    /// Longest wait for open connections on a graceful shutdown before the
    /// lease is released (agents keep their runs and report them as orphans).
    #[serde(default = "default_shutdown_grace")]
    pub shutdown_grace_ms: u64,
}

fn default_heartbeat() -> u64 {
    1_000
}
fn default_stale_after() -> u64 {
    5_000
}
fn default_poll() -> u64 {
    200
}
fn default_true() -> bool {
    true
}
fn default_shutdown_grace() -> u64 {
    2_000
}

impl HaConfig {
    /// Checks timing bounds.
    ///
    /// # Errors
    /// A message naming the problem.
    pub fn validate(&self, store: &Path) -> Result<(), String> {
        if store == Path::new(":memory:") {
            return Err("ha: the store must be a file on the shared volume, not :memory:".to_owned());
        }
        if self.heartbeat_ms < 50 {
            return Err("ha: heartbeat_ms must be at least 50".to_owned());
        }
        if self.stale_after_ms < 3 * self.heartbeat_ms {
            return Err("ha: stale_after_ms must be at least 3 x heartbeat_ms".to_owned());
        }
        if self.poll_ms < 10 || self.poll_ms > self.stale_after_ms {
            return Err("ha: poll_ms must be between 10 and stale_after_ms".to_owned());
        }
        if let Some(node) = &self.node_id
            && (node.is_empty() || node.len() > 128 || node.chars().any(char::is_control))
        {
            return Err("ha: node_id must be 1-128 printable characters".to_owned());
        }
        Ok(())
    }

    /// The lease directory: `lease_dir`, else the store's directory.
    pub fn lease_dir_for(&self, store: &Path) -> PathBuf {
        self.lease_dir.clone().unwrap_or_else(|| match store.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        })
    }

    /// Lease timing.
    pub fn lease_settings(&self) -> lease::LeaseSettings {
        lease::LeaseSettings {
            takeover: self.takeover,
            stale_after_ms: self.stale_after_ms,
            kill_stale_holder: self.kill_stale_holder,
        }
    }

    /// Self-fence window: an active stops writing when its last confirmed
    /// heartbeat is older than this, one heartbeat before any passive may
    /// consider it stale.
    pub fn self_fence_window_ms(&self) -> u64 {
        self.stale_after_ms.saturating_sub(self.heartbeat_ms).max(self.heartbeat_ms)
    }

    /// Heartbeat period.
    pub fn heartbeat(&self) -> Duration {
        Duration::from_millis(self.heartbeat_ms)
    }
}

/// Host name: `$GRAPHRT_HOSTNAME`, `/etc/hostname`, `hostname`, else `localhost`.
pub fn host_name() -> String {
    let from_env = std::env::var("GRAPHRT_HOSTNAME").ok();
    let from_file = || std::fs::read_to_string("/etc/hostname").ok();
    let from_command = || {
        std::process::Command::new("hostname")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
    };
    from_env
        .or_else(from_file)
        .or_else(from_command)
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "localhost".to_owned())
}

/// This process's identity (`node_id` override, else `<host>-<pid>`).
pub fn identity(node_id: Option<&str>) -> Identity {
    let host = host_name();
    let pid = std::process::id();
    let node = node_id.map_or_else(|| format!("{host}-{pid}"), str::to_owned);
    Identity { node, host, pid }
}

/// `SIGKILL`s a stale holder (through `/bin/kill`: the workspace forbids `unsafe`).
pub async fn kill_holder(pid: u32) -> bool {
    tokio::process::Command::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> HaConfig {
        serde_saphyr::from_str(yaml).unwrap()
    }

    #[test]
    fn validates_timing_and_derives_the_lease_dir() {
        let config = parse("node_id: gw-a\n");
        assert_eq!(config.takeover, Takeover::Auto);
        assert!(config.validate(Path::new("/srv/graphrt/gateway.db")).is_ok());
        assert_eq!(config.lease_dir_for(Path::new("/srv/graphrt/gateway.db")), PathBuf::from("/srv/graphrt"));
        assert_eq!(config.self_fence_window_ms(), 4_000);
        assert!(config.validate(Path::new(":memory:")).is_err());
        assert!(parse("heartbeat_ms: 1000\nstale_after_ms: 2000\n").validate(Path::new("g.db")).is_err());
        assert!(parse("poll_ms: 1\n").validate(Path::new("g.db")).is_err());
        assert!(serde_saphyr::from_str::<HaConfig>("unknown: 1\n").is_err());
    }
}
