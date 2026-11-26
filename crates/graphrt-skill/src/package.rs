//! Reading a skill directory: every file once, with limits.
//!
//! A package is read in a single pass. The bytes that are hashed and scanned
//! are the bytes that get installed, so a file cannot change between the
//! check and the copy.
//!
//! What is refused: symbolic links and anything that is not a regular file or
//! a directory, path components that are unsafe on any supported system
//! (`..`, separators, Windows device names, names that differ only in case),
//! and packages beyond [`PackageLimits`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::Error;
use crate::manifest::{ParsedSkill, SKILL_FILE, parse_skill_file};
use crate::scan::{Finding, Severity, scan_text};

/// File name of an Agent Plugins manifest.
pub const PLUGIN_MANIFEST_FILE: &str = "plugin.json";
/// Directory of an Agent Plugins package that holds its skills.
pub const PLUGIN_SKILLS_DIR: &str = "skills";
/// Directory of a skill that holds executable code. Graphrt never runs it.
pub const SCRIPTS_DIR: &str = "scripts";

/// Directory and file names that are never part of a skill.
const IGNORED_NAMES: &[&str] = &[".git", ".DS_Store", "Thumbs.db", "node_modules", "__pycache__"];
/// Names Windows reserves for devices, with or without an extension.
const WINDOWS_DEVICE_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9", "lpt1", "lpt2",
    "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];
const MAX_COMPONENT_BYTES: usize = 128;

/// Upper bounds for one skill package.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackageLimits {
    /// Most files, `SKILL.md` included.
    pub max_files: usize,
    /// Largest single file, in bytes.
    pub max_file_bytes: u64,
    /// Largest sum of all files, in bytes.
    pub max_total_bytes: u64,
    /// Deepest directory nesting below the skill directory.
    pub max_depth: usize,
}

impl Default for PackageLimits {
    fn default() -> Self {
        Self { max_files: 256, max_file_bytes: 1024 * 1024, max_total_bytes: 8 * 1024 * 1024, max_depth: 5 }
    }
}

/// One file of a skill, pinned by its content hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillFile {
    /// Path inside the skill, with `/` separators.
    pub path: String,
    /// Lowercase hex SHA-256 of the content.
    pub sha256: String,
    /// Size in bytes.
    pub bytes: u64,
}

/// A skill directory after it was read, checked and scanned.
#[derive(Clone, Debug)]
pub struct SkillPackage {
    /// The parsed `SKILL.md`.
    pub parsed: ParsedSkill,
    /// Every file, sorted by path.
    pub files: Vec<SkillFile>,
    /// Hash over the paths and hashes of `files`: the identity of this exact content.
    pub digest: String,
    /// `allowed-tools` entries Graphrt understands (`provider/tool` or `provider/*`).
    pub requested_tools: Vec<String>,
    /// `allowed-tools` entries in another client's syntax, which Graphrt cannot grant.
    pub unsupported_tools: Vec<String>,
    /// Whether the skill ships files under `scripts/`.
    pub has_scripts: bool,
    /// What the scan found.
    pub findings: Vec<Finding>,
    /// File contents, in the order of `files`.
    pub(crate) contents: Vec<Vec<u8>>,
}

/// What a directory offers to install.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discovered {
    /// Skill directories, each holding a `SKILL.md`.
    pub skills: Vec<PathBuf>,
    /// Set when the directory is an Agent Plugins package.
    pub plugin: Option<PluginPackage>,
}

/// The parts of an Agent Plugins package besides its skills.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginPackage {
    /// `name` of `plugin.json`, when it could be read.
    pub name: Option<String>,
    /// The package declares MCP servers (`mcp.json`). Graphrt does not install them from here.
    pub has_mcp_servers: bool,
    /// The package declares hooks (`hooks.json`). Graphrt does not run them.
    pub has_hooks: bool,
}

