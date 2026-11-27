//! Skills of the graphrt-plugins catalog: `graphrt skill install <name>[@version]`
//! and `graphrt skill search`.
//!
//! Skills are the `skills` list of the catalog's `index-v2.json`, next to the
//! plugins (format: `docs/skills.md` of Graphrt/graphrt-plugins). Fetching one:
//!
//! 1. reads `index-v2.json` and its minisign signature and checks the
//!    signature against the trusted catalog key **before the index is
//!    parsed**. There is no unsigned mode for skills: an index that does not
//!    verify is not used. A verified index must also not be older than the
//!    newest one already accepted;
//! 2. picks the latest version of the skill, or the exact one asked for;
//! 3. downloads the archive (https, or a local file when the index is local),
//!    capped in size and time, and checks its sha256 against the verified
//!    index;
//! 4. unpacks it into a private scratch directory, refusing unsafe entries,
//!    and expects the single top-level directory `<name>/` with a `SKILL.md`.
//!
//! What comes out is a directory. The caller installs it through the normal
//! skill install (validation, scan, grants), exactly like a local one.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use graphrt_skill::{OriginKind, SkillOrigin};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::archive::{ExtractLimits, extract_tar_gz};
use super::fetch::{FetchLimits, Location, fetch_bytes, parse_index_location, parse_tarball_location};
use super::freshness::SignedIndex;
use super::index::{PluginSpec, compare_versions, parse_plugin_spec, printable};
use super::trust::{IndexTrust, TrustOptions};
use crate::error::CliError;

/// File name of the index that lists skills, next to the plugins' `index.json`.
const SKILL_INDEX_FILE: &str = "index-v2.json";
const PLUGIN_INDEX_FILE: &str = "index.json";
/// The index schema that carries a `skills` list.
const SKILL_INDEX_SCHEMA: u64 = 2;
const INDEX_LIMITS: FetchLimits = FetchLimits { max_bytes: 4 * 1024 * 1024, timeout: Duration::from_secs(60) };
/// The catalog publishes skill archives of at most 8 MiB.
const MAX_ARCHIVE_BYTES: u64 = 8 * 1024 * 1024;
const ARCHIVE_TIMEOUT: Duration = Duration::from_secs(120);
/// What graphrt accepts for one skill (8 MiB, 256 files), with room for directories.
const EXTRACT_LIMITS: ExtractLimits = ExtractLimits { max_total_bytes: 8 * 1024 * 1024, max_entries: 1_024 };
const DESCRIPTION_WIDTH: usize = 72;

/// One published skill version.
#[derive(Clone, Debug, Deserialize)]
pub struct SkillCatalogEntry {
    pub name: String,
    pub version: String,
    /// The name people see.
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: String,
    /// Where the `skill-<name>-<version>.tar.gz` archive is downloaded from.
    pub tarball_url: String,
    /// Lowercase hex sha256 of the archive bytes.
    pub sha256: String,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    #[serde(default)]
    pub publisher: Option<String>,
}

impl SkillCatalogEntry {
    /// A valid skill name and a release version, so both print as plain text.
    fn is_well_formed(&self) -> bool {
        graphrt_skill::is_valid_name(&self.name) && parse_plugin_spec(&format!("x@{}", self.version)).is_some()
    }
}

#[derive(Debug, Deserialize)]
struct SkillIndex {
    schema: u64,
    #[serde(default)]
    generated_at: Option<serde_json::Value>,
    /// Absent in an index written before skills existed.
    #[serde(default)]
    skills: Vec<SkillCatalogEntry>,
}

/// Where the catalog is and which key it must be signed with.
#[derive(Clone, Copy, Debug, Default)]
pub struct SkillCatalogSource<'a> {
    /// `--index`, if given.
    pub index: Option<&'a str>,
    /// `--index-key`, if given.
    pub index_key: Option<&'a str>,
    /// Where the newest accepted index is remembered, instead of
    /// `~/.graphrt/plugin-catalog-state.json`.
    pub state_file: Option<&'a Path>,
}

/// The catalog a long running host installs skills from. It is fixed when the
/// host starts: a request never names an index or a key, so whoever may ask
/// for an install cannot choose who is trusted or which address is fetched.
#[derive(Clone, Debug, Default)]
pub struct SkillCatalogConfig {
    /// The index location, instead of `$GRAPHRT_PLUGIN_INDEX` and the default.
    pub index: Option<String>,
    /// The key the index must be signed with, instead of
    /// `$GRAPHRT_PLUGIN_INDEX_KEY` and the built-in catalog key.
    pub index_key: Option<String>,
    /// See [`SkillCatalogSource::state_file`].
    pub state_file: Option<PathBuf>,
}

impl SkillCatalogConfig {
    /// This configuration as the source of one fetch.
    pub fn source(&self) -> SkillCatalogSource<'_> {
        SkillCatalogSource {
            index: self.index.as_deref(),
            index_key: self.index_key.as_deref(),
            state_file: self.state_file.as_deref(),
        }
    }
}

