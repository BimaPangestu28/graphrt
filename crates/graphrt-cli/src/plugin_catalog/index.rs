//! The plugin catalog index (`index.json`).
//!
//! The format is defined by the graphrt-plugins repository
//! (`scripts/lib/plugin-package.mjs`, `buildCatalog` and `buildCatalogEntry`):
//!
//! ```json
//! { "schema": 1, "generated_at": "2026-11-19T08:14:13Z",
//!   "plugins": [{ "name": "open-meteo", "version": "0.1.0", "description": "...",
//!                 "tarball_url": "https://github.com/.../open-meteo-0.1.0.tar.gz",
//!                 "sha256": "<64 hex>", "size_bytes": 12345,
//!                 "status": "beta", "status_reason": "...", "tos_risk": "none",
//!                 "warning": null, "permissions": {...}, "nodes": [...], ... }] }
//! ```
//!
//! Only the fields the installer needs are read. Fields added later within
//! schema 1 are ignored (the generator documents them as additive), and the
//! status labels are optional because catalogs published before they existed
//! do not carry them.

use std::cmp::Ordering;

use serde::Deserialize;

/// The catalog `schema` this reader understands.
pub const SUPPORTED_SCHEMA: u64 = 1;

/// A parsed `index.json`.
#[derive(Clone, Debug, Deserialize)]
pub struct CatalogIndex {
    /// Format version; see [`SUPPORTED_SCHEMA`].
    pub schema: u64,
    /// Every published plugin release.
    pub plugins: Vec<CatalogEntry>,
}

/// One published plugin release.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct CatalogEntry {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// Where the `<name>-<version>.tar.gz` release tarball is downloaded from.
    pub tarball_url: String,
    /// Lowercase hex sha256 of the tarball bytes.
    pub sha256: String,
    /// Tarball size in bytes.
    #[serde(default)]
    pub size_bytes: Option<u64>,
    /// `stable`, `beta` or `experimental`.
    #[serde(default)]
    pub status: Option<String>,
    /// Why the plugin has its status.
    #[serde(default)]
    pub status_reason: Option<String>,
    /// Terms of Service risk: `none`, `low`, `medium` or `high`.
    #[serde(default)]
    pub tos_risk: Option<String>,
    /// The first warning of the plugin README, as one line of markdown.
    #[serde(default)]
    pub warning: Option<String>,
}

impl CatalogEntry {
    /// The status label, or `unknown` when the catalog does not carry one.
    pub fn status_label(&self) -> &str {
        self.status.as_deref().unwrap_or("unknown")
    }

    /// The ToS risk label, or `unknown` when the catalog does not carry one.
    pub fn tos_risk_label(&self) -> &str {
        self.tos_risk.as_deref().unwrap_or("unknown")
    }

    /// Experimental plugins and plugins with a high ToS risk are installed only
    /// after an explicit confirmation (`--yes`).
    pub fn needs_confirmation(&self) -> bool {
        self.status.as_deref() == Some("experimental") || self.tos_risk.as_deref() == Some("high")
    }

    /// The first line of the description, shortened to `max_chars` characters.
    pub fn one_line_description(&self, max_chars: usize) -> String {
        let first_line = self.description.lines().next().unwrap_or_default().trim();
        if first_line.chars().count() <= max_chars {
            return first_line.to_owned();
        }
        let shortened: String = first_line.chars().take(max_chars.saturating_sub(3)).collect();
        format!("{}...", shortened.trim_end())
    }
}

/// Parse `index.json` bytes.
///
/// # Errors
/// A message for invalid JSON, a missing required field or an unsupported `schema`.
pub fn parse_index(index_bytes: &[u8]) -> Result<CatalogIndex, String> {
    let index: CatalogIndex =
        serde_json::from_slice(index_bytes).map_err(|error| format!("invalid catalog index: {error}"))?;
    if index.schema != SUPPORTED_SCHEMA {
        return Err(format!(
            "catalog index schema {} is not supported (this graphrt reads schema {SUPPORTED_SCHEMA}); upgrade graphrt",
            index.schema
        ));
    }
    Ok(index)
}

/// A plugin requested by name: `name` or `name@version`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginSpec {
    pub name: String,
    pub version: Option<String>,
}

