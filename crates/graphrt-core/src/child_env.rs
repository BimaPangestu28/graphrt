//! The host environment given to child processes: the minimal one for
//! processes that do not inherit the whole environment (plugin processes,
//! stdio MCP servers), and the computer's own environment, without what an
//! AppImage launcher added, for every process graphrt starts.
//!
//! Lives in the core because both host crates that start such processes
//! (`graphrt-plugin`, `graphrt-mcp`) already depend on it and on nothing else
//! in common.
//!
//! # AppImage
//!
//! An application packaged as a Linux AppImage (e.g. the graphrt desktop app,
//! which embeds these crates) runs with the bundle's folders in front of
//! `LD_LIBRARY_PATH`, `PATH`, `PYTHONPATH`, `XDG_DATA_DIRS`, … and with
//! `PYTHONHOME`, `GTK_PATH`, `GIO_MODULE_DIR`, … pointing into the bundle (the
//! AppImage's `AppRun` and its GTK hook set them, without keeping the old
//! values). The application needs them; a program of the computer started with
//! them loads the bundle's older libraries and fails, e.g. ``/usr/bin/env:
//! …/libsystemd.so.0: version `LIBSYSTEMD_254' not found`` for a
//! `#!/usr/bin/env node` launcher on Ubuntu 26.04. So when the running binary
//! is inside `$APPDIR`, every entry inside `$APPDIR` is dropped for children: a
//! list variable keeps its other entries, a variable with nothing left is
//! removed, and the runtime's `APPIMAGE` / `ARGV0` / `OWD` are removed. Outside
//! an AppImage (and on macOS / Windows) nothing changes.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Host variables always passed to a child process that does not inherit the
/// environment.
///
/// - `PATH`, `HOME`, `LANG`, `TMPDIR`: what most launchers (`npx`, `uvx`,
///   `docker`, `sh`) need to start, find their tools and pick a temp dir.
/// - `SYSTEMROOT`, `USERPROFILE`, `APPDATA`, `TEMP`, `TMP`: what most Windows
///   programs need to start at all and to find their profile and temp space.
/// - `PATHEXT`: for the child's own command lookups (a shell or `node` that
///   resolves `npm` to `npm.cmd`). It does not affect how the host resolves
///   the child's program: Rust's `Command` ignores `PATHEXT`, so a `.cmd` or
///   `.bat` launcher must be configured with its extension.
pub const DEFAULT_PASS_ENV: &[&str] =
    &["PATH", "HOME", "LANG", "TMPDIR", "SYSTEMROOT", "USERPROFILE", "APPDATA", "TEMP", "TMP", "PATHEXT"];

/// The variables of [`DEFAULT_PASS_ENV`] plus `extra` that are set on the host,
/// with their values, for a child started after `env_clear()`.
///
/// ```
/// let mut command = std::process::Command::new("true");
/// command.env_clear().envs(graphrt_core::child_env::minimal_env(["MY_TOKEN"]));
/// ```
///
/// Values are read with [`host_var`], so inside an AppImage `PATH` does not
/// start with the bundle's folders.
pub fn minimal_env<'a>(extra: impl IntoIterator<Item = &'a str>) -> impl Iterator<Item = (&'a str, OsString)> {
    DEFAULT_PASS_ENV.iter().copied().chain(extra).filter_map(|name| host_var(name).map(|value| (name, value)))
}

/// Set by the AppImage runtime; meaningless (or misleading) for other programs.
const APPIMAGE_RUNTIME_VARS: &[&str] = &["APPIMAGE", "ARGV0", "OWD"];

/// One change that gives a child the computer's own environment back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvFix {
    /// Set the variable to this value (the original without the bundle's entries).
    Set(OsString, OsString),
    /// Remove the variable (it only pointed into the bundle).
    Remove(OsString),
}

impl EnvFix {
    /// The variable this fix is about.
    pub fn name(&self) -> &OsStr {
        match self {
            EnvFix::Set(name, _) | EnvFix::Remove(name) => name,
        }
    }
}

