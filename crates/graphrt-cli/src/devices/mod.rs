//! Access tokens for monitoring devices (`graphrt serve --devices`, cargo
//! feature `devices`).
//!
//! The API key reads and changes everything, so it should not live on a phone.
//! A device token is a second kind of credential for that:
//!
//! - **Scoped.** It reads runs (`runs:read`) and resolves approvals
//!   (`approvals:resolve`), and nothing else: it starts no run, changes no
//!   knowledge, deletes nothing and creates no further token.
//! - **Revocable.** Every token belongs to a listed device. Deleting the device
//!   ends the token at once, on every server that shares the store directory.
//! - **Stored hashed.** The server keeps the SHA-256 of the token's secret. The
//!   token itself is shown once, when the device is created.
//!
//! ```text
//! grd1.<device id>.<secret>
//! ```
//!
//! `secret` is 256 random bits (base64url), so a plain hash is enough: there
//! is nothing to guess that a slow hash would protect.
//!
//! Devices live in `<store>/devices.json` (owner-only, replaced atomically), or
//! in memory without a store. The API is in [`api`].

pub mod api;

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use graphrt_core::private_fs;
use graphrt_core::store::now_ms;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::CliError;
use crate::shared_file::SharedJsonFile;

/// Prefix of device tokens; the digit is the format version.
pub const TOKEN_PREFIX: &str = "grd1.";
/// The file of the devices inside the store directory.
const FILE_NAME: &str = "devices.json";
/// The most devices a server keeps.
pub const MAX_DEVICES: usize = 100;
/// The longest device name, in bytes.
pub const MAX_NAME_BYTES: usize = 100;
/// How often a device's "last used" time is written to disk at most.
const LAST_USED_WRITE_EVERY_MS: u64 = 60_000;
const MS_PER_DAY: u64 = 24 * 60 * 60 * 1000;

/// Whether `token` claims to be a device token (it may still be invalid).
pub fn is_device_token(token: &str) -> bool {
    token.starts_with(TOKEN_PREFIX)
}

/// What a device token may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scope {
    /// List runs, read a run and follow its events.
    #[serde(rename = "runs:read")]
    RunsRead,
    /// List what waits for a person and answer it.
    #[serde(rename = "approvals:resolve")]
    ApprovalsResolve,
}

impl Scope {
    /// Every scope: what a new device gets unless the request names some.
    pub const ALL: [Scope; 2] = [Scope::RunsRead, Scope::ApprovalsResolve];

    /// The scope as requests and answers write it.
    pub fn name(self) -> &'static str {
        match self {
            Scope::RunsRead => "runs:read",
            Scope::ApprovalsResolve => "approvals:resolve",
        }
    }
}

/// A paired device, as stored and as listed (the hash is never listed).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub scopes: Vec<Scope>,
    pub created_ms: u64,
    /// When the token stops working; `None` for no expiry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_ms: Option<u64>,
    /// When the token was last used, to the minute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_ms: Option<u64>,
    /// SHA-256 of the token's secret, hex.
    token_hash: String,
}

impl Device {
    /// The device without its hash, for API answers.
    pub fn public_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "name": self.name,
            "scopes": self.scopes,
            "created_ms": self.created_ms,
            "expires_ms": self.expires_ms,
            "last_used_ms": self.last_used_ms,
        })
    }
}

/// The device behind a request that carried a valid device token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceAccess {
    pub id: String,
    pub scopes: Vec<Scope>,
}

impl DeviceAccess {
    pub fn allows(&self, scope: Scope) -> bool {
        self.scopes.contains(&scope)
    }
}

/// Why a device request failed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum DeviceError {
    #[error("the device token is not valid")]
    InvalidToken,
    #[error("the device token expired")]
    Expired,
    #[error("{0}")]
    Invalid(String),
    #[error("this server already has {MAX_DEVICES} devices: remove one first")]
    TooMany,
    #[error("device store: {0}")]
    Io(String),
}

/// A device that was just created, with the only copy of its token.
#[derive(Debug)]
pub struct NewDevice {
    pub device: Device,
    pub token: String,
}

/// The devices of a server.
#[derive(Debug)]
pub struct Devices {
    /// `<store>/devices.json`, or the list in memory without a store.
    file: SharedJsonFile<Vec<Device>>,
    /// When "last used" times were last written.
    last_used_written_ms: AtomicU64,
    random: SystemRandom,
}

fn hash_secret(secret: &str) -> String {
    crate::hex::to_hex(&Sha256::digest(secret.as_bytes()))
}

impl From<String> for DeviceError {
    fn from(message: String) -> Self {
        DeviceError::Io(message)
    }
}

