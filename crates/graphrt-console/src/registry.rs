//! The agent registry: every version of each served workflow, which version
//! each environment runs, and the history of what was promoted and rolled back.
//!
//! A version is the exact file uploaded (a workflow YAML or JSON, or a `.grt`
//! bundle), kept once by its SHA-256 in `registry/blobs/`. Versions are
//! numbered per workflow (1, 2, 3...). An environment runs one version of a
//! workflow at a time: uploading can deploy to the first environment,
//! promoting copies the version of one environment to the next, rolling back
//! goes back to the version an environment ran before.
//!
//! A server serves the versions of its own environment (`environment` in the
//! config): what changed takes effect when it restarts, which the console can
//! do itself (see [`crate::Host::restart`]). Servers of other environments
//! that share the directory pick their versions up the same way.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::is_name;
use crate::error::{ConsoleError, Result};
use crate::util::{JsonFile, ensure_dir, now_ms, sha256_hex};

/// History entries kept per environment and workflow.
const MAX_HISTORY: usize = 100;
/// Versions kept per workflow.
pub const MAX_VERSIONS: usize = 200;

/// The kind of file of a version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Yaml,
    Json,
    /// A portable bundle (`.grt`).
    Bundle,
}

impl Format {
    /// The file extension the version is served with.
    pub fn extension(self) -> &'static str {
        match self {
            Format::Yaml => "yaml",
            Format::Json => "json",
            Format::Bundle => "grt",
        }
    }

    /// The format of a file name or extension (`flow.yaml`, `grt`).
    pub fn from_name(name: &str) -> Option<Self> {
        let extension = name.rsplit('.').next().unwrap_or(name).to_ascii_lowercase();
        match extension.as_str() {
            "yaml" | "yml" => Some(Format::Yaml),
            "json" => Some(Format::Json),
            "grt" | "bundle" => Some(Format::Bundle),
            _ => None,
        }
    }

    /// The format of uploaded bytes: a zip archive is a bundle, `{` JSON, else YAML.
    pub fn sniff(bytes: &[u8]) -> Self {
        if bytes.starts_with(b"PK\x03\x04") {
            Format::Bundle
        } else if bytes.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'{') {
            Format::Json
        } else {
            Format::Yaml
        }
    }
}

/// One uploaded version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    pub version: u32,
    /// SHA-256 of the file, hex.
    pub hash: String,
    pub format: Format,
    pub size: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    pub uploaded_ms: u64,
    pub uploaded_by: String,
}

/// One change of the version an environment runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deployment {
    pub version: u32,
    /// `deploy`, `promote` or `rollback`.
    pub action: String,
    /// The environment a promoted version came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub at_ms: u64,
    pub by: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct WorkflowEntry {
    versions: Vec<Version>,
    /// Newest last; the last entry is what the environment runs.
    #[serde(default)]
    environments: BTreeMap<String, Vec<Deployment>>,
}

impl WorkflowEntry {
    fn version(&self, number: u32) -> Option<&Version> {
        self.versions.iter().find(|v| v.version == number)
    }

