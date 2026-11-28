//! Opening and inspecting bundles.

use std::collections::BTreeMap;
use std::path::Path;

use graphrt_core::{Registry, Workflow};
use serde::Serialize;

use crate::archive::{ArchiveLimits, check_extractable, read_zip, validate_path};
use crate::error::BundleError;
use crate::manifest::{BUNDLE_FORMAT_VERSION, BundleManifest, MANIFEST_PATH, SIGNATURE_PATH};
use crate::sha256_hex;
use crate::sign::{BundleSignature, parse_public_key, verify_manifest};

/// Default [`OpenOptions::max_knowledge_bytes`]: 4 GiB.
pub const DEFAULT_MAX_KNOWLEDGE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Options for [`Bundle::open`].
#[derive(Clone, Debug)]
pub struct OpenOptions {
    /// Maximum archive entries.
    pub max_entries: usize,
    /// Maximum size of one file.
    pub max_file_bytes: u64,
    /// Maximum total uncompressed size, files under `knowledge/` not counted.
    pub max_total_bytes: u64,
    /// Maximum uncompressed size of the knowledge base files (`knowledge/`): one
    /// file and all of them together. Passage stores grow with the documents, so
    /// they get their own, larger limit; the compression-ratio guard still applies.
    pub max_knowledge_bytes: u64,
    /// Maximum per-file compression ratio (zip-bomb guard).
    pub max_compression_ratio: u64,
    /// Hex Ed25519 public keys allowed to sign. When non-empty, a valid signature
    /// from one of these keys is required.
    pub trusted_keys: Vec<String>,
    /// Require a valid signature even when `trusted_keys` is empty.
    pub require_signature: bool,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            max_entries: 2_000,
            max_file_bytes: 64 * 1024 * 1024,
            max_total_bytes: 256 * 1024 * 1024,
            max_knowledge_bytes: DEFAULT_MAX_KNOWLEDGE_BYTES,
            max_compression_ratio: 200,
            trusted_keys: Vec::new(),
            require_signature: false,
        }
    }
}

impl OpenOptions {
    /// Require a signature from `public_key` (hex). May be called repeatedly.
    pub fn trust_key(mut self, public_key: impl Into<String>) -> Self {
        self.trusted_keys.push(public_key.into());
        self
    }
}

/// A plugin shipped in the bundle.
#[derive(Clone, Debug)]
pub struct BundledPlugin {
    pub name: String,
    pub version: String,
    /// `wasm`, `process` or `http`.
    pub runtime: String,
    /// The plugin manifest (`plugins/<name>/graphrt-plugin.json`).
    pub manifest: serde_json::Value,
    /// Module bytes for WebAssembly plugins.
    pub module: Option<Vec<u8>>,
    /// Directory of the plugin inside the bundle (`plugins/<name>`).
    pub dir: String,
}

impl BundledPlugin {
    /// Whether the plugin runs on every host (WebAssembly only).
    pub fn is_portable(&self) -> bool {
        self.runtime == "wasm"
    }
}

/// Result of [`Bundle::check`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CompatReport {
    /// Node kinds the workflows use that the host registry lacks.
    pub missing_node_kinds: Vec<String>,
    /// Plugins the host cannot load portably (process / http plugins).
    pub non_portable_plugins: Vec<String>,
    /// Model providers the workflows reference (the host must configure them).
    pub providers: Vec<String>,
    /// Environment variables the workflows declare (the host must supply them).
    pub env: Vec<String>,
    /// Set when the bundle's runtime requirement does not match this runtime.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_mismatch: Option<String>,
}

impl CompatReport {
    /// Whether the bundle can run on this host (env and providers are only advisory).
    pub fn is_compatible(&self) -> bool {
        self.missing_node_kinds.is_empty() && self.version_mismatch.is_none()
    }
}

/// An opened, fully verified bundle held in memory.
#[derive(Clone, Debug)]
pub struct Bundle {
    manifest: BundleManifest,
    manifest_bytes: Vec<u8>,
    files: BTreeMap<String, Vec<u8>>,
    signer: Option<String>,
}