/// Find what `dir` offers: itself when it holds a `SKILL.md`, or the skills
/// of an Agent Plugins package (`plugin.json` plus `skills/<name>/SKILL.md`,
/// one level deep).
///
/// # Errors
/// [`Error::NotASkill`] when the directory is neither.
pub fn discover(dir: &Path) -> Result<Discovered, Error> {
    if is_regular_file(&dir.join(SKILL_FILE)) {
        return Ok(Discovered { skills: vec![dir.to_path_buf()], plugin: None });
    }
    let manifest_path = dir.join(PLUGIN_MANIFEST_FILE);
    let skills_dir = dir.join(PLUGIN_SKILLS_DIR);
    if !is_regular_file(&manifest_path) || !is_directory(&skills_dir) {
        return Err(Error::NotASkill(dir.to_path_buf()));
    }
    let entries = std::fs::read_dir(&skills_dir).map_err(|e| Error::io(&skills_dir, &e))?;
    let mut skills: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| is_directory(path) && is_regular_file(&path.join(SKILL_FILE)))
        .collect();
    skills.sort_unstable();
    if skills.is_empty() {
        return Err(Error::NotASkill(dir.to_path_buf()));
    }
    let name = std::fs::read(&manifest_path)
        .ok()
        .filter(|bytes| bytes.len() <= 64 * 1024)
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|manifest| manifest.get("name").and_then(serde_json::Value::as_str).map(str::to_owned));
    let plugin = PluginPackage {
        name,
        has_mcp_servers: is_regular_file(&dir.join("mcp.json")),
        has_hooks: is_regular_file(&dir.join("hooks.json")),
    };
    Ok(Discovered { skills, plugin: Some(plugin) })
}

/// Read, check and scan the skill in `dir` with the default limits.
///
/// # Errors
/// See [`read_package_with`].
pub fn read_package(dir: &Path) -> Result<SkillPackage, Error> {
    read_package_with(dir, PackageLimits::default())
}

/// Read, check and scan the skill in `dir`.
///
/// # Errors
/// [`Error::Format`] for a bad `SKILL.md`, [`Error::UnsafePath`] for a link or
/// an unsafe file name, [`Error::TooLarge`] beyond `limits`, [`Error::Io`]
/// when a file cannot be read.
pub fn read_package_with(dir: &Path, limits: PackageLimits) -> Result<SkillPackage, Error> {
    let mut reader = Reader { limits, files: Vec::new(), contents: Vec::new(), total_bytes: 0, seen: HashSet::new() };
    reader.walk(dir, "", 0)?;
    let mut order: Vec<usize> = (0..reader.files.len()).collect();
    order.sort_unstable_by(|a, b| reader.files.get(*a).map(|f| &f.path).cmp(&reader.files.get(*b).map(|f| &f.path)));
    let mut files = Vec::with_capacity(order.len());
    let mut contents = Vec::with_capacity(order.len());
    for index in order {
        if let (Some(file), Some(content)) = (reader.files.get(index), reader.contents.get_mut(index)) {
            files.push(file.clone());
            contents.push(std::mem::take(content));
        }
    }
    let entry = files
        .iter()
        .position(|file| file.path == SKILL_FILE)
        .ok_or_else(|| Error::Format(format!("{} has no {SKILL_FILE}", dir.display())))?;
    let skill_text = contents
        .get(entry)
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .ok_or_else(|| Error::Format(format!("{SKILL_FILE} is not UTF-8 text")))?;
    let parsed = parse_skill_file(skill_text)?;
    let (requested_tools, unsupported_tools) = split_tool_requests(&parsed.manifest.allowed_tools);
    let has_scripts = files.iter().any(|file| file.path.starts_with("scripts/"));
    let mut findings = Vec::new();
    for (file, content) in files.iter().zip(&contents) {
        if let Ok(text) = std::str::from_utf8(content) {
            scan_text(&file.path, text, &mut findings);
        }
    }
    if has_scripts {
        let count = files.iter().filter(|file| file.path.starts_with("scripts/")).count();
        findings.push(Finding::new(
            Severity::Info,
            "scripts_not_run",
            format!("Contains {count} script file(s). Graphrt does not run scripts from skills."),
        ));
    }
    if !unsupported_tools.is_empty() {
        findings.push(Finding::new(
            Severity::Info,
            "unsupported_tools",
            format!(
                "Asks for tools of another product that Graphrt cannot grant: {}.",
                crate::text::shorten(&unsupported_tools.join(", "), 300)
            ),
        ));
    }
    let digest = package_digest(&files);
    Ok(SkillPackage { parsed, files, digest, requested_tools, unsupported_tools, has_scripts, findings, contents })
}

