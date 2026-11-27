//! The graphrt-plugins catalog: `graphrt plugin install <name>[@version]` and
//! `graphrt plugin search`.
//!
//! The catalog is the `catalog` release of github.com/Graphrt/graphrt-plugins:
//! an `index.json` (see [`index`]) listing every published plugin with its
//! tarball URL, sha256 and status labels. Installing by name:
//!
//! 1. reads the index (`--index`, `$GRAPHRT_PLUGIN_INDEX` or [`DEFAULT_INDEX_URL`])
//!    and its minisign signature (`<index>.minisig`), checks the signature
//!    against the trusted catalog key before the index is parsed (see `trust`),
//!    and refuses an index older than one already used (see `freshness`);
//! 2. picks the latest version, or the exact one asked for;
//! 3. prints the status and ToS risk, and stops unless `--yes` was given when
//!    the plugin is experimental or its ToS risk is high;
//! 4. downloads the tarball over https (or from a local file when the index is
//!    local), capped in size and time, and checks its sha256 against the
//!    verified index;
//! 5. extracts it into a scratch directory, refusing unsafe entries;
//! 6. installs the extracted `<name>/` directory exactly like a local directory
//!    install (manifest validation, checksums, staged copy, remove-then-rename).

#[cfg(feature = "plugins")]
mod archive;
#[cfg(feature = "plugins")]
mod fetch;
#[cfg(feature = "plugins")]
mod freshness;
pub mod index;
#[cfg(all(feature = "plugins", feature = "skills"))]
pub mod skills;
#[cfg(feature = "plugins")]
mod trust;

#[cfg(feature = "plugins")]
pub use installer::{CatalogInstall, install_from_catalog, search_catalog};
#[cfg(feature = "plugins")]
pub use trust::{INDEX_KEY_ENV, TrustOptions};

