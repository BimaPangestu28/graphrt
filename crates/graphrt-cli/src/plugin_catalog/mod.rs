//! The graphrt-plugins catalog: `graphrt plugin install <name>[@version]` and
//! `graphrt plugin search`.
//!
//! The catalog is the `catalog` release of github.com/BimaPangestu28/graphrt-plugins:
//! an `index.json` (see [`index`]) listing every published plugin with its
//! tarball URL, sha256 and status labels. Installing by name:
//!
//! 1. reads the index (`--index`, `$GRAPHRT_PLUGIN_INDEX` or [`DEFAULT_INDEX_URL`]);
//! 2. picks the latest version, or the exact one asked for;
//! 3. prints the status and ToS risk, and stops unless `--yes` was given when
//!    the plugin is experimental or its ToS risk is high;
//! 4. downloads the tarball over https (or from a local file when the index is
//!    local), capped in size and time, and checks its sha256 against the index;
//! 5. extracts it into a scratch directory, refusing unsafe entries;
//! 6. installs the extracted `<name>/` directory exactly like a local directory
//!    install (manifest validation, checksums, atomic replace).

#[cfg(feature = "plugins")]
mod archive;
#[cfg(feature = "plugins")]
mod fetch;
pub mod index;

#[cfg(feature = "plugins")]
pub use installer::{CatalogInstall, install_from_catalog, search_catalog};

/// The published catalog index.
pub const DEFAULT_INDEX_URL: &str =
    "https://github.com/BimaPangestu28/graphrt-plugins/releases/download/catalog/index.json";

/// Environment variable overriding [`DEFAULT_INDEX_URL`] (`--index` wins over it).
pub const INDEX_ENV: &str = "GRAPHRT_PLUGIN_INDEX";

/// The index location to use: `--index`, then `$GRAPHRT_PLUGIN_INDEX`, then the default.
pub fn index_location_text(index_flag: Option<&str>) -> String {
    if let Some(flag) = index_flag {
        return flag.to_owned();
    }
    match std::env::var(INDEX_ENV) {
        Ok(value) if !value.trim().is_empty() => value,
        _ => DEFAULT_INDEX_URL.to_owned(),
    }
}

#[cfg(feature = "plugins")]
mod installer {
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use sha2::{Digest, Sha256};

    use super::archive::{ExtractLimits, extract_tar_gz};
    use super::fetch::{FetchLimits, Location, fetch_bytes, parse_index_location, parse_tarball_location};
    use super::index::{CatalogEntry, CatalogIndex, PluginSpec, parse_index, search_entries, select_entry};
    use crate::error::CliError;

    /// The index lists every plugin with its node schemas: a few MiB at most.
    const INDEX_LIMITS: FetchLimits = FetchLimits { max_bytes: 32 * 1024 * 1024, timeout: Duration::from_secs(60) };
    /// Plugin tarballs bundle their npm dependencies (a few MiB today).
    const MAX_TARBALL_BYTES: u64 = 256 * 1024 * 1024;
    const TARBALL_TIMEOUT: Duration = Duration::from_secs(300);
    const EXTRACT_LIMITS: ExtractLimits = ExtractLimits { max_total_bytes: 1024 * 1024 * 1024, max_entries: 100_000 };
    const DESCRIPTION_WIDTH: usize = 72;

