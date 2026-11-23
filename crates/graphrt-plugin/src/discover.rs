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
        dirs.sort_unstable();
        found.extend(dirs.iter().filter_map(|d| manifest_in(d)));
    }
    found
}

/// Lowercase hex SHA-256 of a file, read in chunks (no whole-file buffer).
pub fn sha256_file(path: &Path) -> Result<String, PluginError> {
    use std::io::Read;
    let io = |e: std::io::Error| PluginError::Io { path: path.to_path_buf(), message: e.to_string() };
    let mut file = std::fs::File::open(path).map_err(io)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(io)?;
        if n == 0 {
            break;
        }
        hasher.update(buf.get(..n).unwrap_or_default());
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Verify every `checksums` entry of a manifest.
///
/// Limits (see `docs/plugins.md`): the hashes live in the manifest they protect,
/// so they detect corruption and partial edits, not an attacker who can rewrite
/// the plugin directory; files are hashed and then executed by path. Keep plugin
/// directories writable only by the operator. Symlinked files are rejected so a
/// pinned path cannot be redirected outside the plugin directory.
pub fn verify_checksums(plugin: &LoadedManifest) -> Result<(), PluginError> {
    for (file, expected) in &plugin.manifest.checksums {
        let path = plugin.resolve(file);
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| PluginError::Io { path: path.clone(), message: e.to_string() })?;
        if !meta.file_type().is_file() {
            return Err(PluginError::Manifest(format!(
                "plugin `{}`: checksum path `{file}` must be a regular file (not a symlink or directory)",
                plugin.manifest.name
            )));
        }
        let actual = sha256_file(&path)?;
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
/// plugin at its root. Requires the `git` executable.
///
/// For a repository holding several plugins in subdirectories, use
/// [`install_from_git_subdir`].
#[cfg(feature = "git")]
pub fn install_from_git(url: &str, rev: Option<&str>, dest_root: &Path) -> Result<LoadedManifest, PluginError> {
    clone_and_install(url, rev, None, dest_root)
}

/// Clone a git repository (optionally at `rev`, a branch or tag) and install the
/// plugin in its subdirectory `subdir` (for example `whatsapp-api` in a
/// monorepo with one plugin per directory). Requires the `git` executable.
///
/// `subdir` must be a relative path inside the repository: absolute paths,
/// `..` components and symlinks anywhere along the path are rejected. Only the
/// subdirectory's contents are installed, after the same manifest, name and
/// checksum checks as [`install_from_dir`].
///
/// # Errors
/// [`PluginError::Install`] for an unsafe or missing `subdir` or a failed
/// clone; any error of [`install_from_dir`] for the plugin itself.
///
/// # Example
/// ```no_run
/// # fn demo() -> Result<(), graphrt_plugin::PluginError> {
/// let dest = graphrt_plugin::default_install_dir().unwrap_or_default();
/// graphrt_plugin::install_from_git_subdir(
///     "https://github.com/BimaPangestu28/graphrt-plugins",
///     Some("main"),
///     "whatsapp-api",
///     &dest,
/// )?;
/// # Ok(()) }
/// ```
#[cfg(feature = "git")]
pub fn install_from_git_subdir(
    url: &str,
    rev: Option<&str>,
    subdir: &str,
    dest_root: &Path,
) -> Result<LoadedManifest, PluginError> {
    let relative = validate_subdir(subdir)?;
    clone_and_install(url, rev, Some(&relative), dest_root)
}

#[cfg(feature = "git")]
fn clone_and_install(
    url: &str,
    rev: Option<&str>,
    subdir: Option<&Path>,
    dest_root: &Path,
) -> Result<LoadedManifest, PluginError> {
    if url.starts_with('-') || rev.is_some_and(|r| r.starts_with('-')) {
        return Err(PluginError::Install("invalid git url or revision".into()));
    }
    let scratch = create_private_temp_dir()?;
    let checkout = scratch.join("repository");
    let result = clone_repository(url, rev, &checkout).and_then(|()| match subdir {
        Some(relative) => resolve_subdir(&checkout, relative).and_then(|dir| install_from_dir(&dir, dest_root)),
        None => install_from_dir(&checkout, dest_root),
    });
    let _cleanup = std::fs::remove_dir_all(&scratch);
    result
}

/// A new temporary directory, owner-only (`0700`) on Unix, to clone into.
/// `git clone` creates the checkout with the umask's mode, and its files are
/// checksum-verified and then copied, so no other local user may be able to
/// write there in between (a shared `/tmp` under a permissive umask).
#[cfg(feature = "git")]
fn create_private_temp_dir() -> Result<PathBuf, PluginError> {
    graphrt_core::private_fs::create_fresh_temp_dir("graphrt-plugin")
        .map_err(|e| PluginError::Io { path: std::env::temp_dir(), message: e.to_string() })
}

#[cfg(feature = "git")]
fn clone_repository(url: &str, rev: Option<&str>, checkout: &Path) -> Result<(), PluginError> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(["clone", "--depth", "1", "--quiet"]);
    if let Some(rev) = rev {
        cmd.args(["--branch", rev]);
    }
    cmd.arg("--").arg(url).arg(checkout);
    let output = cmd.output().map_err(|e| PluginError::Install(format!("cannot run git: {e}")))?;
    if !output.status.success() {
        return Err(PluginError::Install(format!(
            "git clone failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// Check that `subdir` is a non-empty relative path without `..`, and return it
/// with `.` components dropped.
#[cfg(feature = "git")]
fn validate_subdir(subdir: &str) -> Result<PathBuf, PluginError> {
    use std::path::Component;
    let invalid = |reason: &str| PluginError::Install(format!("invalid subdirectory `{subdir}`: {reason}"));
    let mut relative = PathBuf::new();
    for component in Path::new(subdir).components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            Component::ParentDir => return Err(invalid("`..` is not allowed")),
            Component::RootDir | Component::Prefix(_) => {
                return Err(invalid("must be a path relative to the repository root"));
            }
        }
    }
    if relative.as_os_str().is_empty() {
        return Err(invalid("must name a directory inside the repository"));
    }
    Ok(relative)
}

/// Walk `relative` inside `checkout`, requiring every component to be a real
/// directory (not a symlink), so the plugin cannot come from outside the clone.
#[cfg(feature = "git")]
fn resolve_subdir(checkout: &Path, relative: &Path) -> Result<PathBuf, PluginError> {
    let mut dir = checkout.to_path_buf();
    for part in relative.components() {
        dir.push(part);
        let shown = dir.strip_prefix(checkout).unwrap_or(&dir).display().to_string();
        let meta = std::fs::symlink_metadata(&dir)
            .map_err(|_| PluginError::Install(format!("subdirectory `{shown}` does not exist in the repository")))?;
        if meta.file_type().is_symlink() {
            return Err(PluginError::Install(format!("subdirectory `{shown}` is a symlink, which is not allowed")));
        }
        if !meta.is_dir() {
            return Err(PluginError::Install(format!("subdirectory `{shown}` is not a directory")));
        }
    }
    if manifest_in(&dir).is_none() {
        return Err(PluginError::Install(format!(
            "subdirectory `{}` has no plugin manifest (graphrt-plugin.json or .yaml)",
            relative.display()
        )));
    }
    Ok(dir)
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

    #[cfg(all(feature = "git", unix))]
    #[test]
    fn git_checkouts_go_into_an_owner_only_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = create_private_temp_dir().unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(mode & 0o777, 0o700);
    }

    /// Run `git` in `dir`, with an identity so commits work on any machine.
    #[cfg(feature = "git")]
    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(["-c", "user.name=test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(status.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&status.stderr));
    }

    #[cfg(feature = "git")]
    fn has_git() -> bool {
        std::process::Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success())
    }

    /// Commit everything in `repo` and return its `file://` URL (so `--depth` applies).
    #[cfg(feature = "git")]
    fn commit_repo(repo: &Path) -> String {
        git(repo, &["init", "--quiet"]);
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "--quiet", "-m", "fixture"]);
        git(repo, &["tag", "v1"]);
        format!("file://{}", repo.display())
    }

    #[cfg(feature = "git")]
    #[test]
    fn installs_plugins_from_monorepo_subdirectories() {
        if !has_git() {
            eprintln!("skipping: git is required");
            return;
        }
        let repo = temp("mono-repo");
        write_plugin(&repo.join("alpha"), "alpha", serde_json::json!({}));
        write_plugin(&repo.join("nested/beta"), "beta", serde_json::json!({}));
        std::fs::write(repo.join("README.md"), "collection").unwrap();
        let url = commit_repo(&repo);
        let dest = temp("mono-dest");

        let alpha = install_from_git_subdir(&url, Some("v1"), "alpha", &dest).unwrap();
        assert_eq!(alpha.manifest.name, "alpha");
        assert!(dest.join("alpha/run.sh").is_file());
        // Only the subdirectory is copied: nothing from the repository root.
        assert!(!dest.join("alpha/README.md").exists());
        assert!(!dest.join("alpha/nested").exists());

        let beta = install_from_git_subdir(&url, None, "./nested/beta/", &dest).unwrap();
        assert_eq!(beta.manifest.name, "beta");
        assert!(dest.join("beta/graphrt-plugin.json").is_file());
    }

    #[cfg(feature = "git")]
    #[test]
    fn root_install_from_git_is_unchanged() {
        if !has_git() {
            eprintln!("skipping: git is required");
            return;
        }
        let repo = temp("root-repo");
        write_plugin(&repo, "rooted", serde_json::json!({}));
        let url = commit_repo(&repo);
        let dest = temp("root-dest");
        let installed = install_from_git(&url, Some("v1"), &dest).unwrap();
        assert_eq!(installed.manifest.name, "rooted");
        assert!(dest.join("rooted/run.sh").is_file());
        assert!(!dest.join("rooted/.git").exists());
    }

    #[cfg(feature = "git")]
    #[test]
    fn unsafe_or_missing_subdirectories_are_rejected() {
        if !has_git() {
            eprintln!("skipping: git is required");
            return;
        }
        let outside = temp("mono-outside");
        write_plugin(&outside.join("evil"), "evil", serde_json::json!({}));
        let repo = temp("mono-bad-repo");
        write_plugin(&repo.join("good"), "good", serde_json::json!({}));
        std::fs::create_dir_all(repo.join("docs")).unwrap();
        std::fs::write(repo.join("docs/index.md"), "no manifest here").unwrap();
        std::fs::write(repo.join("file.txt"), "not a dir").unwrap();
        write_plugin(&repo.join("bad-sum"), "bad-sum", serde_json::json!({"run.sh": "00ff"}));
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.join("evil"), repo.join("escape")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("good", repo.join("alias")).unwrap();
        let url = commit_repo(&repo);
        let dest = temp("mono-bad-dest");

        let message = |subdir: &str| match install_from_git_subdir(&url, None, subdir, &dest) {
            Err(e) => e.to_string(),
            Ok(m) => format!("unexpectedly installed {}", m.manifest.name),
        };
        assert!(message("..").contains("`..` is not allowed"), "{}", message(".."));
        assert!(message("good/../../x").contains("`..` is not allowed"));
        assert!(message("/etc").contains("relative to the repository root"));
        assert!(message("").contains("must name a directory"));
        assert!(message(".").contains("must name a directory"));
        assert!(message("missing").contains("does not exist"));
        assert!(message("file.txt").contains("not a directory"));
        assert!(message("docs").contains("has no plugin manifest"));
        assert!(matches!(install_from_git_subdir(&url, None, "bad-sum", &dest), Err(PluginError::Checksum { .. })));
        #[cfg(unix)]
        {
            assert!(message("escape").contains("is a symlink"), "{}", message("escape"));
            assert!(message("alias").contains("is a symlink"));
        }
        // Nothing was installed by any rejected attempt.
        assert_eq!(std::fs::read_dir(&dest).unwrap().count(), 0);
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
