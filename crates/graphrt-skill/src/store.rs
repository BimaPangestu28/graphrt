//! The skill store: installed skills on disk, pinned by content hash.
//!
//! ```text
//! <root>/
//!   index.json            one entry per installed skill
//!   skills/<name>/...     the installed copy of each skill
//! ```
//!
//! The index is the source of truth for everything decided at install time:
//! the name and description shown to models, the SHA-256 of every file, what
//! the scan found, and the tools the owner granted. Grants live here and
//! never in the package, so a skill cannot grant itself anything.
//!
//! Files are verified against their pinned hash every time they are read. A
//! file that changed after the install is refused ([`Error::Tampered`]) until
//! the skill is installed again, which is when a person looks at it.
//!
//! Opening a store does no I/O. The index is read on first use and kept in
//! memory until the file changes. Several processes may read one store. Use
//! one writer at a time: concurrent installs from different processes are not
//! coordinated.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use graphrt_core::private_fs;
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::manifest::{SKILL_FILE, is_valid_name, parse_skill_file};
use crate::package::{SkillFile, SkillPackage, read_package, sha256_hex};
use crate::scan::Finding;

/// Environment variable naming the store directory.
pub const SKILLS_DIR_ENV: &str = "GRAPHRT_SKILLS_DIR";
/// Version of the index format this store reads and writes.
pub const INDEX_SCHEMA: u32 = 1;

const INDEX_FILE: &str = "index.json";
const SKILLS_DIR: &str = "skills";
/// Largest index file read, so a damaged or hostile file cannot exhaust memory.
const MAX_INDEX_BYTES: u64 = 16 * 1024 * 1024;

/// Where an installed skill came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginKind {
    /// A directory on this machine.
    #[default]
    Local,
    /// A published catalog.
    Catalog,
    /// Written here, for example from a plain language description.
    Authored,
    /// Proposed from what an agent learned, then approved.
    Learned,
}

/// The origin of an installed skill.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillOrigin {
    /// The kind of source.
    pub kind: OriginKind,
    /// Where exactly, for a person: a path, a catalog entry, an agent name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Who published it, when the source states that.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    /// Whether the host verified a signature covering this content before installing.
    #[serde(default)]
    pub verified: bool,
}

/// How calls of a granted tool are handled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantMode {
    /// Every call waits for approval.
    #[default]
    Ask,
    /// Calls run without asking.
    Allow,
}

/// A tool the owner granted to a skill.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// `provider/tool` or `provider/*`, exactly as the skill asks for it.
    pub tool: String,
    /// Whether calls ask first.
    #[serde(default)]
    pub mode: GrantMode,
}

impl Grant {
    /// A grant whose calls ask for approval first.
    pub fn ask(tool: impl Into<String>) -> Self {
        Self { tool: tool.into(), mode: GrantMode::Ask }
    }

    /// A grant whose calls run without asking.
    pub fn allow(tool: impl Into<String>) -> Self {
        Self { tool: tool.into(), mode: GrantMode::Allow }
    }
}

/// One installed skill.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillEntry {
    /// The skill's name.
    pub name: String,
    /// What it does and when to use it.
    pub description: String,
    /// Where it came from.
    #[serde(default)]
    pub origin: SkillOrigin,
    /// When it was installed, in milliseconds since the Unix epoch.
    #[serde(default)]
    pub installed_at_ms: u64,
    /// Whether agents may use it.
    pub enabled: bool,
    /// Identity of the installed content (see [`SkillPackage::digest`]).
    pub digest: String,
    /// Every installed file with its pinned hash.
    pub files: Vec<SkillFile>,
    /// Tools the skill asks for that Graphrt can grant.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requested_tools: Vec<String>,
    /// Tool requests in another client's syntax.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unsupported_tools: Vec<String>,
    /// Tools the owner granted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<Grant>,
    /// What the scan found at install time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
    /// Whether the skill ships scripts (which are never run).
    #[serde(default)]
    pub has_scripts: bool,
    /// The skill's license, as it states it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// Environment requirements, as the skill states them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<String>,
    /// `metadata.version` of the skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// `metadata.author` of the skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
}

