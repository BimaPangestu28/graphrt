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