/// Parse `name` or `name@version` (a valid plugin name and a semver version).
/// Returns `None` for anything else, such as paths and git URLs.
pub fn parse_plugin_spec(source: &str) -> Option<PluginSpec> {
    let (name, version) = match source.split_once('@') {
        Some((name, version)) => (name, Some(version)),
        None => (source, None),
    };
    if !graphrt_plugin::is_valid_name(name) || version.is_some_and(|v| !is_release_version(v)) {
        return None;
    }
    Some(PluginSpec { name: name.to_owned(), version: version.map(str::to_owned) })
}

/// Versions the catalog publishes: `MAJOR.MINOR.PATCH` with an optional
/// `-prerelease` or `+build` suffix of `[0-9A-Za-z.-]` (the generator's rule).
fn is_release_version(version: &str) -> bool {
    let (core, suffix) = match version.find(['-', '+']) {
        Some(position) => (version.get(..position).unwrap_or_default(), version.get(position + 1..)),
        None => (version, None),
    };
    let core_is_valid = core.split('.').count() == 3
        && core.split('.').all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()));
    let suffix_is_valid = suffix
        .is_none_or(|text| !text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'));
    core_is_valid && suffix_is_valid
}

/// Semver precedence of two versions (build metadata ignored). Versions that
/// do not parse sort before every valid one.
pub fn compare_versions(left: &str, right: &str) -> Ordering {
    match (parse_version(left), parse_version(right)) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => left.cmp(right),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedVersion<'a> {
    core: [u64; 3],
    prerelease: Option<&'a str>,
}

impl Ord for ParsedVersion<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core.cmp(&other.core).then_with(|| match (self.prerelease, other.prerelease) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(left), Some(right)) => compare_prereleases(left, right),
        })
    }
}

impl PartialOrd for ParsedVersion<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn parse_version(version: &str) -> Option<ParsedVersion<'_>> {
    let without_build = version.split('+').next().unwrap_or_default();
    let (core_text, prerelease) = match without_build.split_once('-') {
        Some((core, prerelease)) => (core, Some(prerelease)),
        None => (without_build, None),
    };
    let mut numbers = core_text.split('.').map(|part| part.parse::<u64>().ok());
    let core = [numbers.next()??, numbers.next()??, numbers.next()??];
    if numbers.next().is_some() {
        return None;
    }
    Some(ParsedVersion { core, prerelease })
}

/// Semver rule 11: numeric identifiers compare numerically and sort before
/// alphanumeric ones; a longer list wins when all shared identifiers are equal.
fn compare_prereleases(left: &str, right: &str) -> Ordering {
    let mut left_parts = left.split('.');
    let mut right_parts = right.split('.');
    loop {
        let ordering = match (left_parts.next(), right_parts.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(left_part), Some(right_part)) => match (left_part.parse::<u64>(), right_part.parse::<u64>()) {
                (Ok(left_number), Ok(right_number)) => left_number.cmp(&right_number),
                (Ok(_), Err(_)) => Ordering::Less,
                (Err(_), Ok(_)) => Ordering::Greater,
                (Err(_), Err(_)) => left_part.cmp(right_part),
            },
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
}

/// Why no catalog entry matches a [`PluginSpec`].
#[derive(Debug, PartialEq, Eq)]
pub enum SelectionError {
    /// No entry has this name.
    UnknownName(String),
    /// Entries with this name exist, but none with the requested version.
    UnknownVersion { name: String, version: String, available: Vec<String> },
}

impl std::fmt::Display for SelectionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownName(name) => {
                write!(formatter, "`{name}` is not in the plugin catalog (see `graphrt plugin search`)")
            }
            Self::UnknownVersion { name, version, available } => {
                write!(
                    formatter,
                    "no version {version} of `{name}` in the plugin catalog (available: {})",
                    available.join(", ")
                )
            }
        }
    }
}