impl Bundle {
    /// Open a bundle from archive bytes: enforce archive limits, check every file
    /// against its SHA-256, reject unlisted files, and verify the signature.
    pub fn open(bytes: &[u8], options: &OpenOptions) -> Result<Self, BundleError> {
        let limits = ArchiveLimits {
            max_entries: options.max_entries,
            max_entry_bytes: options.max_file_bytes,
            max_total_bytes: options.max_total_bytes,
            max_knowledge_bytes: options.max_knowledge_bytes,
            max_ratio: options.max_compression_ratio,
        };
        let mut files = read_zip(bytes, &limits)?;
        let manifest_bytes =
            files.remove(MANIFEST_PATH).ok_or_else(|| BundleError::Manifest(format!("missing `{MANIFEST_PATH}`")))?;
        let manifest: BundleManifest =
            serde_json::from_slice(&manifest_bytes).map_err(|e| BundleError::Manifest(e.to_string()))?;
        if manifest.format_version == 0 || manifest.format_version > BUNDLE_FORMAT_VERSION {
            return Err(BundleError::Manifest(format!(
                "unsupported format version {} (this runtime reads {BUNDLE_FORMAT_VERSION})",
                manifest.format_version
            )));
        }

        // Signature.
        let signer = match files.remove(SIGNATURE_PATH) {
            Some(raw) => {
                let sig: BundleSignature = serde_json::from_slice(&raw)
                    .map_err(|e| BundleError::Signature(format!("malformed `{SIGNATURE_PATH}`: {e}")))?;
                Some(verify_manifest(&manifest_bytes, &sig)?)
            }
            None => None,
        };
        if !options.trusted_keys.is_empty() {
            let trusted: Vec<String> = options
                .trusted_keys
                .iter()
                .map(|k| parse_public_key(k).map(|key| crate::hex(key.as_bytes())))
                .collect::<Result<_, _>>()?;
            match &signer {
                None => {
                    return Err(BundleError::Signature(
                        "bundle is unsigned but a trusted signature is required".into(),
                    ));
                }
                Some(key) if !trusted.contains(key) => {
                    return Err(BundleError::Signature(format!("signed by untrusted key {key}")));
                }
                Some(_) => {}
            }
        } else if options.require_signature && signer.is_none() {
            return Err(BundleError::Signature("bundle is unsigned but a signature is required".into()));
        }

        // Integrity: exact set of files, each matching its hash.
        for path in manifest.files.keys() {
            validate_path(path)?;
        }
        for (path, data) in &files {
            let Some(expected) = manifest.files.get(path) else {
                return Err(BundleError::Integrity(format!("`{path}` is not listed in the manifest")));
            };
            if !expected.eq_ignore_ascii_case(&sha256_hex(data)) {
                return Err(BundleError::Integrity(format!("`{path}` does not match its recorded SHA-256")));
            }
        }
        if let Some(missing) = manifest.files.keys().find(|p| !files.contains_key(*p)) {
            return Err(BundleError::Integrity(format!("`{missing}` is listed in the manifest but missing")));
        }
        if !manifest.workflows.contains(&manifest.entry) {
            return Err(BundleError::Manifest(format!("entry `{}` is not one of the workflows", manifest.entry)));
        }
        if let Some(w) = manifest.workflows.iter().find(|w| !files.contains_key(*w)) {
            return Err(BundleError::Manifest(format!("workflow `{w}` is missing")));
        }
        Ok(Self { manifest, manifest_bytes, files, signer })
    }

    /// Read and open a bundle file.
    pub fn open_path(path: impl AsRef<Path>, options: &OpenOptions) -> Result<Self, BundleError> {
        let path = path.as_ref();
        let meta = std::fs::metadata(path).map_err(|e| BundleError::io(path, &e))?;
        if meta.len() > options.max_total_bytes {
            return Err(BundleError::Archive(format!("file is larger than {} bytes", options.max_total_bytes)));
        }
        let bytes = std::fs::read(path).map_err(|e| BundleError::io(path, &e))?;
        Self::open(&bytes, options)
    }

    pub fn manifest(&self) -> &BundleManifest {
        &self.manifest
    }

    /// Exact manifest bytes (what signatures cover).
    pub fn manifest_bytes(&self) -> &[u8] {
        &self.manifest_bytes
    }