    fn active(&self, environment: &str) -> Option<&Version> {
        let deployed = self.environments.get(environment)?.last()?;
        self.version(deployed.version)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct RegistryFile {
    #[serde(default)]
    workflows: BTreeMap<String, WorkflowEntry>,
}

/// A workflow of the registry as listed.
#[derive(Clone, Debug, Serialize)]
pub struct WorkflowView {
    pub name: String,
    pub versions: Vec<Version>,
    /// The version each environment runs and how it got there, newest first.
    pub environments: BTreeMap<String, EnvironmentView>,
}

/// What one environment runs.
#[derive(Clone, Debug, Serialize)]
pub struct EnvironmentView {
    pub active: Option<u32>,
    pub history: Vec<Deployment>,
}

/// The registry in `<dir>/registry`.
#[derive(Debug)]
pub struct Registry {
    blobs: PathBuf,
    file: JsonFile<RegistryFile>,
    environments: Vec<String>,
}

impl Registry {
    /// The registry in `<data_dir>/registry`, with the environments of the config.
    ///
    /// @throws ConsoleError::Io - When the directory cannot be created or the file read.
    pub fn open(data_dir: &Path, environments: Vec<String>) -> Result<Self> {
        let dir = data_dir.join("registry");
        let blobs = dir.join("blobs");
        ensure_dir(&blobs)?;
        Ok(Self { blobs, file: JsonFile::open(dir.join("registry.json"))?, environments })
    }

    /// The environments, in promotion order.
    pub fn environments(&self) -> &[String] {
        &self.environments
    }

    fn check_environment(&self, environment: &str) -> Result<()> {
        if self.environments.iter().any(|e| e == environment) {
            Ok(())
        } else {
            Err(ConsoleError::Invalid(format!(
                "unknown environment `{environment}` (one of {})",
                self.environments.join(", ")
            )))
        }
    }

    /// The file of a version.
    pub fn blob_path(&self, version: &Version) -> PathBuf {
        self.blobs.join(format!("{}.{}", version.hash, version.format.extension()))
    }

    /// Keep a new version of `name`. The same file again is the version it already is.
    ///
    /// @param deploy_to - An environment that runs it from now on.
    /// @returns The version, and whether it is new.
    /// @throws ConsoleError - `Invalid` name or environment, `Conflict` when too
    ///   many versions are kept, `Io` on a failed write.
    pub fn add(
        &self,
        name: &str,
        format: Format,
        bytes: &[u8],
        note: &str,
        by: &str,
        deploy_to: Option<&str>,
    ) -> Result<(Version, bool)> {
        check_workflow_name(name)?;
        if let Some(environment) = deploy_to {
            self.check_environment(environment)?;
        }
        let hash = sha256_hex(bytes);
        let candidate = Version {
            version: 0,
            hash,
            format,
            size: bytes.len() as u64,
            note: note.chars().take(500).collect(),
            uploaded_ms: now_ms(),
            uploaded_by: by.to_owned(),
        };
        let blob = self.blob_path(&candidate);
        if !blob.exists() {
            crate::util::replace_file(&blob, bytes)?;
        }
        self.file.update(|registry| {
            let entry = registry.workflows.entry(name.to_owned()).or_default();
            let existing = entry.versions.iter().find(|v| v.hash == candidate.hash && v.format == format).cloned();
            let (version, new) = match existing {
                Some(existing) => (existing, false),
                None => {
                    if entry.versions.len() >= MAX_VERSIONS {
                        return Err(ConsoleError::Conflict(format!(
                            "workflow `{name}` has {MAX_VERSIONS} versions, the most kept"
                        )));
                    }
                    let number = entry.versions.iter().map(|v| v.version).max().unwrap_or(0) + 1;
                    let version = Version { version: number, ..candidate.clone() };
                    entry.versions.push(version.clone());
                    (version, true)
                }
            };
            if let Some(environment) = deploy_to {
                deploy(entry, environment, version.version, "deploy", None, by);
            }
            Ok((version, new))
        })
    }

    /// Run `version` of `name` in `environment`.
    ///
    /// @throws ConsoleError - `NotFound` for an unknown workflow or version, `Invalid` environment.
    pub fn deploy(&self, name: &str, version: u32, environment: &str, by: &str) -> Result<Version> {
        self.check_environment(environment)?;
        self.file.update(|registry| {
            let entry = workflow_mut(registry, name)?;
            let found = entry.version(version).cloned().ok_or_else(|| unknown_version(name, version))?;
            deploy(entry, environment, version, "deploy", None, by);
            Ok(found)
        })
    }

    /// Run in `to` the version `from` runs. Without `to`, the environment
    /// after `from` in the promotion order.
    ///
    /// @returns The version, and the environment it went to.
    /// @throws ConsoleError - `Invalid` environments, `Conflict` when `from` runs nothing.
    pub fn promote(&self, name: &str, from: &str, to: Option<&str>, by: &str) -> Result<(Version, String)> {
        self.check_environment(from)?;
        let to = match to {
            Some(to) => to.to_owned(),
            None => {
                let position = self.environments.iter().position(|e| e == from).unwrap_or(0);
                self.environments
                    .get(position + 1)
                    .cloned()
                    .ok_or_else(|| ConsoleError::Invalid(format!("`{from}` is the last environment")))?
            }
        };
        self.check_environment(&to)?;
        if to == from {
            return Err(ConsoleError::Invalid("a version is promoted to another environment".into()));
        }
        self.file.update(|registry| {
            let entry = workflow_mut(registry, name)?;
            let version = entry
                .active(from)
                .cloned()
                .ok_or_else(|| ConsoleError::Conflict(format!("`{from}` runs no version of `{name}`")))?;
            deploy(entry, &to, version.version, "promote", Some(from), by);
            Ok((version, to.clone()))
        })
    }

    /// Go back to the version `environment` ran before the current one (or to `version`).
    ///
    /// @throws ConsoleError - `Conflict` when there is nothing to go back to.
    pub fn rollback(&self, name: &str, environment: &str, version: Option<u32>, by: &str) -> Result<Version> {
        self.check_environment(environment)?;
        self.file.update(|registry| {
            let entry = workflow_mut(registry, name)?;
            let history = entry.environments.get(environment).cloned().unwrap_or_default();
            let current = history.last().map(|d| d.version);
            let target = match version {
                Some(version) => version,
                None => history.iter().rev().map(|d| d.version).find(|v| Some(*v) != current).ok_or_else(|| {
                    ConsoleError::Conflict(format!("`{environment}` ran no earlier version of `{name}`"))
                })?,
            };
            let found = entry.version(target).cloned().ok_or_else(|| unknown_version(name, target))?;
            if Some(target) == current {
                return Err(ConsoleError::Conflict(format!("`{environment}` runs version {target} already")));
            }
            deploy(entry, environment, target, "rollback", None, by);
            Ok(found)
        })
    }

    /// The version `environment` runs of `name`.
    ///
    /// @throws ConsoleError::Io - When the file changed and cannot be read.
    pub fn active(&self, name: &str, environment: &str) -> Result<Option<Version>> {
        self.file.read(|registry| registry.workflows.get(name).and_then(|entry| entry.active(environment)).cloned())
    }

    /// The files `environment` serves: one per workflow that has a version there.
    ///
    /// @throws ConsoleError - `Io` when the registry cannot be read, `NotFound` when a file is missing.
    pub fn served_files(&self, environment: &str) -> Result<Vec<(String, Version, PathBuf)>> {
        let active = self.file.read(|registry| {
            registry
                .workflows
                .iter()
                .filter_map(|(name, entry)| entry.active(environment).map(|v| (name.clone(), v.clone())))
                .collect::<Vec<_>>()
        })?;
        active
            .into_iter()
            .map(|(name, version)| {
                let path = self.blob_path(&version);
                if !path.is_file() {
                    return Err(ConsoleError::NotFound(format!(
                        "version {} of `{name}` is missing from the registry ({})",
                        version.version,
                        path.display()
                    )));
                }
                Ok((name, version, path))
            })
            .collect()
    }

    /// Every workflow of the registry.
    ///
    /// @throws ConsoleError::Io - When the file changed and cannot be read.
    pub fn list(&self) -> Result<Vec<WorkflowView>> {
        self.file.read(|registry| {
            registry
                .workflows
                .iter()
                .map(|(name, entry)| WorkflowView {
                    name: name.clone(),
                    versions: entry.versions.iter().rev().cloned().collect(),
                    environments: self
                        .environments
                        .iter()
                        .map(|environment| {
                            let history = entry.environments.get(environment).cloned().unwrap_or_default();
                            let view = EnvironmentView {
                                active: history.last().map(|d| d.version),
                                history: history.into_iter().rev().collect(),
                            };
                            (environment.clone(), view)
                        })
                        .collect(),
                })
                .collect()
        })
    }
}

fn deploy(entry: &mut WorkflowEntry, environment: &str, version: u32, action: &str, from: Option<&str>, by: &str) {
    let history = entry.environments.entry(environment.to_owned()).or_default();
    history.push(Deployment {
        version,
        action: action.to_owned(),
        from: from.map(str::to_owned),
        at_ms: now_ms(),
        by: by.to_owned(),
    });
    if history.len() > MAX_HISTORY {
        let excess = history.len() - MAX_HISTORY;
        history.drain(..excess);
    }
}

fn workflow_mut<'r>(registry: &'r mut RegistryFile, name: &str) -> Result<&'r mut WorkflowEntry> {
    registry
        .workflows
        .get_mut(name)
        .ok_or_else(|| ConsoleError::NotFound(format!("workflow `{name}` has no version in the registry")))
}

