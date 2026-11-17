//! Plugin discovery, installation and integrity checks.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::PluginError;
use crate::manifest::{LoadedManifest, manifest_in};

/// Environment variable listing extra plugin directories.
pub const PLUGIN_PATH_ENV: &str = "GRAPHRT_PLUGIN_PATH";

/// Default plugin search paths: `$GRAPHRT_PLUGIN_PATH` entries, then `~/.graphrt/plugins`.
pub fn default_plugin_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = match std::env::var_os(PLUGIN_PATH_ENV) {
        Some(v) => std::env::split_paths(&v).filter(|p| !p.as_os_str().is_empty()).collect(),
        None => Vec::new(),
    };
    if let Some(dir) = default_install_dir() {
        paths.push(dir);
    }
    paths
}

/// `~/.graphrt/plugins`, if a home directory is known.
pub fn default_install_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(".graphrt").join("plugins"))
}

/// Find plugin manifests under `paths`. A path that is itself a plugin directory
/// is returned directly; otherwise its direct subdirectories are scanned.
/// Missing paths are skipped. Results are sorted for determinism.
pub fn discover(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for path in paths {
        if let Some(manifest) = manifest_in(path) {
            found.push(manifest);
            continue;
        }
        let Ok(entries) = std::fs::read_dir(path) else { continue };
        let mut dirs: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.is_dir()).collect();
        dirs.sort();
        found.extend(dirs.iter().filter_map(|d| manifest_in(d)));
    }
    found
}

/// Lowercase hex SHA-256 of a file.
pub fn sha256_file(path: &Path) -> Result<String, PluginError> {
    let bytes =
        std::fs::read(path).map_err(|e| PluginError::Io { path: path.to_path_buf(), message: e.to_string() })?;
    let digest = Sha256::digest(&bytes);
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// Verify every `checksums` entry of a manifest.
pub fn verify_checksums(plugin: &LoadedManifest) -> Result<(), PluginError> {
    for (file, expected) in &plugin.manifest.checksums {
        let actual = sha256_file(&plugin.resolve(file))?;
        if !actual.eq_ignore_ascii_case(expected.trim()) {
            return Err(PluginError::Checksum { plugin: plugin.manifest.name.clone(), file: file.clone() });
        }
    }
    Ok(())
}

/// Copy the plugin in `src` into `<dest_root>/<name>` (replacing an existing copy)
/// and return the installed manifest.
pub fn install_from_dir(src: &Path, dest_root: &Path) -> Result<LoadedManifest, PluginError> {
    let source = LoadedManifest::from_dir(src)?;
    verify_checksums(&source)?;
    let target = dest_root.join(&source.manifest.name);
    let staging = dest_root.join(format!(".{}.installing", source.manifest.name));
    let io = |path: &Path, e: std::io::Error| PluginError::Io { path: path.to_path_buf(), message: e.to_string() };
    std::fs::create_dir_all(dest_root).map_err(|e| io(dest_root, e))?;
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(|e| io(&staging, e))?;
    }
    copy_dir(&source.dir, &staging)?;
    if target.exists() {
        std::fs::remove_dir_all(&target).map_err(|e| io(&target, e))?;
    }
    std::fs::rename(&staging, &target).map_err(|e| io(&target, e))?;
    LoadedManifest::from_dir(&target)
}

/// Clone a git repository (optionally at `rev`, a branch or tag) and install the
/// plugin it contains. Requires the `git` executable.
#[cfg(feature = "git")]
pub fn install_from_git(url: &str, rev: Option<&str>, dest_root: &Path) -> Result<LoadedManifest, PluginError> {
    if url.starts_with('-') || rev.is_some_and(|r| r.starts_with('-')) {
        return Err(PluginError::Install("invalid git url or revision".into()));
    }
    let tmp = std::env::temp_dir().join(format!(
        "graphrt-plugin-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
    ));
    let mut cmd = std::process::Command::new("git");
    cmd.args(["clone", "--depth", "1", "--quiet"]);
    if let Some(rev) = rev {
        cmd.args(["--branch", rev]);
    }
    cmd.arg("--").arg(url).arg(&tmp);
    let output = cmd.output().map_err(|e| PluginError::Install(format!("cannot run git: {e}")))?;
    if !output.status.success() {
        let _cleanup = std::fs::remove_dir_all(&tmp);
        return Err(PluginError::Install(format!(
            "git clone failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let result = install_from_dir(&tmp, dest_root);
    let _cleanup = std::fs::remove_dir_all(&tmp);
    result
}

fn copy_dir(src: &Path, dst: &Path) -> Result<(), PluginError> {
    let io = |path: &Path, e: std::io::Error| PluginError::Io { path: path.to_path_buf(), message: e.to_string() };
    std::fs::create_dir_all(dst).map_err(|e| io(dst, e))?;
    for entry in std::fs::read_dir(src).map_err(|e| io(src, e))? {
        let entry = entry.map_err(|e| io(src, e))?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        let file_type = entry.file_type().map_err(|e| io(&from, e))?;
        if file_type.is_dir() {
            copy_dir(&from, &to)?;
        } else if file_type.is_file() {
            std::fs::copy(&from, &to).map_err(|e| io(&from, e))?;
        }
        // Symlinks are skipped: they could point outside the plugin directory.
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("graphrt-plugin-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_plugin(dir: &Path, name: &str, checksums: serde_json::Value) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("run.sh"), "echo hi").unwrap();
        let manifest = serde_json::json!({
            "name": name, "runtime": "process", "command": "sh", "args": ["run.sh"], "checksums": checksums
        });
        std::fs::write(dir.join("graphrt-plugin.json"), manifest.to_string()).unwrap();
    }

    #[test]
    fn discovers_and_installs() {
        let root = temp("discover");
        write_plugin(&root.join("b"), "b", serde_json::json!({}));
        write_plugin(&root.join("a"), "a", serde_json::json!({}));
        std::fs::create_dir_all(root.join("empty")).unwrap();
        let found = discover(&[root.clone(), root.join("missing")]);
        assert_eq!(found.len(), 2);
        assert!(found[0].ends_with("a/graphrt-plugin.json"));
        // A plugin directory passed directly.
        assert_eq!(discover(&[root.join("a")]).len(), 1);

        let dest = temp("install");
        let installed = install_from_dir(&root.join("a"), &dest).unwrap();
        assert_eq!(installed.manifest.name, "a");
        assert!(dest.join("a").join("run.sh").is_file());
        // Reinstall replaces.
        assert!(install_from_dir(&root.join("a"), &dest).is_ok());
    }

    #[test]
    fn checksums_are_enforced() {
        let root = temp("checksum");
        let digest = {
            std::fs::write(root.join("probe"), "echo hi").unwrap();
            sha256_file(&root.join("probe")).unwrap()
        };
        write_plugin(&root.join("ok"), "ok", serde_json::json!({"run.sh": digest}));
        assert!(verify_checksums(&LoadedManifest::from_dir(&root.join("ok")).unwrap()).is_ok());
        write_plugin(&root.join("bad"), "bad", serde_json::json!({"run.sh": "00ff"}));
        let bad = LoadedManifest::from_dir(&root.join("bad")).unwrap();
        assert!(matches!(verify_checksums(&bad), Err(PluginError::Checksum { .. })));
        assert!(install_from_dir(&root.join("bad"), &temp("dest-bad")).is_err());
    }

    #[test]
    fn sha256_known_value() {
        let root = temp("sha");
        std::fs::write(root.join("f"), "abc").unwrap();
        assert_eq!(
            sha256_file(&root.join("f")).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