    /// Hex public key of the verified signer, if the bundle is signed.
    pub fn signer(&self) -> Option<&str> {
        self.signer.as_deref()
    }

    /// Content of one file.
    pub fn file(&self, path: &str) -> Option<&[u8]> {
        self.files.get(path).map(Vec::as_slice)
    }

    /// Paths of every file (excluding manifest and signature).
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    pub(crate) fn raw_files(&self) -> &BTreeMap<String, Vec<u8>> {
        &self.files
    }

    /// Source text of a workflow (`None` = the entry workflow).
    pub fn workflow_text(&self, name: Option<&str>) -> Result<&str, BundleError> {
        let path = self.workflow_path(name)?;
        let bytes = self.files.get(path).ok_or_else(|| BundleError::Manifest(format!("workflow `{path}` missing")))?;
        std::str::from_utf8(bytes)
            .map_err(|_| BundleError::Workflow { name: path.to_owned(), message: "not valid UTF-8".into() })
    }

    /// Parse a workflow (`None` = the entry workflow). `name` may be the stored path
    /// or its file stem (`support` for `workflows/support.yaml`).
    pub fn workflow(&self, name: Option<&str>) -> Result<Workflow, BundleError> {
        let path = self.workflow_path(name)?.to_owned();
        let text = self.workflow_text(Some(&path))?;
        Workflow::parse(text).map_err(|e| BundleError::Workflow { name: path, message: e.to_string() })
    }

    fn workflow_path(&self, name: Option<&str>) -> Result<&str, BundleError> {
        let Some(name) = name else {
            return Ok(&self.manifest.entry);
        };
        self.manifest
            .workflows
            .iter()
            .find(|w| w.as_str() == name || stem(w) == name)
            .map(String::as_str)
            .ok_or_else(|| BundleError::Workflow { name: name.to_owned(), message: "not in this bundle".into() })
    }

