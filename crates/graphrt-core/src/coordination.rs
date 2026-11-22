//! Versioned key-value objects for coordinating writers that share one store.
//!
//! A [`CoordinationStore`] keeps small values (JSON documents, markers) under
//! slash-separated keys, each with a version number that changes on every
//! write. The only ways to write are *create if absent* and *compare and
//! swap*, which map onto what every backend can do atomically: a local
//! transaction (SQLite), an OS lock (a directory), or a conditional PUT
//! (`If-None-Match: *` / `If-Match` on S3 and R2, `x-goog-if-generation-match`
//! on GCS).
//!
//! It is what a gateway uses for idempotency markers (webhook deliveries, cron
//! fires: create-if-absent), leases and fencing (compare-and-swap), and
//! anything else that must be decided exactly once across restarts.
//!
//! Keys are namespaced per deployment with [`deployment_key`], e.g.
//! `d/<deployment>/runs/<run_id>`.

use std::fmt;

use futures::future::BoxFuture;

use crate::store::{StoreError, StoreResult};

/// Largest key, in bytes.
pub const MAX_KEY_BYTES: usize = 512;
/// Largest value, in bytes. Coordination objects stay well below the single-PUT
/// limits of object stores (5 MiB), so no backend needs multipart uploads.
pub const MAX_VALUE_BYTES: usize = 1024 * 1024;

/// A stored value and its version. Versions start at 1 and increase by one per
/// write of the key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Versioned {
    pub value: Vec<u8>,
    pub version: u64,
}

/// Versioned objects written only with create-if-absent and compare-and-swap.
///
/// Implementations must make every write atomic: of two concurrent
/// `create_if_absent` calls (or two `compare_and_swap` calls with the same
/// expected version) on one key, exactly one succeeds. A successful write is
/// visible to every later `get` and `list` (read-after-write). The
/// `store-conformance` feature of this crate ships the test suite.
pub trait CoordinationStore: Send + Sync + fmt::Debug {
    /// Store `value` under `key` at version 1. Fails with [`StoreError::Conflict`]
    /// when the key exists.
    fn create_if_absent<'a>(&'a self, key: &'a str, value: Vec<u8>) -> BoxFuture<'a, StoreResult<u64>>;
    /// Replace the value of `key` if it is at `expected_version`, and return the
    /// new version. Fails with [`StoreError::Precondition`] when the version
    /// moved, [`StoreError::NotFound`] when the key does not exist.
    fn compare_and_swap<'a>(
        &'a self,
        key: &'a str,
        expected_version: u64,
        value: Vec<u8>,
    ) -> BoxFuture<'a, StoreResult<u64>>;
    /// The value of `key`, if any.
    fn get_object<'a>(&'a self, key: &'a str) -> BoxFuture<'a, StoreResult<Option<Versioned>>>;
    /// Every key starting with `prefix`, in ascending key order.
    fn list_objects<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, StoreResult<Vec<(String, Versioned)>>>;
    /// Delete `key` if it is at `expected_version`. Returns whether it existed;
    /// fails with [`StoreError::Precondition`] when the version moved.
    fn delete_object<'a>(&'a self, key: &'a str, expected_version: u64) -> BoxFuture<'a, StoreResult<bool>>;
}

/// Validate a coordination key: 1–[`MAX_KEY_BYTES`] bytes of `/`-separated
/// segments of `[A-Za-z0-9_.:=-]`, none empty, `.` or `..`.
///
/// @throws StoreError::InvalidId - When the key breaks one of these rules.
pub fn validate_key(key: &str) -> StoreResult<()> {
    let segment_ok = |segment: &str| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '='))
    };
    if !key.is_empty() && key.len() <= MAX_KEY_BYTES && key.split('/').all(segment_ok) {
        Ok(())
    } else {
        Err(StoreError::InvalidId(key.chars().take(64).collect()))
    }
}

/// Validate a value size.
///
/// @throws StoreError::Conflict - When the value is larger than [`MAX_VALUE_BYTES`].
pub fn validate_value(value: &[u8]) -> StoreResult<()> {
    if value.len() <= MAX_VALUE_BYTES {
        Ok(())
    } else {
        Err(StoreError::Conflict(format!("coordination value of {} bytes exceeds {MAX_VALUE_BYTES}", value.len())))
    }
}

/// The key of object `id` of `kind` in `deployment`: `d/<deployment>/<kind>/<id>`.
///
/// @example
/// assert_eq!(deployment_key("shop", "runs", "run-1"), "d/shop/runs/run-1");
pub fn deployment_key(deployment: &str, kind: &str, id: &str) -> String {
    format!("d/{deployment}/{kind}/{id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_validated() {
        for good in ["d/shop/runs/run-1", "a", "d/x/cron/daily/2026-11-22T00:00:00Z", "k=v"] {
            assert!(validate_key(good).is_ok(), "{good}");
        }
        let long = "x".repeat(MAX_KEY_BYTES + 1);
        for bad in ["", "/a", "a/", "a//b", "a/../b", "./a", "a b", "a\\b", &long] {
            assert!(validate_key(bad).is_err(), "{bad}");
        }
        assert_eq!(deployment_key("shop", "runs", "r1"), "d/shop/runs/r1");
        assert!(validate_value(&vec![0; MAX_VALUE_BYTES]).is_ok());
        assert!(validate_value(&vec![0; MAX_VALUE_BYTES + 1]).is_err());
    }
}