/// The fixes for an environment `vars` of an application running from the
/// AppImage mounted at `appdir` (see the [module docs](self)), sorted by name.
///
/// ```
/// use graphrt_core::child_env::{appimage_env_fixes, EnvFix};
/// let vars = [("LD_LIBRARY_PATH".into(), "/tmp/.mount_x/usr/lib:/opt/lib".into())];
/// let fixes = appimage_env_fixes(vars, std::path::Path::new("/tmp/.mount_x"));
/// # #[cfg(unix)]
/// assert_eq!(fixes, [EnvFix::Set("LD_LIBRARY_PATH".into(), "/opt/lib".into())]);
/// ```
pub fn appimage_env_fixes(vars: impl IntoIterator<Item = (OsString, OsString)>, appdir: &Path) -> Vec<EnvFix> {
    let mut fixes: Vec<EnvFix> = vars
        .into_iter()
        .filter_map(|(name, value)| {
            if APPIMAGE_RUNTIME_VARS.iter().any(|runtime_var| OsStr::new(runtime_var) == name) {
                return Some(EnvFix::Remove(name));
            }
            let entries = split_entries(&name, &value);
            if !entries.iter().any(|entry| is_inside(entry, appdir)) {
                return None;
            }
            let kept: Vec<&OsStr> =
                entries.into_iter().filter(|entry| !entry.is_empty() && !is_inside(entry, appdir)).collect();
            if kept.is_empty() {
                return Some(EnvFix::Remove(name));
            }
            Some(EnvFix::Set(name, join_entries(&kept)))
        })
        .collect();
    fixes.sort_by(|left, right| left.name().cmp(right.name()));
    fixes
}

/// A value's `:`-separated entries (`LD_PRELOAD` also splits on spaces, like the loader).
#[cfg(unix)]
fn split_entries<'a>(name: &OsStr, value: &'a OsStr) -> Vec<&'a OsStr> {
    use std::os::unix::ffi::OsStrExt;
    let is_separator = |byte: &u8| *byte == b':' || (name == "LD_PRELOAD" && byte.is_ascii_whitespace());
    value.as_bytes().split(is_separator).map(OsStr::from_bytes).collect()
}

/// AppImages are Linux-only; elsewhere a value is one entry.
#[cfg(not(unix))]
fn split_entries<'a>(_name: &OsStr, value: &'a OsStr) -> Vec<&'a OsStr> {
    vec![value]
}

fn join_entries(entries: &[&OsStr]) -> OsString {
    let mut joined = OsString::new();
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            joined.push(":");
        }
        joined.push(entry);
    }
    joined
}

/// Whether `entry` is an absolute path in `appdir` (`$APPDIR//usr/lib` included).
fn is_inside(entry: &OsStr, appdir: &Path) -> bool {
    let entry = Path::new(entry);
    entry.is_absolute() && entry.starts_with(appdir)
}

/// The AppImage folder the application runs from: `$APPDIR`, but only when the
/// running binary is inside it (a stray `APPDIR` in the user's shell changes nothing).
fn running_appdir(appdir_var: Option<OsString>, current_exe: Option<PathBuf>) -> Option<PathBuf> {
    let appdir = PathBuf::from(appdir_var?);
    let is_mounted_bundle = appdir.is_absolute() && appdir.parent().is_some() && current_exe?.starts_with(&appdir);
    is_mounted_bundle.then_some(appdir)
}

/// The fixes for this process, worked out once from the environment at the
/// first call (empty outside a Linux AppImage).
pub fn host_env_fixes() -> &'static [EnvFix] {
    static FIXES: OnceLock<Vec<EnvFix>> = OnceLock::new();
    FIXES.get_or_init(|| {
        if !cfg!(target_os = "linux") {
            return Vec::new();
        }
        match running_appdir(std::env::var_os("APPDIR"), std::env::current_exe().ok()) {
            Some(appdir) => appimage_env_fixes(std::env::vars_os(), &appdir),
            None => Vec::new(),
        }
    })
}

/// `name` as a child gets it: the process value with [`host_env_fixes`] applied.
pub fn host_var(name: &str) -> Option<OsString> {
    fixed_var(name, host_env_fixes())
}

fn fixed_var(name: &str, fixes: &[EnvFix]) -> Option<OsString> {
    match fixes.iter().find(|fix| fix.name() == name) {
        Some(EnvFix::Set(_, value)) => Some(value.clone()),
        Some(EnvFix::Remove(_)) => None,
        None => std::env::var_os(name),
    }
}

/// Sets up the environment of a child process the way plugin processes and
/// stdio MCP servers get it: the whole host environment when `inherit`, else
/// only [`DEFAULT_PASS_ENV`] plus `pass_env`; either way as the computer has
/// it, without an AppImage's entries ([`host_env_fixes`]). Call it before the
/// command's own `env` calls. A Tokio command passes `command.as_std_mut()`.
pub fn set_child_env<'a>(
    command: &mut std::process::Command,
    inherit: bool,
    pass_env: impl IntoIterator<Item = &'a str>,
) {
    set_child_env_with(command, inherit, pass_env, host_env_fixes());
}

