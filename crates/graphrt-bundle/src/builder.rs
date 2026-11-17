//! Building bundles.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use graphrt_core::Workflow;

use crate::archive::{validate_path, write_zip};
use crate::error::BundleError;
use crate::manifest::{BUNDLE_FORMAT_VERSION, BundleManifest, MANIFEST_PATH, PluginRef, Requires, SIGNATURE_PATH};
use crate::secrets::check_no_inline_secrets;
use crate::sha256_hex;

/// Directories never copied from plugin or asset folders.
const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", "__pycache__", ".venv"];
/// Largest single file accepted by the builder.
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Assembles a bundle.
#[derive(Clone, Debug, Default)]
pub struct BundleBuilder {
    name: String,
    version: String,
    description: String,
    created_at: Option<String>,
    workflows: Vec<String>,
    files: BTreeMap<String, Vec<u8>>,
    plugins: Vec<PluginRef>,
}

impl BundleBuilder {
    /// New bundle named `name` at `version`.
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self { name: name.into(), version: version.into(), ..Self::default() }
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Fixed creation time (RFC 3339) for reproducible builds. Defaults to
    /// `$SOURCE_DATE_EPOCH` when set, else the current time.
    pub fn created_at(mut self, created_at: impl Into<String>) -> Self {
        self.created_at = Some(created_at.into());
        self
    }

    /// Add a workflow document stored at `path` (e.g. `workflow.yaml`). The first
    /// workflow added is the entry. The document must parse.
    pub fn workflow(mut self, path: &str, text: impl Into<String>) -> Result<Self, BundleError> {
        let text = text.into();
        validate_path(path)?;
        reserved(path)?;
        Workflow::parse(&text).map_err(|e| BundleError::Workflow { name: path.to_owned(), message: e.to_string() })?;
        if self.workflows.iter().any(|w| w == path) {
            return Err(BundleError::Build(format!("workflow `{path}` added twice")));
        }
        self.workflows.push(path.to_owned());
        self.files.insert(path.to_owned(), text.into_bytes());
        Ok(self)
    }

    /// Add an arbitrary file (assets, data) at `path`.
    pub fn file(mut self, path: &str, bytes: Vec<u8>) -> Result<Self, BundleError> {
        validate_path(path)?;
        reserved(path)?;
        if self.files.insert(path.to_owned(), bytes).is_some() {
            return Err(BundleError::Build(format!("`{path}` added twice")));
        }
        Ok(self)
    }

    /// Copy a directory under `prefix/` (e.g. `assets`), skipping build output and VCS folders.
    pub fn include_dir(mut self, dir: &Path, prefix: &str) -> Result<Self, BundleError> {
        for (rel, bytes) in read_tree(dir)? {
            self = self.file(&format!("{prefix}/{rel}"), bytes)?;
        }
        Ok(self)
    }