/// Lowercase hex SHA-256 of `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing to a String cannot fail.
        let _ = write!(text, "{byte:02x}");
    }
    text
}

fn package_digest(files: &[SkillFile]) -> String {
    let mut hasher = Sha256::new();
    for file in files {
        hasher.update(file.sha256.as_bytes());
        hasher.update(b"  ");
        hasher.update(file.path.as_bytes());
        hasher.update(b"\n");
    }
    hex(&hasher.finalize())
}

/// Splits `allowed-tools` into entries Graphrt can grant and the rest.
///
/// A Graphrt tool reference is `provider/tool` or `provider/*`, made of
/// letters, digits and `_ . : -`. Entries such as `Bash(git:*)` or `Read`
/// belong to other clients.
pub fn split_tool_requests(entries: &[String]) -> (Vec<String>, Vec<String>) {
    let mut requested = Vec::new();
    let mut unsupported = Vec::new();
    for entry in entries {
        let target = if is_tool_reference(entry) { &mut requested } else { &mut unsupported };
        if !target.contains(entry) {
            target.push(entry.clone());
        }
    }
    (requested, unsupported)
}

/// Whether `entry` is `provider/tool` or `provider/*`.
pub fn is_tool_reference(entry: &str) -> bool {
    let Some((provider, tool)) = entry.split_once('/') else { return false };
    let plain = |part: &str| {
        !part.is_empty()
            && part.len() <= 128
            && part.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
    };
    plain(provider) && (tool == "*" || plain(tool))
}

fn is_regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

fn is_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
}

struct Reader {
    limits: PackageLimits,
    files: Vec<SkillFile>,
    contents: Vec<Vec<u8>>,
    total_bytes: u64,
    /// Lowercase paths seen so far, to refuse names that differ only in case.
    seen: HashSet<String>,
}

impl Reader {
    fn walk(&mut self, dir: &Path, prefix: &str, depth: usize) -> Result<(), Error> {
        if depth > self.limits.max_depth {
            return Err(Error::TooLarge(format!("directories nest deeper than {} levels", self.limits.max_depth)));
        }
        let entries = std::fs::read_dir(dir).map_err(|e| Error::io(dir, &e))?;
        let mut paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|entry| entry.path()).collect();
        paths.sort_unstable();
        for path in paths {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                return Err(Error::UnsafePath(format!("a file name under `{prefix}` is not valid Unicode")));
            };
            if IGNORED_NAMES.contains(&name) {
                continue;
            }
            let relative = if prefix.is_empty() { name.to_owned() } else { format!("{prefix}/{name}") };
            check_component(name, &relative)?;
            if !self.seen.insert(relative.to_lowercase()) {
                return Err(Error::UnsafePath(format!("`{relative}` differs from another file only in letter case")));
            }
            let metadata = std::fs::symlink_metadata(&path).map_err(|e| Error::io(&path, &e))?;
            let kind = metadata.file_type();
            if kind.is_dir() {
                self.walk(&path, &relative, depth + 1)?;
            } else if kind.is_file() {
                self.read_file(&path, relative, metadata.len())?;
            } else {
                return Err(Error::UnsafePath(format!("`{relative}` is a link or a special file")));
            }
        }
        Ok(())
    }

    fn read_file(&mut self, path: &Path, relative: String, reported_bytes: u64) -> Result<(), Error> {
        if self.files.len() >= self.limits.max_files {
            return Err(Error::TooLarge(format!("more than {} files", self.limits.max_files)));
        }
        if reported_bytes > self.limits.max_file_bytes {
            return Err(Error::TooLarge(format!("`{relative}` is larger than {} bytes", self.limits.max_file_bytes)));
        }
        let content = std::fs::read(path).map_err(|e| Error::io(path, &e))?;
        // The size is checked again on what was actually read.
        let bytes = content.len() as u64;
        if bytes > self.limits.max_file_bytes {
            return Err(Error::TooLarge(format!("`{relative}` is larger than {} bytes", self.limits.max_file_bytes)));
        }
        self.total_bytes = self.total_bytes.saturating_add(bytes);
        if self.total_bytes > self.limits.max_total_bytes {
            return Err(Error::TooLarge(format!(
                "the files add up to more than {} bytes",
                self.limits.max_total_bytes
            )));
        }
        self.files.push(SkillFile { path: relative, sha256: sha256_hex(&content), bytes });
        self.contents.push(content);
        Ok(())
    }
}

