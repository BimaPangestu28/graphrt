//! The `bundle.json` manifest.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The newest bundle format this runtime reads (it reads every older one too).
/// A bundle is written in the oldest format that holds what it carries, so
/// older runtimes keep reading the bundles they can run:
///
/// - 1: workflows, plugins, knowledge, providers, skills, assets.
/// - 2: also `limits`, the budgets of every run. A runtime that does not know
///   budgets refuses such a bundle rather than run it without them.
pub const BUNDLE_FORMAT_VERSION: u32 = 2;

/// The format of a bundle without `limits`.
pub const BUNDLE_FORMAT_PLAIN: u32 = 1;
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
    /// Budgets every run of the bundle gets, child runs included. A host that
    /// sets its own (`--max-tokens-per-run`, `--max-cost-per-run`) gets the
    /// lower of the two. Format 2 and later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<BundleLimits>,
}

impl BundleManifest {
    /// The workflows besides the entry, each with the name agents list it
    /// under in `subagents`: its file name without the extension
    /// (`research` for `workflows/research.yaml`).
    pub fn subagent_workflows(&self) -> impl Iterator<Item = (&str, &str)> {
        self.workflows.iter().filter(|path| **path != self.entry).map(|path| (workflow_name(path), path.as_str()))
    }
}

/// The name of the workflow stored at `path`: its file name without the extension.
pub fn workflow_name(path: &str) -> &str {
    let file = path.rsplit('/').next().unwrap_or(path);
    file.rsplit_once('.').map_or(file, |(stem, _)| stem)
}

/// The budgets of a bundle's runs. Cost is in the price unit of the
/// providers file (`prices` per model), in millionths. Unknown fields are
/// refused: a budget a runtime does not know must not be ignored, so a new
/// one comes with a new format version.
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