    /// Add a plugin directory (with `graphrt-plugin.json` or `.yaml`) under
    /// `plugins/<name>/`. The manifest is stored as JSON. WebAssembly plugins are
    /// portable; process and http plugins are kept but flagged non-portable.
    pub fn plugin_dir(mut self, dir: &Path) -> Result<Self, BundleError> {
        let manifest = read_plugin_manifest(dir)?;
        let name = manifest
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|n| !n.is_empty() && !n.contains('/') && !n.contains(".."))
            .ok_or_else(|| BundleError::Build(format!("{}: plugin manifest needs a simple `name`", dir.display())))?
            .to_owned();
        let version = manifest.get("version").and_then(|v| v.as_str()).unwrap_or_default().to_owned();
        let runtime = manifest.get("runtime").and_then(|v| v.as_str()).unwrap_or("process").to_owned();
        if self.plugins.iter().any(|p| p.name == name) {
            return Err(BundleError::Build(format!("plugin `{name}` added twice")));
        }
        check_no_inline_secrets(&manifest, &format!("plugin `{name}` manifest"))?;
        let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| BundleError::Build(e.to_string()))?;
        let prefix = format!("plugins/{name}");
        for (rel, bytes) in read_tree(dir)? {
            if rel.starts_with("graphrt-plugin.") {
                continue;
            }
            self = self.file(&format!("{prefix}/{rel}"), bytes)?;
        }
        if runtime == "wasm" {
            let module = manifest.get("module").and_then(|v| v.as_str()).unwrap_or_default();
            if !self.files.contains_key(&format!("{prefix}/{module}")) {
                return Err(BundleError::Build(format!("plugin `{name}`: module `{module}` not found")));
            }
        }
        self.plugins.push(PluginRef {
            name: name.clone(),
            version,
            portable: runtime == "wasm",
            runtime,
            sha256: sha256_hex(&manifest_bytes),
        });
        self.files.insert(format!("{prefix}/graphrt-plugin.json"), manifest_bytes);
        Ok(self)
    }

    /// Add a bare, self-describing WebAssembly module as plugin `name` (a manifest is
    /// generated; its node kinds may be plain names).
    pub fn wasm_module(mut self, name: &str, module: Vec<u8>) -> Result<Self, BundleError> {
        if name.is_empty() || name.contains('/') || name.contains("..") {
            return Err(BundleError::Build(format!("invalid plugin name `{name}`")));
        }
        if self.plugins.iter().any(|p| p.name == name) {
            return Err(BundleError::Build(format!("plugin `{name}` added twice")));
        }
        let manifest = serde_json::json!({
            "name": name,
            "version": "",
            "runtime": "wasm",
            "module": "module.wasm",
            "allow_unprefixed_kinds": true,
        });
        let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| BundleError::Build(e.to_string()))?;
        self = self.file(&format!("plugins/{name}/module.wasm"), module)?;
        self.plugins.push(PluginRef {
            name: name.to_owned(),
            version: String::new(),
            runtime: "wasm".into(),
            portable: true,
            sha256: sha256_hex(&manifest_bytes),
        });
        self.files.insert(format!("plugins/{name}/graphrt-plugin.json"), manifest_bytes);
        Ok(self)
    }

    /// Add a model providers file (YAML or JSON). Files with inline secrets are rejected.
    pub fn providers(mut self, text: &str) -> Result<Self, BundleError> {
        let value = parse_structured(text, "providers file")?;
        check_no_inline_secrets(&value, "providers file")?;
        let bytes = serde_json::to_vec_pretty(&value).map_err(|e| BundleError::Build(e.to_string()))?;
        self.files.insert("providers.json".into(), bytes);
        Ok(self)
    }

    /// Add a knowledge-base config (YAML or JSON). Memory stores whose `path` exists
    /// under `base_dir` are copied into `knowledge/` and the path rewritten, so a
    /// prebuilt index ships with the bundle. Inline secrets are rejected.
    pub fn knowledge(mut self, text: &str, base_dir: &Path) -> Result<Self, BundleError> {
        let mut value = parse_structured(text, "knowledge config")?;
        check_no_inline_secrets(&value, "knowledge config")?;
        if let Some(stores) = value.get_mut("stores").and_then(|s| s.as_object_mut()) {
            for (store_name, store) in stores.iter_mut() {
                let Some(path) = store.get("path").and_then(|p| p.as_str()).map(str::to_owned) else { continue };
                let source = base_dir.join(&path);
                if !source.is_file() {
                    continue;
                }
                let bytes = read_file(&source)?;
                let file_name = source.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
                let file_name = file_name.trim_start_matches('.');
                let target = format!("knowledge/{store_name}-{file_name}");
                if let Some(obj) = store.as_object_mut() {
                    obj.insert("path".into(), serde_json::Value::String(target.clone()));
                }
                self = self.file(&target, bytes)?;
            }
        }
        let bytes = serde_json::to_vec_pretty(&value).map_err(|e| BundleError::Build(e.to_string()))?;
        self.files.insert("knowledge/knowledge.json".into(), bytes);
        Ok(self)
    }

    /// Finish: compute requirements and hashes and produce the archive bytes.
    pub fn build(self) -> Result<Vec<u8>, BundleError> {
        let entry = self.workflows.first().cloned().ok_or_else(|| BundleError::Build("no workflow added".into()))?;
        let mut node_kinds = BTreeSet::new();
        let mut providers = BTreeSet::new();
        let mut env = BTreeSet::new();
        for path in &self.workflows {
            let text = self
                .files
                .get(path)
                .and_then(|b| std::str::from_utf8(b).ok())
                .ok_or_else(|| BundleError::Build(format!("workflow `{path}` is not UTF-8")))?;
            let wf = Workflow::parse(text)
                .map_err(|e| BundleError::Workflow { name: path.clone(), message: e.to_string() })?;
            for decl in &wf.env {
                env.insert(decl.name.clone());
            }
            for node in &wf.graph.nodes {
                node_kinds.insert(node.kind.clone());
                if let Some(provider) =
                    node.config.get("model").and_then(|m| m.get("provider")).and_then(|p| p.as_str())
                {
                    providers.insert(provider.to_owned());
                }
            }
        }
        let files: BTreeMap<String, String> = self.files.iter().map(|(p, b)| (p.clone(), sha256_hex(b))).collect();
        let manifest = BundleManifest {
            format_version: BUNDLE_FORMAT_VERSION,
            name: self.name,
            version: self.version,
            description: self.description,
            created_at: self.created_at.unwrap_or_else(now_rfc3339),
            entry,
            workflows: self.workflows,
            requires: Requires {
                graphrt: runtime_requirement(),
                node_kinds: node_kinds.into_iter().collect(),
                plugins: self.plugins,
                providers: providers.into_iter().collect(),
                env: env.into_iter().collect(),
            },
            files,
        };
        if manifest.name.trim().is_empty() {
            return Err(BundleError::Build("bundle `name` must not be empty".into()));
        }
        let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| BundleError::Build(e.to_string()))?;
        write_zip(&[(MANIFEST_PATH, &manifest_bytes)], &self.files)
    }
}