/// [`set_child_env`] with explicit `fixes` instead of this process's ones (tests,
/// or an embedder that works them out itself).
pub fn set_child_env_with<'a>(
    command: &mut std::process::Command,
    inherit: bool,
    pass_env: impl IntoIterator<Item = &'a str>,
    fixes: &[EnvFix],
) {
    if inherit {
        apply_env_fixes(command, fixes);
        return;
    }
    command.env_clear();
    for name in DEFAULT_PASS_ENV.iter().copied().chain(pass_env) {
        if let Some(value) = fixed_var(name, fixes) {
            command.env(name, value);
        }
    }
}

/// The whole environment as a child gets it (for a command started after `env_clear()`).
pub fn host_vars() -> Vec<(OsString, OsString)> {
    let fixes = host_env_fixes();
    std::env::vars_os()
        .filter_map(|(name, value)| match fixes.iter().find(|fix| fix.name() == name.as_os_str()) {
            Some(EnvFix::Set(_, fixed)) => Some((name, fixed.clone())),
            Some(EnvFix::Remove(_)) => None,
            None => Some((name, value)),
        })
        .collect()
}

/// Gives a command that inherits the environment the computer's own one: call
/// it before the command's own `env` calls. A Tokio command passes
/// `command.as_std_mut()`.
///
/// ```
/// let mut command = std::process::Command::new("git");
/// graphrt_core::child_env::apply_host_env(&mut command);
/// command.arg("--version");
/// ```
pub fn apply_host_env(command: &mut std::process::Command) {
    apply_env_fixes(command, host_env_fixes());
}