/// Pick the entry for `spec`: the exact version when one is requested,
/// otherwise the highest version by semver precedence.
///
/// # Errors
/// [`SelectionError`] when the name or the requested version is not listed.
pub fn select_entry<'a>(index: &'a CatalogIndex, spec: &PluginSpec) -> Result<&'a CatalogEntry, SelectionError> {
    let mut candidates: Vec<&CatalogEntry> = index.plugins.iter().filter(|entry| entry.name == spec.name).collect();
    candidates.sort_by(|left, right| compare_versions(&left.version, &right.version));
    let Some(latest) = candidates.last().copied() else {
        return Err(SelectionError::UnknownName(spec.name.clone()));
    };
    let Some(requested) = &spec.version else {
        return Ok(latest);
    };
    candidates.iter().copied().find(|entry| &entry.version == requested).ok_or_else(|| SelectionError::UnknownVersion {
        name: spec.name.clone(),
        version: requested.clone(),
        available: candidates.iter().map(|entry| entry.version.clone()).collect(),
    })
}

/// The latest entry of every plugin whose name or description contains `query`
/// (case-insensitive; every plugin when `query` is `None`), sorted by name.
pub fn search_entries<'a>(index: &'a CatalogIndex, query: Option<&str>) -> Vec<&'a CatalogEntry> {
    let needle = query.map(str::to_lowercase).unwrap_or_default();
    let mut latest_by_name: std::collections::BTreeMap<&str, &CatalogEntry> = std::collections::BTreeMap::new();
    for entry in &index.plugins {
        let matches = entry.name.to_lowercase().contains(&needle) || entry.description.to_lowercase().contains(&needle);
        if !matches {
            continue;
        }
        let slot = latest_by_name.entry(entry.name.as_str()).or_insert(entry);
        if compare_versions(&entry.version, &slot.version) == Ordering::Greater {
            *slot = entry;
        }
    }
    latest_by_name.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, version: &str) -> CatalogEntry {
        CatalogEntry { name: name.into(), version: version.into(), ..CatalogEntry::default() }
    }

    fn index_of(entries: Vec<CatalogEntry>) -> CatalogIndex {
        CatalogIndex { schema: 1, plugins: entries }
    }

    #[test]
    fn parses_the_generator_format_and_ignores_extra_fields() {
        let text = r#"{"schema": 1, "generated_at": "2026-11-19T08:14:13Z", "plugins": [{
            "name": "meta-api", "version": "0.1.0", "description": "Meta Graph API",
            "tarball_url": "https://github.com/x/y/releases/download/meta-api-v0.1.0/meta-api-0.1.0.tar.gz",
            "sha256": "0ea7", "size_bytes": 29257, "permissions": {"network": true, "secrets": true, "env": []},
            "needs_browser": false, "node": ">=20", "warning": null, "status": "beta",
            "status_reason": "Checked live.", "tos_risk": "low", "operations_count": 1,
            "nodes": [{"kind": "meta-api.x", "config_schema": {}}], "tools": ["x"], "readme_url": "https://x"}]}"#;
        let index = parse_index(text.as_bytes()).unwrap();
        let parsed = &index.plugins[0];
        assert_eq!(parsed.name, "meta-api");
        assert_eq!(parsed.size_bytes, Some(29257));
        assert_eq!(parsed.status_label(), "beta");
        assert_eq!(parsed.tos_risk_label(), "low");
        assert!(!parsed.needs_confirmation());
    }

    #[test]
    fn catalogs_without_status_labels_still_parse() {
        let text = r#"{"schema": 1, "plugins": [{"name": "x-api", "version": "0.1.0",
            "tarball_url": "https://x", "sha256": "00"}]}"#;
        let index = parse_index(text.as_bytes()).unwrap();
        assert_eq!(index.plugins[0].status_label(), "unknown");
        assert_eq!(index.plugins[0].tos_risk_label(), "unknown");
        assert!(!index.plugins[0].needs_confirmation());
    }

    #[test]
    fn rejects_other_schemas_and_missing_fields() {
        let future = parse_index(br#"{"schema": 2, "plugins": []}"#).unwrap_err();
        assert!(future.contains("schema 2 is not supported"), "{future}");
        let missing_sha =
            parse_index(br#"{"schema": 1, "plugins": [{"name": "a", "version": "1.0.0", "tarball_url": "x"}]}"#);
        assert!(missing_sha.unwrap_err().contains("sha256"));
        assert!(parse_index(b"not json").is_err());
    }

    #[test]
    fn experimental_or_high_risk_needs_confirmation() {
        let experimental = CatalogEntry { status: Some("experimental".into()), ..entry("a", "1.0.0") };
        let risky = CatalogEntry { tos_risk: Some("high".into()), ..entry("a", "1.0.0") };
        let medium =
            CatalogEntry { status: Some("stable".into()), tos_risk: Some("medium".into()), ..entry("a", "1.0.0") };
        assert!(experimental.needs_confirmation());
        assert!(risky.needs_confirmation());
        assert!(!medium.needs_confirmation());
    }

    #[test]
    fn parses_names_and_name_at_version() {
        assert_eq!(parse_plugin_spec("open-meteo"), Some(PluginSpec { name: "open-meteo".into(), version: None }));
        assert_eq!(
            parse_plugin_spec("open-meteo@0.1.0-rc.1"),
            Some(PluginSpec { name: "open-meteo".into(), version: Some("0.1.0-rc.1".into()) })
        );
        for not_a_spec in [
            "./open-meteo",
            "/abs/path",
            "Open-Meteo",
            "git@github.com:acme/plugin.git",
            "https://github.com/acme/plugin",
            "name@",
            "name@1.0",
            "name@1.0.0.0",
            "name@v1.0.0",
            "name@1.0.0-",
            "name@1.0.0-rc+1",
            "",
        ] {
            assert_eq!(parse_plugin_spec(not_a_spec), None, "{not_a_spec}");
        }
    }

    #[test]
    fn orders_versions_by_semver_precedence() {
        let mut versions = vec!["0.10.0", "0.2.0", "1.0.0", "1.0.0-rc.2", "1.0.0-rc.10", "1.0.0-alpha", "0.2.0+build"];
        versions.sort_by(|left, right| compare_versions(left, right));
        assert_eq!(versions, ["0.2.0", "0.2.0+build", "0.10.0", "1.0.0-alpha", "1.0.0-rc.2", "1.0.0-rc.10", "1.0.0"]);
        assert_eq!(compare_versions("garbage", "0.0.1"), Ordering::Less);
    }

    #[test]
    fn selects_latest_or_exact_versions() {
        let index = index_of(vec![entry("demo", "0.2.0"), entry("demo", "0.10.0"), entry("other", "9.0.0")]);
        let latest = select_entry(&index, &PluginSpec { name: "demo".into(), version: None }).unwrap();
        assert_eq!(latest.version, "0.10.0");
        let exact = select_entry(&index, &PluginSpec { name: "demo".into(), version: Some("0.2.0".into()) }).unwrap();
        assert_eq!(exact.version, "0.2.0");
        let missing_version = select_entry(&index, &PluginSpec { name: "demo".into(), version: Some("1.0.0".into()) });
        assert_eq!(
            missing_version.unwrap_err(),
            SelectionError::UnknownVersion {
                name: "demo".into(),
                version: "1.0.0".into(),
                available: vec!["0.2.0".into(), "0.10.0".into()]
            }
        );
        let missing_name = select_entry(&index, &PluginSpec { name: "nosuch".into(), version: None });
        assert_eq!(missing_name.unwrap_err(), SelectionError::UnknownName("nosuch".into()));
    }

    #[test]
    fn searches_names_and_descriptions_keeping_the_latest_version() {
        let weather = CatalogEntry { description: "Weather FORECASTS".into(), ..entry("open-meteo", "0.1.0") };
        let index = index_of(vec![entry("demo", "0.2.0"), entry("demo", "0.10.0"), weather]);
        let all: Vec<(&str, &str)> =
            search_entries(&index, None).iter().map(|e| (e.name.as_str(), e.version.as_str())).collect();
        assert_eq!(all, [("demo", "0.10.0"), ("open-meteo", "0.1.0")]);
        let by_description = search_entries(&index, Some("forecast"));
        assert_eq!(by_description.len(), 1);
        assert_eq!(by_description[0].name, "open-meteo");
        assert!(search_entries(&index, Some("zzz")).is_empty());
    }

    #[test]
    fn shortens_descriptions_to_one_line() {
        let described = CatalogEntry { description: "First line here\nsecond".into(), ..entry("a", "1.0.0") };
        assert_eq!(described.one_line_description(80), "First line here");
        assert_eq!(described.one_line_description(10), "First l...");
    }
}