    /// What `graphrt plugin install <name>` was asked for.
    #[derive(Debug)]
    pub struct CatalogInstall<'a> {
        pub spec: PluginSpec,
        /// `--index`, if given.
        pub index: Option<&'a str>,
        /// `--yes`: install experimental and high-ToS-risk plugins without stopping.
        pub assume_yes: bool,
    }

    /// Install a catalog plugin into `<install_root>/<name>`, printing its labels to `out`.
    ///
    /// # Errors
    /// Usage errors (exit 2) for a bad index location, an unknown name or version
    /// and a missing `--yes`; failures (exit 1) for download, checksum,
    /// extraction and install problems.
    pub fn install_from_catalog(
        request: &CatalogInstall<'_>,
        install_root: &Path,
        out: &mut impl Write,
    ) -> Result<graphrt_plugin::LoadedManifest, CliError> {
        let index_location = resolve_index_location(request.index)?;
        let catalog = load_index(&index_location)?;
        let entry = select_entry(&catalog, &request.spec).map_err(|error| CliError::usage(error.to_string()))?;
        print_labels(entry, out)?;
        require_confirmation(entry, request.assume_yes)?;
        let tarball = download_verified_tarball(entry, &index_location)?;
        let scratch = ScratchDir::create()?;
        extract_tar_gz(&tarball, scratch.path(), EXTRACT_LIMITS)
            .map_err(|error| CliError::failed(format!("{} {}: {error}", entry.name, entry.version)))?;
        let plugin_dir = staged_plugin_dir(scratch.path(), entry)?;
        graphrt_plugin::install_from_dir(&plugin_dir, install_root).map_err(|error| CliError::failed(error.to_string()))
    }

    /// Print the latest version of every catalog plugin matching `query`.
    ///
    /// # Errors
    /// As for [`install_from_catalog`]'s index handling, plus output errors.
    pub fn search_catalog(query: Option<&str>, index_flag: Option<&str>, out: &mut impl Write) -> Result<(), CliError> {
        let catalog = load_index(&resolve_index_location(index_flag)?)?;
        let matches = search_entries(&catalog, query);
        let write_error = |error: std::io::Error| CliError::failed(error.to_string());
        if matches.is_empty() {
            let message = match query {
                Some(query) => format!("no catalog plugins match `{query}`"),
                None => "the plugin catalog is empty".to_owned(),
            };
            return writeln!(out, "{message}").map_err(write_error);
        }
        writeln!(out, "{:24}  {:10}  {:12}  DESCRIPTION", "NAME", "VERSION", "STATUS").map_err(write_error)?;
        for entry in matches {
            let description = entry.one_line_description(DESCRIPTION_WIDTH);
            writeln!(out, "{:24}  {:10}  {:12}  {description}", entry.name, entry.version, entry.status_label())
                .map_err(write_error)?;
        }
        Ok(())
    }

    fn resolve_index_location(index_flag: Option<&str>) -> Result<Location, CliError> {
        parse_index_location(&super::index_location_text(index_flag)).map_err(CliError::usage)
    }

    fn load_index(location: &Location) -> Result<CatalogIndex, CliError> {
        let index_bytes = fetch_bytes(location, INDEX_LIMITS).map_err(CliError::failed)?;
        parse_index(&index_bytes).map_err(|error| CliError::failed(format!("{location}: {error}")))
    }

    fn print_labels(entry: &CatalogEntry, out: &mut impl Write) -> Result<(), CliError> {
        let write_error = |error: std::io::Error| CliError::failed(error.to_string());
        writeln!(
            out,
            "{} {}: status {}, ToS risk {}",
            entry.name,
            entry.version,
            entry.status_label(),
            entry.tos_risk_label()
        )
        .map_err(write_error)?;
        if let Some(reason) = entry.status_reason.as_deref().filter(|reason| !reason.trim().is_empty()) {
            writeln!(out, "  {}", reason.trim()).map_err(write_error)?;
        }
        if let Some(warning) = entry.warning.as_deref().filter(|warning| !warning.trim().is_empty()) {
            eprintln!("warning: {}: {}", entry.name, warning.trim());
        }
        Ok(())
    }

    /// graphrt has no interactive prompt, so the confirmation is always `--yes`.
    fn require_confirmation(entry: &CatalogEntry, assume_yes: bool) -> Result<(), CliError> {
        if assume_yes || !entry.needs_confirmation() {
            return Ok(());
        }
        let mut labels = Vec::new();
        if entry.status.as_deref() == Some("experimental") {
            labels.push("status experimental".to_owned());
        }
        if entry.tos_risk.as_deref() == Some("high") {
            labels.push("ToS risk high".to_owned());
        }
        Err(CliError::usage(format!(
            "`{}` {} is labelled {}; re-run with `--yes` to install it anyway (there is no interactive prompt)",
            entry.name,
            entry.version,
            labels.join(" and ")
        )))
    }

    fn download_verified_tarball(entry: &CatalogEntry, index_location: &Location) -> Result<Vec<u8>, CliError> {
        let label = format!("{} {}", entry.name, entry.version);
        let location = parse_tarball_location(&entry.tarball_url, index_location)
            .map_err(|error| CliError::failed(format!("{label}: {error}")))?;
        let max_bytes = tarball_size_limit(entry)?;
        let tarball = fetch_bytes(&location, FetchLimits { max_bytes, timeout: TARBALL_TIMEOUT })
            .map_err(|error| CliError::failed(format!("{label}: {error}")))?;
        verify_sha256(&tarball, entry)?;
        Ok(tarball)
    }

    /// The catalog's `size_bytes` when declared (a larger download is refused),
    /// never more than [`MAX_TARBALL_BYTES`].
    fn tarball_size_limit(entry: &CatalogEntry) -> Result<u64, CliError> {
        match entry.size_bytes {
            Some(declared) if declared > MAX_TARBALL_BYTES => Err(CliError::failed(format!(
                "{} {}: the catalog declares a {declared}-byte tarball, more than the {MAX_TARBALL_BYTES}-byte limit",
                entry.name, entry.version
            ))),
            Some(declared) => Ok(declared),
            None => Ok(MAX_TARBALL_BYTES),
        }
    }

    fn verify_sha256(tarball: &[u8], entry: &CatalogEntry) -> Result<(), CliError> {
        let expected = entry.sha256.trim();
        let actual: String = Sha256::digest(tarball).iter().map(|byte| format!("{byte:02x}")).collect();
        if expected.len() != 64 || !actual.eq_ignore_ascii_case(expected) {
            return Err(CliError::failed(format!(
                "{} {}: sha256 mismatch: the catalog lists `{expected}`, the download is `{actual}`; refusing to install",
                entry.name, entry.version
            )));
        }
        Ok(())
    }

    /// The tarball's top-level `<name>/` directory, whose manifest must match the entry.
    fn staged_plugin_dir(scratch: &Path, entry: &CatalogEntry) -> Result<PathBuf, CliError> {
        let plugin_dir = scratch.join(&entry.name);
        let staged = graphrt_plugin::LoadedManifest::from_dir(&plugin_dir).map_err(|error| {
            CliError::failed(format!(
                "{} {}: the tarball has no usable top-level `{}/` plugin directory: {error}",
                entry.name, entry.version, entry.name
            ))
        })?;
        if staged.manifest.name != entry.name || staged.manifest.version != entry.version {
            return Err(CliError::failed(format!(
                "{} {}: the tarball holds `{}` {} instead",
                entry.name, entry.version, staged.manifest.name, staged.manifest.version
            )));
        }
        Ok(plugin_dir)
    }

    /// A fresh temporary directory, removed on drop.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn create() -> Result<Self, CliError> {
            let nanos =
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
            let path = std::env::temp_dir().join(format!("graphrt-catalog-{}-{nanos}", std::process::id()));
            std::fs::create_dir(&path)
                .map_err(|error| CliError::failed(format!("cannot create {}: {error}", path.display())))?;
            Ok(Self(path))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            if let Err(error) = std::fs::remove_dir_all(&self.0) {
                eprintln!("warning: cannot remove {}: {error}", self.0.display());
            }
        }
    }
}