/// [`apply_host_env`] with explicit `fixes` (tests, or an embedder that works them out itself).
pub fn apply_env_fixes(command: &mut std::process::Command, fixes: &[EnvFix]) {
    for fix in fixes {
        match fix {
            EnvFix::Set(name, value) => command.env(name, value),
            EnvFix::Remove(name) => command.env_remove(name),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_set_variables_are_passed() {
        let names: Vec<&str> = minimal_env(["GRAPHRT_CHILD_ENV_SURELY_UNSET"]).map(|(name, _)| name).collect();
        assert!(!names.contains(&"GRAPHRT_CHILD_ENV_SURELY_UNSET"));
        assert!(names.iter().all(|name| DEFAULT_PASS_ENV.contains(name)));
        if std::env::var_os("PATH").is_some() {
            assert!(names.contains(&"PATH"));
        }
    }

    const APPDIR: &str = "/tmp/.mount_graphremPBnj";

    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs.iter().map(|(name, value)| (OsString::from(name), OsString::from(value))).collect()
    }

    fn set(name: &str, value: &str) -> EnvFix {
        EnvFix::Set(name.into(), value.into())
    }

    fn remove(name: &str) -> EnvFix {
        EnvFix::Remove(name.into())
    }

    #[cfg(unix)]
    #[test]
    fn drops_bundle_entries_from_lists_and_keeps_the_rest() {
        let env = vars(&[
            ("PATH", "/tmp/.mount_graphremPBnj/usr/bin/:/tmp/.mount_graphremPBnj/usr/sbin/:/usr/local/bin:/usr/bin"),
            (
                "XDG_DATA_DIRS",
                "/tmp/.mount_graphremPBnj/usr/share/:/tmp/.mount_graphremPBnj/usr/share:/usr/share:/var/lib/flatpak/exports/share",
            ),
            ("LD_LIBRARY_PATH", "/tmp/.mount_graphremPBnj/usr/lib/:/tmp/.mount_graphremPBnj/lib64/:/opt/cuda/lib64"),
        ]);
        assert_eq!(
            appimage_env_fixes(env, Path::new(APPDIR)),
            [
                set("LD_LIBRARY_PATH", "/opt/cuda/lib64"),
                set("PATH", "/usr/local/bin:/usr/bin"),
                set("XDG_DATA_DIRS", "/usr/share:/var/lib/flatpak/exports/share"),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn removes_variables_left_empty_and_single_bundle_paths() {
        // AppRun ends each list with the old value, which is empty when it was unset.
        let env = vars(&[
            ("LD_LIBRARY_PATH", "/tmp/.mount_graphremPBnj/usr/lib/:/tmp/.mount_graphremPBnj/usr/lib64/:"),
            ("PYTHONHOME", "/tmp/.mount_graphremPBnj/usr/"),
            ("GIO_MODULE_DIR", "/tmp/.mount_graphremPBnj//usr/lib/gio/modules"),
            ("APPDIR", APPDIR),
        ]);
        assert_eq!(
            appimage_env_fixes(env, Path::new(APPDIR)),
            [remove("APPDIR"), remove("GIO_MODULE_DIR"), remove("LD_LIBRARY_PATH"), remove("PYTHONHOME")]
        );
    }

    #[test]
    fn removes_the_appimage_runtime_variables() {
        let env =
            vars(&[("APPIMAGE", "/home/me/graphrt.AppImage"), ("ARGV0", "./graphrt.AppImage"), ("OWD", "/home/me")]);
        assert_eq!(appimage_env_fixes(env, Path::new(APPDIR)), [remove("APPIMAGE"), remove("ARGV0"), remove("OWD")]);
    }

    #[test]
    fn leaves_everything_else_alone() {
        let env = vars(&[
            ("HOME", "/home/me"),
            ("PATH", "/usr/local/bin:/usr/bin"),
            ("GTK_THEME", "Adwaita:dark"),
            // Same prefix, different folder.
            ("SOME_DIR", "/tmp/.mount_graphremPBnjX/usr/lib"),
            // Relative entries never count as the bundle.
            ("CDPATH", "tmp/.mount_graphremPBnj:."),
            ("URL", "https://example.com:8443/tmp/.mount_graphremPBnj"),
        ]);
        assert!(appimage_env_fixes(env, Path::new(APPDIR)).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn ld_preload_splits_on_spaces_too() {
        let env = vars(&[("LD_PRELOAD", "/tmp/.mount_graphremPBnj/usr/lib/libhack.so /usr/lib/libgamemodeauto.so.0")]);
        assert_eq!(appimage_env_fixes(env, Path::new(APPDIR)), [set("LD_PRELOAD", "/usr/lib/libgamemodeauto.so.0")]);
    }

    #[test]
    fn appdir_counts_only_when_the_binary_runs_from_it() {
        let exe_inside = Some(PathBuf::from("/tmp/.mount_graphremPBnj/usr/bin/graphrt"));
        assert_eq!(running_appdir(Some(APPDIR.into()), exe_inside.clone()), Some(PathBuf::from(APPDIR)));
        assert_eq!(running_appdir(Some(APPDIR.into()), Some(PathBuf::from("/usr/bin/graphrt"))), None);
        assert_eq!(running_appdir(None, exe_inside.clone()), None);
        assert_eq!(running_appdir(Some("relative/dir".into()), exe_inside.clone()), None);
        assert_eq!(running_appdir(Some("/".into()), exe_inside), None);
        assert_eq!(running_appdir(Some(APPDIR.into()), None), None);
    }

    #[test]
    fn apply_sets_and_removes_on_the_command() {
        let mut command = std::process::Command::new("true");
        apply_env_fixes(&mut command, &[set("PATH", "/usr/bin"), remove("LD_LIBRARY_PATH")]);
        let envs: Vec<(&OsStr, Option<&OsStr>)> = command.get_envs().collect();
        assert_eq!(envs, [(OsStr::new("LD_LIBRARY_PATH"), None), (OsStr::new("PATH"), Some(OsStr::new("/usr/bin")))]);
    }

    /// The variables a command sets (`None`: removed), by name.
    fn command_env(command: &std::process::Command) -> Vec<(String, Option<String>)> {
        command
            .get_envs()
            .map(|(name, value)| {
                (name.to_string_lossy().into_owned(), value.map(|value| value.to_string_lossy().into_owned()))
            })
            .collect()
    }

    #[test]
    fn an_inheriting_child_gets_the_fixes() {
        let mut command = std::process::Command::new("true");
        set_child_env_with(&mut command, true, [], &[remove("LD_LIBRARY_PATH"), set("PATH", "/usr/bin")]);
        assert_eq!(command_env(&command), [("LD_LIBRARY_PATH".into(), None), ("PATH".into(), Some("/usr/bin".into()))]);
    }

    #[test]
    fn a_minimal_child_gets_fixed_values_and_nothing_removed() {
        let mut command = std::process::Command::new("true");
        // `HOME` is passed by default; a fix removing it wins over the host value.
        set_child_env_with(
            &mut command,
            false,
            ["GRAPHRT_CHILD_ENV_SURELY_UNSET"],
            &[set("PATH", "/usr/bin"), remove("HOME")],
        );
        let env = command_env(&command);
        assert!(env.contains(&("PATH".into(), Some("/usr/bin".into()))));
        assert!(env.iter().all(|(name, _)| name != "HOME" && name != "GRAPHRT_CHILD_ENV_SURELY_UNSET"));
        assert!(env.iter().all(|(name, _)| DEFAULT_PASS_ENV.contains(&name.as_str())));
    }

    #[test]
    fn nothing_changes_outside_an_appimage() {
        // The test runner is not an AppImage.
        assert!(host_env_fixes().is_empty());
        assert_eq!(host_var("HOME"), std::env::var_os("HOME"));
        let mut command = std::process::Command::new("true");
        apply_host_env(&mut command);
        assert_eq!(command.get_envs().count(), 0);
    }
}