fn reserved(path: &str) -> Result<(), BundleError> {
    if path == MANIFEST_PATH || path == SIGNATURE_PATH {
        return Err(BundleError::Build(format!("`{path}` is reserved")));
    }
    Ok(())
}

/// `>=major.minor` of this runtime.
fn runtime_requirement() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let mut parts = version.split('.');
    match (parts.next(), parts.next()) {
        (Some(major), Some(minor)) => format!(">={major}.{minor}"),
        _ => String::new(),
    }
}

fn parse_structured(text: &str, what: &str) -> Result<serde_json::Value, BundleError> {
    if text.trim_start().starts_with(['{', '[']) {
        return serde_json::from_str(text).map_err(|e| BundleError::Build(format!("invalid {what}: {e}")));
    }
    let options = serde_saphyr::options! { strict_booleans: true };
    serde_saphyr::from_str_with_options(text, options).map_err(|e| BundleError::Build(format!("invalid {what}: {e}")))
}

fn read_plugin_manifest(dir: &Path) -> Result<serde_json::Value, BundleError> {
    for name in ["graphrt-plugin.json", "graphrt-plugin.yaml", "graphrt-plugin.yml"] {
        let path = dir.join(name);
        if path.is_file() {
            let bytes = read_file(&path)?;
            let text =
                String::from_utf8(bytes).map_err(|_| BundleError::Build(format!("{}: not UTF-8", path.display())))?;
            return parse_structured(&text, &format!("plugin manifest {}", path.display()));
        }
    }
    Err(BundleError::Build(format!("{}: no graphrt-plugin.json or graphrt-plugin.yaml", dir.display())))
}

fn read_file(path: &Path) -> Result<Vec<u8>, BundleError> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| BundleError::io(path, &e))?;
    if !meta.is_file() {
        return Err(BundleError::Build(format!("{}: not a regular file (symlinks are not bundled)", path.display())));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(BundleError::Build(format!("{}: larger than {MAX_FILE_BYTES} bytes", path.display())));
    }
    std::fs::read(path).map_err(|e| BundleError::io(path, &e))
}

/// Regular files under `dir` as (`/`-separated relative path, bytes), sorted.
/// Symlinks and build / VCS folders are skipped.
fn read_tree(dir: &Path) -> Result<Vec<(String, Vec<u8>)>, BundleError> {
    let mut out = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), String::new())];
    while let Some((path, rel)) = stack.pop() {
        let entries = std::fs::read_dir(&path).map_err(|e| BundleError::io(&path, &e))?;
        for entry in entries {
            let entry = entry.map_err(|e| BundleError::io(&path, &e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let child_rel = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            let file_type = entry.file_type().map_err(|e| BundleError::io(&entry.path(), &e))?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) {
                    stack.push((entry.path(), child_rel));
                }
            } else if file_type.is_file() {
                out.push((child_rel, read_file(&entry.path())?));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Current time as RFC 3339 UTC, or `$SOURCE_DATE_EPOCH` for reproducible builds.
fn now_rfc3339() -> String {
    let secs = std::env::var("SOURCE_DATE_EPOCH").ok().and_then(|s| s.parse::<i64>().ok()).unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_secs()).unwrap_or(0))
            .unwrap_or(0)
    });
    rfc3339(secs)
}

/// Format Unix seconds as `YYYY-MM-DDTHH:MM:SSZ` (proleptic Gregorian calendar).
pub(crate) fn rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rem / 3_600, (rem % 3_600) / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_formatting() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_795_392_000), "2026-11-23T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn requirement_is_major_minor() {
        assert!(runtime_requirement().starts_with(">="));
    }
}