fn unknown_version(name: &str, version: u32) -> ConsoleError {
    ConsoleError::NotFound(format!("workflow `{name}` has no version {version}"))
}

/// A workflow name: what `POST /v1/workflows/{name}/runs` names.
///
/// @throws ConsoleError::Invalid - For a name with other characters.
pub fn check_workflow_name(name: &str) -> Result<()> {
    if is_name(name) {
        Ok(())
    } else {
        Err(ConsoleError::Invalid(format!("`{name}` is not a workflow name (letters, digits, `-`, `_` and `.`)")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(name: &str) -> (PathBuf, Registry) {
        let dir = graphrt_core::private_fs::create_fresh_temp_dir(name).unwrap();
        let registry = Registry::open(&dir, vec!["dev".into(), "staging".into(), "prod".into()]).unwrap();
        (dir, registry)
    }

    #[test]
    fn versions_are_numbered_and_kept_once() {
        let (dir, registry) = registry("graphrt-console-registry-add");
        let (one, new) = registry.add("support", Format::Yaml, b"a: 1", "first", "alice", Some("dev")).unwrap();
        assert!(new && one.version == 1);
        let (again, new) = registry.add("support", Format::Yaml, b"a: 1", "", "bob", None).unwrap();
        assert!(!new && again.version == 1);
        let (two, _) = registry.add("support", Format::Yaml, b"a: 2", "", "bob", None).unwrap();
        assert_eq!(two.version, 2);
        assert_eq!(std::fs::read(registry.blob_path(&two)).unwrap(), b"a: 2");
        assert_eq!(registry.active("support", "dev").unwrap().map(|v| v.version), Some(1));
        assert!(registry.add("bad name", Format::Yaml, b"", "", "a", None).is_err());
        assert!(registry.add("x", Format::Yaml, b"", "", "a", Some("qa")).is_err());
        let _cleanup = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn promotion_follows_the_order_and_rollback_goes_back() {
        let (dir, registry) = registry("graphrt-console-registry-promote");
        registry.add("support", Format::Yaml, b"v1", "", "a", Some("dev")).unwrap();
        let (version, to) = registry.promote("support", "dev", None, "a").unwrap();
        assert_eq!((version.version, to.as_str()), (1, "staging"));
        registry.promote("support", "staging", Some("prod"), "a").unwrap();
        registry.add("support", Format::Yaml, b"v2", "", "a", Some("dev")).unwrap();
        registry.promote("support", "dev", Some("prod"), "a").unwrap();
        assert_eq!(registry.active("support", "prod").unwrap().map(|v| v.version), Some(2));
        let back = registry.rollback("support", "prod", None, "b").unwrap();
        assert_eq!(back.version, 1);
        assert_eq!(registry.active("support", "prod").unwrap().map(|v| v.version), Some(1));
        let forward = registry.rollback("support", "prod", None, "b").unwrap();
        assert_eq!(forward.version, 2, "a second rollback undoes the first");
        assert!(matches!(registry.rollback("support", "prod", Some(2), "b"), Err(ConsoleError::Conflict(_))));
        assert!(matches!(registry.promote("support", "prod", None, "a"), Err(ConsoleError::Invalid(_))));
        assert!(matches!(registry.promote("other", "dev", None, "a"), Err(ConsoleError::NotFound(_))));
        let view = registry.list().unwrap();
        let prod = &view[0].environments["prod"];
        assert_eq!(prod.active, Some(2));
        assert_eq!(prod.history[0].action, "rollback");
        let served = registry.served_files("prod").unwrap();
        assert_eq!(std::fs::read(&served[0].2).unwrap(), b"v2");
        let _cleanup = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn formats_are_recognised() {
        assert_eq!(Format::sniff(b"PK\x03\x04rest"), Format::Bundle);
        assert_eq!(Format::sniff(b"  {\"graph\": {}}"), Format::Json);
        assert_eq!(Format::sniff(b"graph:\n"), Format::Yaml);
        assert_eq!(Format::from_name("flow.YML"), Some(Format::Yaml));
        assert_eq!(Format::from_name("app.grt"), Some(Format::Bundle));
        assert_eq!(Format::from_name("x.txt"), None);
    }
}