/// Why a catalog skill was not fetched.
#[derive(Debug)]
pub enum CatalogSkillError {
    /// The verified catalog lists no such skill, or not in that version.
    NotListed(CliError),
    /// Anything else: the index location or key, a missing or wrong
    /// signature, a stale index, the download, the checksum, the archive.
    Refused(CliError),
}

impl From<CatalogSkillError> for CliError {
    fn from(error: CatalogSkillError) -> Self {
        match error {
            CatalogSkillError::NotListed(error) | CatalogSkillError::Refused(error) => error,
        }
    }
}

/// A catalog skill, downloaded, checked against the signed index and unpacked.
/// The directory is removed when this is dropped.
#[derive(Debug)]
pub struct FetchedSkill {
    pub entry: SkillCatalogEntry,
    /// The unpacked `<name>/` directory.
    pub dir: PathBuf,
    scratch: PathBuf,
}

impl Drop for FetchedSkill {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.scratch) {
            eprintln!("warning: cannot remove {}: {error}", self.scratch.display());
        }
    }
}

/// Fetch catalog skill `spec` (`name` or `name@version`).
///
/// # Errors
/// Usage errors (exit 2) for a bad index location and an unknown name or
/// version. Failures (exit 1) for a missing or wrong signature, a stale index,
/// and download, checksum and unpacking problems.
pub fn fetch_catalog_skill(
    spec: &PluginSpec,
    source: SkillCatalogSource<'_>,
) -> Result<FetchedSkill, CatalogSkillError> {
    let location = skill_index_location(source.index).map_err(CatalogSkillError::Refused)?;
    let index = load_verified_index(&location, source).map_err(CatalogSkillError::Refused)?;
    let entry = select(&index, spec).map_err(CatalogSkillError::NotListed)?.clone();
    fetch_listed(entry, &location).map_err(CatalogSkillError::Refused)
}

/// The origin an installed catalog skill records, after checking that the
/// unpacked archive holds the skill and version the signed index describes.
///
/// # Errors
/// A failure when the archive holds another skill or version, or cannot be
/// read as a skill.
pub fn checked_origin(store: &graphrt_skill::SkillStore, fetched: &FetchedSkill) -> Result<SkillOrigin, CliError> {
    let review = store.inspect_dir(&fetched.dir).map_err(|error| CliError::failed(error.to_string()))?;
    let entry = &fetched.entry;
    if review.entry.name != entry.name || review.entry.version.as_deref() != Some(entry.version.as_str()) {
        return Err(CliError::failed(format!(
            "skill {} {}: the archive holds `{}` {} instead",
            entry.name,
            entry.version,
            review.entry.name,
            review.entry.version.as_deref().unwrap_or("without a version")
        )));
    }
    Ok(SkillOrigin {
        kind: OriginKind::Catalog,
        detail: Some(entry.tarball_url.clone()),
        publisher: entry.publisher.clone(),
        verified: true,
        version: Some(entry.version.clone()),
    })
}

/// Download, check and unpack the archive of `entry`, an entry of the verified index at `location`.
fn fetch_listed(entry: SkillCatalogEntry, location: &Location) -> Result<FetchedSkill, CliError> {
    let label = format!("skill {} {}", entry.name, entry.version);
    let failed = |error: String| CliError::failed(printable(&format!("{label}: {error}")));
    let archive_location = parse_tarball_location(&entry.tarball_url, location).map_err(failed)?;
    let max_bytes = match entry.size_bytes {
        Some(declared) if declared > MAX_ARCHIVE_BYTES => {
            return Err(failed(format!(
                "the catalog declares a {declared}-byte archive, more than the {MAX_ARCHIVE_BYTES}-byte limit"
            )));
        }
        Some(declared) => declared,
        None => MAX_ARCHIVE_BYTES,
    };
    let archive =
        fetch_bytes(&archive_location, FetchLimits { max_bytes, timeout: ARCHIVE_TIMEOUT }).map_err(failed)?;
    let actual: String = Sha256::digest(&archive).iter().map(|byte| format!("{byte:02x}")).collect();
    let expected = entry.sha256.trim();
    if expected.len() != 64 || !actual.eq_ignore_ascii_case(expected) {
        return Err(failed(format!(
            "sha256 mismatch: the catalog lists `{expected}`, the download is `{actual}`; refusing to install"
        )));
    }
    let scratch = graphrt_core::private_fs::create_fresh_temp_dir("graphrt-catalog-skill")
        .map_err(|error| CliError::failed(format!("cannot create a catalog scratch directory: {error}")))?;
    // From here on the scratch directory is owned by `fetched` and removed with it.
    let fetched = FetchedSkill { dir: scratch.join(&entry.name), entry, scratch };
    extract_tar_gz(&archive, &fetched.scratch, EXTRACT_LIMITS).map_err(failed)?;
    let unexpected = top_level_names(&fetched.scratch).into_iter().find(|name| name != &fetched.entry.name);
    if let Some(name) = unexpected {
        return Err(failed(format!("the archive holds `{name}` next to or instead of `{}/`", fetched.entry.name)));
    }
    if !fetched.dir.join(graphrt_skill::SKILL_FILE).is_file() {
        return Err(failed(format!("the archive has no `{}/SKILL.md`", fetched.entry.name)));
    }
    Ok(fetched)
}

