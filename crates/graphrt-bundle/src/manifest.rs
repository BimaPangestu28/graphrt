//! The `bundle.json` manifest.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Current bundle format version.
pub const BUNDLE_FORMAT_VERSION: u32 = 1;
/// Path of the manifest inside the archive.
pub const MANIFEST_PATH: &str = "bundle.json";
/// Path of the optional signature inside the archive.
pub const SIGNATURE_PATH: &str = "bundle.sig";

/// Bundle manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleManifest {
    pub format_version: u32,
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Creation time (RFC 3339, UTC).
    pub created_at: String,
    /// Workflow run by default (a path listed in `workflows`).
    pub entry: String,
    /// Workflow documents in the bundle.
    pub workflows: Vec<String>,
    #[serde(default)]
    pub requires: Requires,
    /// SHA-256 (hex) of every file except the manifest and signature.
    pub files: BTreeMap<String, String>,
    /// Budgets every run of the bundle gets, child runs included, unless the
    /// host sets its own (`--max-tokens-per-run`, `--max-cost-per-run`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<BundleLimits>,
}

/// The budgets of a bundle's runs. Cost is in the price unit of the
/// providers file (`prices` per model), in millionths.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleLimits {
    /// Tokens (prompt and completion) a run may use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total_tokens: Option<u64>,
    /// Cost a run may reach, in millionths of the price unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_micros: Option<u64>,
}

impl BundleLimits {
    /// Whether no budget is set.
    pub fn is_empty(&self) -> bool {
        self.max_total_tokens.is_none() && self.max_cost_micros.is_none()
    }
}

/// What a host needs to run the bundle.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requires {
    /// Semver requirement on the graphrt runtime version (e.g. `>=0.1`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub graphrt: String,
    /// Node kinds used by the workflows (built-in or plugin).
    #[serde(default)]
    pub node_kinds: Vec<String>,
    /// Plugins shipped in the bundle.
    #[serde(default)]
    pub plugins: Vec<PluginRef>,
    /// Model providers referenced by the workflows.
    #[serde(default)]
    pub providers: Vec<String>,
    /// Environment variables declared by the workflows (names only, never values).
    #[serde(default)]
    pub env: Vec<String>,
}

/// A plugin shipped in the bundle (under `plugins/<name>/`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginRef {
    pub name: String,
    #[serde(default)]
    pub version: String,
    /// `wasm`, `process` or `http`.
    pub runtime: String,
    /// Whether the plugin runs on every host (only WebAssembly plugins do).
    pub portable: bool,
    /// SHA-256 of the plugin manifest file.
    pub sha256: String,
}