/// Refuse a file or directory name that is unsafe on Linux, macOS or Windows.
fn check_component(name: &str, relative: &str) -> Result<(), Error> {
    let unsafe_path = |why: &str| Error::UnsafePath(format!("`{}`: {why}", crate::text::shorten(relative, 200)));
    if name == "." || name == ".." {
        return Err(unsafe_path("relative path segments are not allowed"));
    }
    if name.len() > MAX_COMPONENT_BYTES {
        return Err(unsafe_path("the name is too long"));
    }
    if name.chars().any(|c| c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')) {
        return Err(unsafe_path("the name contains a character that is not allowed in file names"));
    }
    if name.ends_with('.') || name.ends_with(' ') || name.starts_with(' ') {
        return Err(unsafe_path("the name starts or ends with a space or ends with a dot"));
    }
    let stem = name.split('.').next().unwrap_or_default().to_ascii_lowercase();
    if WINDOWS_DEVICE_NAMES.contains(&stem.as_str()) {
        return Err(unsafe_path("the name is reserved on Windows"));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn tool_references_are_told_apart_from_other_clients_syntax() {
        let entries: Vec<String> = [
            "mail/send",
            "sheets/*",
            "mcp:github/create_issue",
            "Bash(git:*)",
            "Read",
            "a/b/c",
            "/x",
            "x/",
            "mail/send",
        ]
        .iter()
        .map(|entry| (*entry).to_owned())
        .collect();
        let (requested, unsupported) = split_tool_requests(&entries);
        assert_eq!(requested, vec!["mail/send", "sheets/*", "mcp:github/create_issue"]);
        assert_eq!(unsupported, vec!["Bash(git:*)", "Read", "a/b/c", "/x", "x/"]);
    }

    #[test]
    fn unsafe_names_are_refused() {
        for bad in ["..", ".", "a:b", "a\\b", "nul", "NUL.txt", "com1.md", "trailing.", "trailing ", " lead", "a\u{7}"]
        {
            assert!(check_component(bad, bad).is_err(), "{bad:?}");
        }
        for good in ["SKILL.md", "references", "price-list_v2.csv", ".hidden", "console.md", "número.md"] {
            assert!(check_component(good, good).is_ok(), "{good:?}");
        }
    }

    #[test]
    fn digest_depends_on_paths_and_contents() {
        let file = |path: &str, sha256: &str| SkillFile { path: path.into(), sha256: sha256.into(), bytes: 1 };
        let base = package_digest(&[file("SKILL.md", "aa"), file("a.md", "bb")]);
        assert_eq!(base, package_digest(&[file("SKILL.md", "aa"), file("a.md", "bb")]));
        assert_ne!(base, package_digest(&[file("SKILL.md", "aa"), file("a.md", "bc")]));
        assert_ne!(base, package_digest(&[file("SKILL.md", "aa"), file("b.md", "bb")]));
        assert_eq!(sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }
}