/// Print the latest version of every catalog skill matching `query`.
///
/// # Errors
/// As for [`fetch_catalog_skill`]'s index handling, plus output errors.
pub fn search_catalog_skills(
    query: Option<&str>,
    source: SkillCatalogSource<'_>,
    out: &mut impl Write,
) -> Result<(), CliError> {
    let location = skill_index_location(source.index)?;
    let index = load_verified_index(&location, source)?;
    let needle = query.map(str::to_lowercase);
    let mut latest: Vec<&SkillCatalogEntry> = Vec::new();
    for entry in index.skills.iter().filter(|entry| entry.is_well_formed()) {
        let matches = needle.as_deref().is_none_or(|needle| {
            let display = entry.display_name.as_deref().unwrap_or_default().to_lowercase();
            entry.name.contains(needle) || display.contains(needle) || entry.description.to_lowercase().contains(needle)
        });
        if !matches {
            continue;
        }
        match latest.iter_mut().find(|known| known.name == entry.name) {
            Some(known) if compare_versions(&entry.version, &known.version).is_gt() => *known = entry,
            Some(_) => {}
            None => latest.push(entry),
        }
    }
    latest.sort_by(|a, b| a.name.cmp(&b.name));
    let write_error = |error: std::io::Error| CliError::failed(error.to_string());
    if latest.is_empty() {
        let message = match query {
            Some(query) => format!("no catalog skills match `{query}`"),
            None => "the catalog lists no skills".to_owned(),
        };
        return writeln!(out, "{message}").map_err(write_error);
    }
    writeln!(out, "{:28}  {:10}  DESCRIPTION", "NAME", "VERSION").map_err(write_error)?;
    for entry in latest {
        let first_line = printable(entry.description.lines().next().unwrap_or_default().trim());
        let description = graphrt_skill::text::shorten(&first_line, DESCRIPTION_WIDTH);
        writeln!(out, "{:28}  {:10}  {description}", entry.name, entry.version).map_err(write_error)?;
    }
    Ok(())
}

/// The location of `index-v2.json`: the index location, with a plugin
/// `index.json` file name replaced by its neighbour that lists skills.
fn skill_index_location(index_flag: Option<&str>) -> Result<Location, CliError> {
    let text = super::index_location_text(index_flag);
    let text = match text.strip_suffix(PLUGIN_INDEX_FILE) {
        Some(base) if base.is_empty() || base.ends_with(['/', '\\']) => format!("{base}{SKILL_INDEX_FILE}"),
        _ => text,
    };
    parse_index_location(&text).map_err(CliError::usage)
}

/// Read the index, verify its signature, and only then parse it.
fn load_verified_index(location: &Location, source: SkillCatalogSource<'_>) -> Result<SkillIndex, CliError> {
    // No unsigned mode: `allow_unsigned` is never set for skills.
    let trust = IndexTrust::resolve(TrustOptions { index_key: source.index_key, allow_unsigned: false }, location)?;
    let bytes = fetch_bytes(location, INDEX_LIMITS).map_err(CliError::failed)?;
    let Some(verified) = trust.check(location, &bytes)? else {
        return Err(CliError::failed(format!("{location}: a skill catalog must be signed")));
    };
    let failed = |error: String| CliError::failed(printable(&format!("{location}: {error}")));
    let index: SkillIndex =
        serde_json::from_slice(&bytes).map_err(|error| failed(format!("invalid catalog index: {error}")))?;
    if index.schema != SKILL_INDEX_SCHEMA {
        return Err(failed(format!(
            "catalog index schema {} does not list skills (this graphrt reads skills from schema {SKILL_INDEX_SCHEMA})",
            index.schema
        )));
    }
    let state_path = source.state_file.map(Path::to_path_buf).or_else(super::freshness::default_state_path);
    if let Some(state_path) = state_path {
        let generated_at = index.generated_at.as_ref().and_then(serde_json::Value::as_str);
        let signed = SignedIndex { location: &location.identity(), public_key: &verified.public_key };
        super::freshness::check_and_record(&state_path, signed, generated_at).map_err(failed)?;
    }
    Ok(index)
}

fn select<'a>(index: &'a SkillIndex, spec: &PluginSpec) -> Result<&'a SkillCatalogEntry, CliError> {
    let versions = index.skills.iter().filter(|entry| entry.is_well_formed() && entry.name == spec.name);
    let chosen = match &spec.version {
        Some(wanted) => versions.into_iter().find(|entry| &entry.version == wanted),
        None => versions.max_by(|a, b| compare_versions(&a.version, &b.version)),
    };
    chosen.ok_or_else(|| {
        let wanted = spec.version.as_deref().map(|version| format!(" {version}")).unwrap_or_default();
        CliError::usage(format!("the catalog has no skill `{}`{wanted}; see `graphrt skill search`", spec.name))
    })
}

/// Names directly inside `dir`, as text.
fn top_level_names(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries.filter_map(Result::ok).map(|entry| entry.file_name().to_string_lossy().into_owned()).collect()
}