impl Devices {
    /// The devices in `store_dir` (created when missing), or in memory without one.
    ///
    /// @throws CliError (usage) - When the directory or the file cannot be read.
    pub fn open(store_dir: Option<&Path>) -> Result<Self, CliError> {
        let path = match store_dir {
            Some(dir) => {
                private_fs::create_dir_all(dir)
                    .map_err(|e| CliError::usage(format!("--devices: cannot create `{}`: {e}", dir.display())))?;
                Some(dir.join(FILE_NAME))
            }
            None => None,
        };
        let file = SharedJsonFile::open(path).map_err(|e| CliError::usage(format!("--devices: {e}")))?;
        Ok(Self { file, last_used_written_ms: AtomicU64::new(0), random: SystemRandom::new() })
    }

    /// One line for the startup log.
    pub fn describe(&self) -> String {
        let count = self.file.read(Vec::len).unwrap_or_default();
        match self.file.path() {
            Some(path) => format!("device tokens: {count} device(s) in {} (/v1/devices)", path.display()),
            None => format!(
                "device tokens: {count} device(s), kept in memory and gone at a restart (set --store to keep them)"
            ),
        }
    }

    /// Pair a device: a new id and token.
    ///
    /// @param name - What the owner calls the device, 1 to [`MAX_NAME_BYTES`] bytes.
    /// @param scopes - What the token may do; empty means every scope.
    /// @param expires_in_days - Days until the token stops working; `None` for no expiry.
    /// @throws DeviceError - `Invalid` for a bad name, `TooMany` at the limit, `Io` when the file cannot be written.
    pub fn create(&self, name: &str, scopes: &[Scope], expires_in_days: Option<u32>) -> Result<NewDevice, DeviceError> {
        let name = name.trim();
        if name.is_empty() || name.len() > MAX_NAME_BYTES || name.chars().any(char::is_control) {
            return Err(DeviceError::Invalid(format!(
                "`name` must be 1 to {MAX_NAME_BYTES} bytes without control characters"
            )));
        }
        let mut id_bytes = [0_u8; 8];
        let mut secret_bytes = [0_u8; 32];
        self.random
            .fill(&mut id_bytes)
            .and_then(|()| self.random.fill(&mut secret_bytes))
            .map_err(|_| DeviceError::Io("no system randomness".to_owned()))?;
        let id = format!("dev-{}", crate::hex::to_hex(&id_bytes));
        let secret = URL_SAFE_NO_PAD.encode(secret_bytes);
        let mut granted: Vec<Scope> = Scope::ALL.into_iter().filter(|scope| scopes.contains(scope)).collect();
        if granted.is_empty() {
            granted = Scope::ALL.to_vec();
        }
        let created_ms = now_ms();
        let device = Device {
            id: id.clone(),
            name: name.to_owned(),
            scopes: granted,
            created_ms,
            expires_ms: expires_in_days.map(|days| created_ms.saturating_add(u64::from(days) * MS_PER_DAY)),
            last_used_ms: None,
            token_hash: hash_secret(&secret),
        };
        self.file.update(|devices| {
            if devices.len() >= MAX_DEVICES {
                return Err(DeviceError::TooMany);
            }
            devices.push(device.clone());
            Ok(())
        })?;
        Ok(NewDevice { device, token: format!("{TOKEN_PREFIX}{id}.{secret}") })
    }

    /// Every device, oldest first.
    ///
    /// @throws DeviceError - `Io` when the file cannot be read.
    pub fn list(&self) -> Result<Vec<Device>, DeviceError> {
        Ok(self.file.read(Clone::clone)?)
    }

    /// Remove device `id`, which ends its token. Returns whether it existed.
    ///
    /// @throws DeviceError - `Io` when the file cannot be written.
    pub fn revoke(&self, id: &str) -> Result<bool, DeviceError> {
        if !self.file.read(|devices| devices.iter().any(|device| device.id == id))? {
            return Ok(false);
        }
        self.file.update(|devices| {
            let before = devices.len();
            devices.retain(|device| device.id != id);
            Ok::<_, DeviceError>(devices.len() < before)
        })
    }

    /// The device of `token`.
    ///
    /// @throws DeviceError - `InvalidToken` for a token of no listed device, `Expired` past its expiry.
    pub fn verify(&self, token: &str) -> Result<DeviceAccess, DeviceError> {
        self.verify_at(token, now_ms())
    }