impl SkillEntry {
    /// The entry `package` would get, turned off and without grants.
    fn from_package(package: &SkillPackage) -> Self {
        let manifest = &package.parsed.manifest;
        Self {
            name: manifest.name.clone(),
            description: manifest.description.clone(),
            origin: SkillOrigin::default(),
            installed_at_ms: 0,
            enabled: false,
            digest: package.digest.clone(),
            files: package.files.clone(),
            requested_tools: package.requested_tools.clone(),
            unsupported_tools: package.unsupported_tools.clone(),
            grants: Vec::new(),
            findings: package.findings.clone(),
            has_scripts: package.has_scripts,
            license: manifest.license.clone(),
            compatibility: manifest.compatibility.clone(),
            version: manifest.metadata.get("version").cloned(),
            author: manifest.metadata.get("author").cloned(),
        }
    }

    /// Requested tools that are not granted.
    pub fn ungranted_tools(&self) -> Vec<&str> {
        self.requested_tools
            .iter()
            .filter(|tool| !self.grants.iter().any(|grant| &grant.tool == *tool))
            .map(String::as_str)
            .collect()
    }

    /// Paths of the files besides `SKILL.md`.
    pub fn resource_paths(&self) -> impl Iterator<Item = &str> {
        self.files.iter().map(|file| file.path.as_str()).filter(|path| *path != SKILL_FILE)
    }
}

/// The store's index file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillIndex {
    /// Format version, see [`INDEX_SCHEMA`].
    pub schema: u32,
    /// Installed skills by name.
    #[serde(default)]
    pub skills: BTreeMap<String, SkillEntry>,
}

/// How an install differs from the installed skill of the same name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SkillChanges {
    /// Tools the new version asks for that the installed one did not.
    pub added_tools: Vec<String>,
    /// Tools the installed version asked for that the new one does not.
    pub removed_tools: Vec<String>,
    /// Files whose content differs.
    pub changed_files: Vec<String>,
    /// Files only in the new version.
    pub added_files: Vec<String>,
    /// Files only in the installed version.
    pub removed_files: Vec<String>,
}

impl SkillChanges {
    fn between(installed: &SkillEntry, new: &SkillEntry) -> Self {
        let only_in = |a: &[String], b: &[String]| a.iter().filter(|tool| !b.contains(tool)).cloned().collect();
        let hash_of = |entry: &SkillEntry, path: &str| {
            entry.files.iter().find(|file| file.path == path).map(|file| file.sha256.clone())
        };
        let mut changes = Self {
            added_tools: only_in(&new.requested_tools, &installed.requested_tools),
            removed_tools: only_in(&installed.requested_tools, &new.requested_tools),
            ..Self::default()
        };
        for file in &new.files {
            match hash_of(installed, &file.path) {
                None => changes.added_files.push(file.path.clone()),
                Some(hash) if hash != file.sha256 => changes.changed_files.push(file.path.clone()),
                Some(_) => {}
            }
        }
        for file in &installed.files {
            if hash_of(new, &file.path).is_none() {
                changes.removed_files.push(file.path.clone());
            }
        }
        changes
    }

    /// Whether the two versions are identical.
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// A skill as a person reviews it, before or after installing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SkillDetails {
    /// The installed entry, or the entry an install would create.
    pub entry: SkillEntry,
    /// Whether `entry` is installed in this store.
    pub installed: bool,
    /// Whether an install needs `accept_findings` because the scan found something blocking.
    pub blocked: bool,
    /// For a directory that would replace an installed skill: what changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changes: Option<SkillChanges>,
}