/// How long reading a signature or a public key file may take.
#[cfg(feature = "plugins")]
const SIGNATURE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The published catalog index.
pub const DEFAULT_INDEX_URL: &str = "https://github.com/Graphrt/graphrt-plugins/releases/download/catalog/index.json";

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
    use super::freshness::SignedIndex;
    use super::index::{CatalogEntry, CatalogIndex, PluginSpec, parse_index, printable, search_entries, select_entry};
    use super::trust::{IndexTrust, TrustOptions};
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
        /// `--index-key` and `--allow-unsigned-index`.
        pub trust: TrustOptions<'a>,
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
        let catalog = load_index(&index_location, request.trust)?;
        let entry = select_entry(&catalog, &request.spec).map_err(|error| CliError::usage(error.to_string()))?;
        print_labels(entry, out)?;
        require_confirmation(entry, request.assume_yes)?;
        let tarball = download_verified_tarball(entry, &index_location)?;
        let scratch = ScratchDir::create()?;
        let label = format!("{} {}", entry.name, entry.version);
        extract_tar_gz(&tarball, scratch.path(), EXTRACT_LIMITS).map_err(|error| tarball_failure(&label, error))?;
        let plugin_dir = staged_plugin_dir(scratch.path(), entry)?;
        graphrt_plugin::install_from_dir(&plugin_dir, install_root).map_err(|error| tarball_failure(&label, error))
    }

    /// A failure whose message may quote the tarball's contents (entry paths,
    /// manifest fields). The sha256 only proves the tarball is the one the
    /// index lists, not that its author is benign, so the message is made
    /// printable like every index string.
    fn tarball_failure(label: &str, error: impl std::fmt::Display) -> CliError {
        CliError::failed(printable(&format!("{label}: {error}")))
    }

    /// Print the latest version of every catalog plugin matching `query`.
    ///
    /// # Errors
    /// As for [`install_from_catalog`]'s index handling, plus output errors.
    pub fn search_catalog(
        query: Option<&str>,
        index_flag: Option<&str>,
        trust: TrustOptions<'_>,
        out: &mut impl Write,
    ) -> Result<(), CliError> {
        let catalog = load_index(&resolve_index_location(index_flag)?, trust)?;
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

    /// Read the index, check its signature, and only then parse it. A signed
    /// index must also not be older than the newest one already accepted.
    fn load_index(location: &Location, trust: TrustOptions<'_>) -> Result<CatalogIndex, CliError> {
        let trust = IndexTrust::resolve(trust, location)?;
        let index_bytes = fetch_bytes(location, INDEX_LIMITS).map_err(CliError::failed)?;
        let verified = trust.check(location, &index_bytes)?;
        let catalog = parse_index(&index_bytes).map_err(|error| CliError::failed(format!("{location}: {error}")))?;
        if let (Some(verified), Some(state_path)) = (verified, super::freshness::default_state_path()) {
            let generated_at = catalog.generated_at.as_ref().and_then(serde_json::Value::as_str);
            let signed = SignedIndex { location: &location.identity(), public_key: &verified.public_key };
            super::freshness::check_and_record(&state_path, signed, generated_at)
                .map_err(|error| CliError::failed(printable(&format!("{location}: {error}"))))?;
        }
        Ok(catalog)
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
            writeln!(out, "  {}", printable(reason.trim())).map_err(write_error)?;
        }
        if let Some(warning) = entry.warning.as_deref().filter(|warning| !warning.trim().is_empty()) {
            eprintln!("warning: {}: {}", entry.name, printable(warning.trim()));
        }
        Ok(())
    }

    /// graphrt has no interactive prompt, so the confirmation is always `--yes`.
    fn require_confirmation(entry: &CatalogEntry, assume_yes: bool) -> Result<(), CliError> {
        let reasons = entry.confirmation_reasons();
        if assume_yes || reasons.is_empty() {
            return Ok(());
        }
        Err(CliError::usage(format!(
            "`{}` {} needs confirmation: {}; re-run with `--yes` to install it anyway (there is no interactive prompt)",
            entry.name,
            entry.version,
            reasons.join("; ")
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
                "{} {}: sha256 mismatch: the catalog lists `{}`, the download is `{actual}`; refusing to install",
                entry.name,
                entry.version,
                printable(expected)
            )));
        }
        Ok(())
    }

    /// The tarball's top-level `<name>/` directory, whose manifest must match the entry.
    fn staged_plugin_dir(scratch: &Path, entry: &CatalogEntry) -> Result<PathBuf, CliError> {
        let plugin_dir = scratch.join(&entry.name);
        let staged = graphrt_plugin::LoadedManifest::from_dir(&plugin_dir).map_err(|error| {
            let label = format!("{} {}", entry.name, entry.version);
            tarball_failure(
                &label,
                format!("the tarball has no usable top-level `{}/` plugin directory: {error}", entry.name),
            )
        })?;
        if staged.manifest.name != entry.name || staged.manifest.version != entry.version {
            return Err(CliError::failed(format!(
                "{} {}: the tarball holds `{}` {} instead",
                entry.name,
                entry.version,
                staged.manifest.name,
                printable(&staged.manifest.version)
            )));
        }
        Ok(plugin_dir)
    }

    /// A fresh temporary directory, removed on drop.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        /// A new directory, never an existing one, owner-only on Unix: the
        /// extracted files are checksum-verified and then copied, so another
        /// local user able to write here (a shared `/tmp` under a permissive
        /// umask) could swap a file between the two.
        fn create() -> Result<Self, CliError> {
            graphrt_core::private_fs::create_fresh_temp_dir("graphrt-catalog")
                .map(Self)
                .map_err(|error| CliError::failed(format!("cannot create a catalog scratch directory: {error}")))
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

    #[cfg(all(test, unix))]
    mod tests {
        use std::os::unix::fs::PermissionsExt;

        use super::ScratchDir;

        #[test]
        fn scratch_directories_are_owner_only() {
            let scratch = ScratchDir::create().expect("scratch dir");
            let mode = std::fs::metadata(scratch.path()).expect("metadata").permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }
}