    /// Every plugin shipped under `plugins/<name>/`.
    pub fn plugins(&self) -> Result<Vec<BundledPlugin>, BundleError> {
        let mut out = Vec::new();
        for (path, data) in &self.files {
            let Some(rest) = path.strip_prefix("plugins/") else { continue };
            let Some((dir_name, file)) = rest.split_once('/') else { continue };
            if file != "graphrt-plugin.json" {
                continue;
            }
            let plugin_err = |m: String| BundleError::Plugin { plugin: dir_name.to_owned(), message: m };
            let manifest: serde_json::Value =
                serde_json::from_slice(data).map_err(|e| plugin_err(format!("invalid manifest: {e}")))?;
            let name = manifest.get("name").and_then(|v| v.as_str()).unwrap_or(dir_name).to_owned();
            let version = manifest.get("version").and_then(|v| v.as_str()).unwrap_or_default().to_owned();
            let runtime = manifest.get("runtime").and_then(|v| v.as_str()).unwrap_or("process").to_owned();
            let dir = format!("plugins/{dir_name}");
            let module = if runtime == "wasm" {
                let module_path = manifest
                    .get("module")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| plugin_err("wasm plugin without `module`".into()))?;
                let full = format!("{dir}/{module_path}");
                validate_path(&full)?;
                Some(
                    self.files
                        .get(&full)
                        .cloned()
                        .ok_or_else(|| plugin_err(format!("module `{module_path}` is missing")))?,
                )
            } else {
                None
            };
            out.push(BundledPlugin { name, version, runtime, manifest, module, dir });
        }
        Ok(out)
    }

    /// WebAssembly plugins only (portable to every host, including phones).
    pub fn wasm_plugins(&self) -> Result<Vec<BundledPlugin>, BundleError> {
        Ok(self.plugins()?.into_iter().filter(BundledPlugin::is_portable).collect())
    }

    /// Path and text of the knowledge-base config (`knowledge/knowledge.{yaml,yml,json}`).
    pub fn knowledge_config(&self) -> Option<(&str, &str)> {
        ["knowledge/knowledge.yaml", "knowledge/knowledge.yml", "knowledge/knowledge.json"]
            .iter()
            .find_map(|p| self.files.get(*p).and_then(|b| std::str::from_utf8(b).ok()).map(|t| (*p, t)))
    }

    /// Path and text of the providers file (`providers.{yaml,yml,json}`), which never holds secrets.
    pub fn providers_config(&self) -> Option<(&str, &str)> {
        ["providers.yaml", "providers.yml", "providers.json"]
            .iter()
            .find_map(|p| self.files.get(*p).and_then(|b| std::str::from_utf8(b).ok()).map(|t| (*p, t)))
    }

    /// The knowledge-base config as JSON with every store `path` inside `knowledge/`
    /// rewritten to its location under `extracted` (the directory passed to
    /// [`Bundle::extract_to`]). `None` when the bundle has no knowledge config.
    pub fn knowledge_config_at(&self, extracted: &Path) -> Option<Result<serde_json::Value, BundleError>> {
        let (path, text) = self.knowledge_config()?;
        let parsed: Result<serde_json::Value, BundleError> = if path.ends_with(".json") {
            serde_json::from_str(text).map_err(|e| BundleError::Manifest(format!("{path}: {e}")))
        } else {
            graphrt_core::yaml::to_value(text, true).map_err(|e| BundleError::Manifest(format!("{path}: {e}")))
        };
        Some(parsed.map(|mut value| {
            if let Some(stores) = value.get_mut("stores").and_then(|s| s.as_object_mut()) {
                for store in stores.values_mut() {
                    let Some(obj) = store.as_object_mut() else { continue };
                    let rel = obj.get("path").and_then(|p| p.as_str()).map(str::to_owned);
                    if let Some(rel) = rel.filter(|r| r.starts_with("knowledge/") && validate_path(r).is_ok()) {
                        let full = extracted.join(rel).display().to_string();
                        obj.insert("path".into(), serde_json::Value::String(full));
                    }
                }
            }
            value
        }))
    }

    /// Write every file under `dir` (created if needed), preserving the layout.
    /// Paths were validated on open, so nothing escapes `dir`; before writing
    /// anything, paths that are Windows device names or differ only in case
    /// are refused, so every verified file lands as its own file everywhere.
    ///
    /// @throws BundleError::Archive - For a device name or a case collision.
    /// @throws BundleError::Io - When a file cannot be written.
    pub fn extract_to(&self, dir: &Path) -> Result<(), BundleError> {
        check_extractable(self.files.keys().map(String::as_str))?;
        for (path, data) in &self.files {
            let target = dir.join(path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| BundleError::io(parent, &e))?;
            }
            std::fs::write(&target, data).map_err(|e| BundleError::io(&target, &e))?;
        }
        Ok(())
    }

    /// Compare the bundle's requirements with a host registry (after bundled
    /// plugins were registered) and this runtime's version.
    pub fn check(&self, registry: &Registry) -> CompatReport {
        let req = &self.manifest.requires;
        let missing_node_kinds = req.node_kinds.iter().filter(|k| !registry.contains(k)).cloned().collect();
        let non_portable_plugins = req.plugins.iter().filter(|p| !p.portable).map(|p| p.name.clone()).collect();
        let version_mismatch = version_mismatch(&req.graphrt, env!("CARGO_PKG_VERSION"));
        CompatReport {
            missing_node_kinds,
            non_portable_plugins,
            providers: req.providers.clone(),
            env: req.env.clone(),
            version_mismatch,
        }
    }
}

fn stem(path: &str) -> &str {
    crate::manifest::workflow_name(path)
}

fn version_mismatch(requirement: &str, current: &str) -> Option<String> {
    if requirement.trim().is_empty() {
        return None;
    }
    let req = match semver::VersionReq::parse(requirement) {
        Ok(r) => r,
        Err(e) => return Some(format!("invalid runtime requirement `{requirement}`: {e}")),
    };
    match semver::Version::parse(current) {
        Ok(v) if req.matches(&v) => None,
        Ok(v) => Some(format!("bundle requires graphrt {requirement}, this is {v}")),
        Err(e) => Some(format!("cannot parse runtime version `{current}`: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_requirements() {
        assert!(version_mismatch(">=0.1", "0.1.0").is_none());
        assert!(version_mismatch(">=0.2", "0.1.0").is_some());
        assert!(version_mismatch("", "0.1.0").is_none());
        assert!(version_mismatch("nonsense!", "0.1.0").is_some());
        assert_eq!(stem("workflows/support.yaml"), "support");
    }
}