    fn verify_at(&self, token: &str, now: u64) -> Result<DeviceAccess, DeviceError> {
        let (id, secret) =
            token.strip_prefix(TOKEN_PREFIX).and_then(|rest| rest.split_once('.')).ok_or(DeviceError::InvalidToken)?;
        let hash = hash_secret(secret);
        // "Last used" is written at most once a minute: not worth a write per request.
        let write = now.saturating_sub(self.last_used_written_ms.load(Ordering::Relaxed)) >= LAST_USED_WRITE_EVERY_MS;
        let verified = self.file.touch(write, |devices| {
            let device = devices.iter_mut().find(|device| device.id == id).ok_or(DeviceError::InvalidToken)?;
            if !crate::serve::constant_time_eq(hash.as_bytes(), device.token_hash.as_bytes()) {
                return Err(DeviceError::InvalidToken);
            }
            if device.expires_ms.is_some_and(|expires| now >= expires) {
                return Err(DeviceError::Expired);
            }
            device.last_used_ms = Some(now);
            Ok(DeviceAccess { id: device.id.clone(), scopes: device.scopes.clone() })
        });
        if write {
            self.last_used_written_ms.store(now, Ordering::Relaxed);
        }
        // A file that cannot be read must not let a revoked token through: refuse.
        verified.map_err(|_| DeviceError::InvalidToken)?
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("graphrt-devices-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_token_works_until_its_device_is_revoked() {
        let devices = Devices::open(None).unwrap();
        let paired = devices.create("  Bima's phone ", &[], None).unwrap();
        assert_eq!(paired.device.name, "Bima's phone");
        assert_eq!(paired.device.scopes, Scope::ALL);
        assert!(is_device_token(&paired.token));
        let access = devices.verify(&paired.token).unwrap();
        assert_eq!(access.id, paired.device.id);
        assert!(access.allows(Scope::RunsRead) && access.allows(Scope::ApprovalsResolve));
        assert!(devices.list().unwrap()[0].last_used_ms.is_some());
        assert!(devices.revoke(&paired.device.id).unwrap());
        assert!(!devices.revoke(&paired.device.id).unwrap());
        assert_eq!(devices.verify(&paired.token), Err(DeviceError::InvalidToken));
    }

    #[test]
    fn scopes_limit_a_token_and_wrong_tokens_are_refused() {
        let devices = Devices::open(None).unwrap();
        let reader = devices.create("wall display", &[Scope::RunsRead], None).unwrap();
        let access = devices.verify(&reader.token).unwrap();
        assert!(access.allows(Scope::RunsRead) && !access.allows(Scope::ApprovalsResolve));
        let id = &reader.device.id;
        for bad in [
            String::new(),
            "grd1.".to_owned(),
            format!("grd1.{id}"),
            format!("grd1.{id}."),
            format!("grd1.{id}.wrong"),
            format!("grd1.dev-0000.{}", reader.token.rsplit('.').next().unwrap()),
            reader.token.replace("grd1.", "grd2."),
        ] {
            assert_eq!(devices.verify(&bad), Err(DeviceError::InvalidToken), "{bad}");
        }
    }

    #[test]
    fn a_token_expires() {
        let devices = Devices::open(None).unwrap();
        let paired = devices.create("tablet", &[], Some(7)).unwrap();
        let expires = paired.device.expires_ms.unwrap();
        assert_eq!(expires, paired.device.created_ms + 7 * MS_PER_DAY);
        assert!(devices.verify_at(&paired.token, expires - 1).is_ok());
        assert_eq!(devices.verify_at(&paired.token, expires), Err(DeviceError::Expired));
    }

    #[test]
    fn names_and_the_number_of_devices_are_limited() {
        let devices = Devices::open(None).unwrap();
        for bad in ["", "   ", "a\nb", &"x".repeat(MAX_NAME_BYTES + 1)] {
            assert!(matches!(devices.create(bad, &[], None), Err(DeviceError::Invalid(_))), "{bad:?}");
        }
        for n in 0..MAX_DEVICES {
            devices.create(&format!("device {n}"), &[], None).unwrap();
        }
        assert_eq!(devices.create("one more", &[], None).unwrap_err(), DeviceError::TooMany);
    }

    #[test]
    fn devices_are_stored_hashed_and_shared_between_servers() {
        let dir = temp_dir("shared");
        let first = Devices::open(Some(&dir)).unwrap();
        let paired = first.create("phone", &[], None).unwrap();
        let secret = paired.token.rsplit('.').next().unwrap();
        let file = std::fs::read_to_string(dir.join(FILE_NAME)).unwrap();
        assert!(!file.contains(secret), "the token itself is never stored");
        assert!(file.contains(&hash_secret(secret)));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.join(FILE_NAME)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // A second server on the same directory accepts the token, and stops
        // when the first revokes the device.
        let second = Devices::open(Some(&dir)).unwrap();
        assert!(second.verify(&paired.token).is_ok());
        assert!(first.revoke(&paired.device.id).unwrap());
        assert_eq!(second.verify(&paired.token), Err(DeviceError::InvalidToken));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_listed_device_never_shows_its_hash() {
        let devices = Devices::open(None).unwrap();
        let paired = devices.create("phone", &[Scope::ApprovalsResolve], None).unwrap();
        let json = paired.device.public_json();
        assert_eq!(json["scopes"], serde_json::json!(["approvals:resolve"]));
        assert!(json.get("token_hash").is_none());
    }
}