/// Choices made when installing a skill.
#[derive(Clone, Debug, Default)]
pub struct InstallOptions {
    /// Tools to grant. Each must be one the skill asks for.
    pub grants: Vec<Grant>,
    /// Install although the scan found something blocking.
    pub accept_findings: bool,
    /// Replace an installed skill of the same name. Nothing is carried over:
    /// the new version gets exactly `grants`.
    pub replace: bool,
    /// Where the skill comes from.
    pub origin: SkillOrigin,
}

/// The instructions of a skill, ready to hand to a model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedSkill {
    /// The skill's name.
    pub name: String,
    /// The Markdown body of `SKILL.md`.
    pub body: String,
    /// Paths of the skill's other files.
    pub resources: Vec<String>,
    /// Tools granted to the skill.
    pub granted_tools: Vec<String>,
}

#[derive(Debug)]
struct CachedIndex {
    modified: Option<SystemTime>,
    bytes: u64,
    index: Arc<SkillIndex>,
}

/// Installed skills in one directory.
#[derive(Debug)]
pub struct SkillStore {
    root: PathBuf,
    cache: Mutex<Option<CachedIndex>>,
    /// Serializes changes made through this handle.
    write_lock: Mutex<()>,
}

impl SkillStore {
    /// A store in `root`. Nothing is read or created until the store is used.
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), cache: Mutex::new(None), write_lock: Mutex::new(()) }
    }

    /// The default store directory: `$GRAPHRT_SKILLS_DIR`, else `~/.graphrt/skills`.
    pub fn default_root() -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os(SKILLS_DIR_ENV).filter(|dir| !dir.is_empty()) {
            return Some(PathBuf::from(dir));
        }
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
        Some(PathBuf::from(home).join(".graphrt").join("skills"))
    }

    /// The store directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Every installed skill, by name.
    ///
    /// # Errors
    /// [`Error::Io`] or [`Error::Index`] when the index cannot be read.
    pub fn list(&self) -> Result<Vec<SkillEntry>, Error> {
        Ok(self.index()?.skills.values().cloned().collect())
    }

    /// The installed skill `name`.
    ///
    /// # Errors
    /// [`Error::NotFound`] when it is not installed.
    pub fn get(&self, name: &str) -> Result<SkillEntry, Error> {
        self.index()?.skills.get(name).cloned().ok_or_else(|| Error::NotFound(name.to_owned()))
    }

    /// The installed skill `name`, for review.
    ///
    /// # Errors
    /// [`Error::NotFound`] when it is not installed.
    pub fn inspect(&self, name: &str) -> Result<SkillDetails, Error> {
        let entry = self.get(name)?;
        Ok(SkillDetails { entry, installed: true, blocked: false, changes: None })
    }

    /// The skill in `dir`, as it would be installed: its description, the
    /// tools it asks for, its files and what the scan found. Nothing is
    /// written.
    ///
    /// # Errors
    /// The errors of [`read_package`].
    pub fn inspect_dir(&self, dir: &Path) -> Result<SkillDetails, Error> {
        let package = read_package(dir)?;
        let entry = SkillEntry::from_package(&package);
        let installed = self.index()?.skills.get(&entry.name).cloned();
        let changes = installed.as_ref().map(|installed| SkillChanges::between(installed, &entry));
        let blocked = entry.findings.iter().any(Finding::is_blocking);
        Ok(SkillDetails { entry, installed: false, blocked, changes })
    }

    /// Install the skill in `dir`.
    ///
    /// The files are copied into the store and pinned by hash. The installed
    /// skill is turned on and has exactly the grants in `options`.
    ///
    /// # Errors
    /// - the errors of [`read_package`];
    /// - [`Error::Blocked`] when the scan found something blocking and
    ///   `accept_findings` is not set;
    /// - [`Error::GrantNotRequested`] for a grant the skill does not ask for;
    /// - [`Error::AlreadyInstalled`] without `replace`;
    /// - [`Error::Io`] when the store cannot be written.
    pub fn install(&self, dir: &Path, options: InstallOptions) -> Result<SkillEntry, Error> {
        let package = read_package(dir)?;
        self.install_package(&package, options)
    }

    /// Install an already read package. See [`SkillStore::install`].
    ///
    /// # Errors
    /// As [`SkillStore::install`], without the read errors.
    pub fn install_package(&self, package: &SkillPackage, options: InstallOptions) -> Result<SkillEntry, Error> {
        let mut entry = SkillEntry::from_package(package);
        if !is_valid_name(&entry.name) {
            return Err(Error::Format(format!("the name `{}` is not valid", entry.name)));
        }
        if !options.accept_findings && entry.findings.iter().any(Finding::is_blocking) {
            return Err(Error::Blocked { skill: entry.name, findings: entry.findings });
        }
        check_grants(&entry, &options.grants)?;
        let _guard = self.write_lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut index = self.read_index()?;
        if index.skills.contains_key(&entry.name) && !options.replace {
            return Err(Error::AlreadyInstalled(entry.name));
        }
        entry.grants = options.grants;
        entry.origin = options.origin;
        entry.enabled = true;
        entry.installed_at_ms = now_ms();
        self.write_files(&entry.name, package)?;
        index.skills.insert(entry.name.clone(), entry.clone());
        self.write_index(&index)?;
        Ok(entry)
    }

    /// Remove an installed skill and its files.
    ///
    /// # Errors
    /// [`Error::NotFound`] when it is not installed, [`Error::Io`] on failure.
    pub fn remove(&self, name: &str) -> Result<(), Error> {
        let _guard = self.write_lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut index = self.read_index()?;
        if index.skills.remove(name).is_none() {
            return Err(Error::NotFound(name.to_owned()));
        }
        // The index goes first: a skill that is no longer listed is never served,
        // even if removing its files fails afterwards.
        self.write_index(&index)?;
        let dir = self.skill_dir(name);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::io(dir, &error)),
        }
    }

    /// Turn a skill on or off. A skill that is off is not offered to any agent.
    ///
    /// # Errors
    /// [`Error::NotFound`] when it is not installed.
    pub fn set_enabled(&self, name: &str, enabled: bool) -> Result<SkillEntry, Error> {
        self.update(name, |entry| {
            entry.enabled = enabled;
            Ok(())
        })
    }

    /// Replace the grants of a skill.
    ///
    /// # Errors
    /// [`Error::NotFound`], or [`Error::GrantNotRequested`] for a tool the
    /// skill does not ask for.
    pub fn set_grants(&self, name: &str, grants: Vec<Grant>) -> Result<SkillEntry, Error> {
        self.update(name, |entry| {
            check_grants(entry, &grants)?;
            entry.grants = grants;
            Ok(())
        })
    }

    /// The instructions of skill `name`, after checking that `SKILL.md` still
    /// has its pinned hash.
    ///
    /// # Errors
    /// [`Error::NotFound`], [`Error::Disabled`], or [`Error::Tampered`] when
    /// the file changed since the install.
    pub fn load(&self, name: &str) -> Result<LoadedSkill, Error> {
        let entry = self.enabled_entry(name)?;
        let text = self.read_pinned_text(&entry, SKILL_FILE)?;
        let parsed = parse_skill_file(&text)?;
        Ok(LoadedSkill {
            name: entry.name.clone(),
            body: parsed.body,
            resources: entry.resource_paths().map(str::to_owned).collect(),
            granted_tools: entry.grants.iter().map(|grant| grant.tool.clone()).collect(),
        })
    }

    /// The text of one file of skill `name`, after checking its pinned hash.
    ///
    /// `path` must be one of the installed files, exactly as listed. It is
    /// looked up in the index and never joined to a directory as given, so a
    /// path such as `../other/SKILL.md` simply does not exist.
    ///
    /// # Errors
    /// [`Error::NotFound`], [`Error::Disabled`], [`Error::NoSuchFile`],
    /// [`Error::Tampered`] or [`Error::NotText`].
    pub fn read_file(&self, name: &str, path: &str) -> Result<String, Error> {
        let entry = self.enabled_entry(name)?;
        self.read_pinned_text(&entry, path)
    }

    /// The current index, from memory when the file did not change.
    pub(crate) fn index(&self) -> Result<Arc<SkillIndex>, Error> {
        let path = self.index_path();
        let stamp = match std::fs::metadata(&path) {
            Ok(metadata) => Some((metadata.modified().ok(), metadata.len())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(Error::io(path, &error)),
        };
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        let Some((modified, bytes)) = stamp else {
            *cache = None;
            return Ok(Arc::new(SkillIndex { schema: INDEX_SCHEMA, skills: BTreeMap::new() }));
        };
        if let Some(cached) = cache.as_ref()
            && cached.modified == modified
            && cached.bytes == bytes
            && modified.is_some()
        {
            return Ok(Arc::clone(&cached.index));
        }
        let index = Arc::new(self.read_index()?);
        *cache = Some(CachedIndex { modified, bytes, index: Arc::clone(&index) });
        Ok(index)
    }

    fn enabled_entry(&self, name: &str) -> Result<SkillEntry, Error> {
        let entry = self.get(name)?;
        if !entry.enabled {
            return Err(Error::Disabled(name.to_owned()));
        }
        Ok(entry)
    }

    fn update(
        &self,
        name: &str,
        change: impl FnOnce(&mut SkillEntry) -> Result<(), Error>,
    ) -> Result<SkillEntry, Error> {
        let _guard = self.write_lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut index = self.read_index()?;
        let entry = index.skills.get_mut(name).ok_or_else(|| Error::NotFound(name.to_owned()))?;
        change(entry)?;
        let updated = entry.clone();
        self.write_index(&index)?;
        Ok(updated)
    }

    fn index_path(&self) -> PathBuf {
        self.root.join(INDEX_FILE)
    }

    fn skill_dir(&self, name: &str) -> PathBuf {
        self.root.join(SKILLS_DIR).join(name)
    }

    fn read_index(&self) -> Result<SkillIndex, Error> {
        let path = self.index_path();
        let bytes = match std::fs::metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SkillIndex { schema: INDEX_SCHEMA, skills: BTreeMap::new() });
            }
            Err(error) => return Err(Error::io(path, &error)),
            Ok(metadata) if metadata.len() > MAX_INDEX_BYTES => {
                return Err(Error::Index(format!("{} is larger than {MAX_INDEX_BYTES} bytes", path.display())));
            }
            Ok(_) => std::fs::read(&path).map_err(|e| Error::io(&path, &e))?,
        };
        let index: SkillIndex = serde_json::from_slice(&bytes).map_err(|e| Error::Index(e.to_string()))?;
        if index.schema != INDEX_SCHEMA {
            return Err(Error::Index(format!(
                "index format {} is not supported by this version (expected {INDEX_SCHEMA})",
                index.schema
            )));
        }
        // Names become directory names: an index edited by hand must not point elsewhere.
        if let Some(bad) = index.skills.iter().find(|(key, entry)| !is_valid_name(key) || entry.name != **key) {
            return Err(Error::Index(format!("entry `{}` has an invalid name", crate::text::shorten(bad.0, 80))));
        }
        Ok(index)
    }

    /// Write the index to a new file and rename it over the old one, so a
    /// reader sees either the old index or the new one.
    fn write_index(&self, index: &SkillIndex) -> Result<(), Error> {
        private_fs::create_dir_all(&self.root).map_err(|e| Error::io(&self.root, &e))?;
        let text = serde_json::to_vec_pretty(index).map_err(|e| Error::Index(e.to_string()))?;
        let temporary = self.root.join(format!("{INDEX_FILE}.{}", unique_suffix()));
        private_fs::write(&temporary, &text).map_err(|e| Error::io(&temporary, &e))?;
        let path = self.index_path();
        std::fs::rename(&temporary, &path).map_err(|error| {
            let _ = std::fs::remove_file(&temporary);
            Error::io(path, &error)
        })?;
        *self.cache.lock().unwrap_or_else(PoisonError::into_inner) = None;
        Ok(())
    }

    /// Write the files of `package` into a staging directory, then put it in
    /// place of any installed copy.
    fn write_files(&self, name: &str, package: &SkillPackage) -> Result<(), Error> {
        let skills_dir = self.root.join(SKILLS_DIR);
        private_fs::create_dir_all(&skills_dir).map_err(|e| Error::io(&skills_dir, &e))?;
        let staging = skills_dir.join(format!(".staging-{name}-{}", unique_suffix()));
        let result = self.stage(&staging, package).and_then(|()| {
            let target = self.skill_dir(name);
            match std::fs::remove_dir_all(&target) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(Error::io(&target, &error)),
            }
            std::fs::rename(&staging, &target).map_err(|e| Error::io(&target, &e))
        });
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&staging);
        }
        result
    }

    fn stage(&self, staging: &Path, package: &SkillPackage) -> Result<(), Error> {
        private_fs::create_dir_all(staging).map_err(|e| Error::io(staging, &e))?;
        for (file, content) in package.files.iter().zip(&package.contents) {
            let path = join_relative(staging, &file.path);
            if let Some(parent) = path.parent() {
                private_fs::create_dir_all(parent).map_err(|e| Error::io(parent, &e))?;
            }
            private_fs::write(&path, content).map_err(|e| Error::io(&path, &e))?;
        }
        Ok(())
    }

    /// Read `path` of an installed skill and check it against its pinned hash.
    fn read_pinned_text(&self, entry: &SkillEntry, path: &str) -> Result<String, Error> {
        let pinned = entry
            .files
            .iter()
            .find(|file| file.path == path)
            .ok_or_else(|| Error::NoSuchFile { skill: entry.name.clone(), file: crate::text::shorten(path, 200) })?;
        let tampered = || Error::Tampered { skill: entry.name.clone(), file: pinned.path.clone() };
        // Paths were checked at install. An index edited by hand must not lead out of the skill.
        let plain = |part: &str| !part.is_empty() && part != "." && part != ".." && !part.contains(['\\', ':']);
        if !pinned.path.split('/').all(plain) {
            return Err(tampered());
        }
        let full_path = join_relative(&self.skill_dir(&entry.name), &pinned.path);
        // A link swapped in for the file would be followed by a plain read.
        let metadata = std::fs::symlink_metadata(&full_path).map_err(|_| tampered())?;
        if !metadata.file_type().is_file() || metadata.len() != pinned.bytes {
            return Err(tampered());
        }
        let bytes = std::fs::read(&full_path).map_err(|_| tampered())?;
        if sha256_hex(&bytes) != pinned.sha256 {
            return Err(tampered());
        }
        String::from_utf8(bytes).map_err(|_| Error::NotText { skill: entry.name.clone(), file: pinned.path.clone() })
    }
}

fn check_grants(entry: &SkillEntry, grants: &[Grant]) -> Result<(), Error> {
    match grants.iter().find(|grant| !entry.requested_tools.contains(&grant.tool)) {
        Some(extra) => Err(Error::GrantNotRequested { skill: entry.name.clone(), tool: extra.tool.clone() }),
        None => Ok(()),
    }
}

/// `base` plus a validated relative path with `/` separators.
fn join_relative(base: &Path, relative: &str) -> PathBuf {
    relative.split('/').fold(base.to_path_buf(), |path, part| path.join(part))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

fn unique_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or_default();
    format!("{}-{}-{nanos}.tmp", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed))
}
