//! The minimal host environment given to child processes that do not inherit
//! the whole environment (plugin processes, stdio MCP servers).
//!
//! Lives in the core because both host crates that start such processes
//! (`graphrt-plugin`, `graphrt-mcp`) already depend on it and on nothing else
//! in common.

use std::ffi::OsString;

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
pub fn minimal_env<'a>(extra: impl IntoIterator<Item = &'a str>) -> impl Iterator<Item = (&'a str, OsString)> {
    DEFAULT_PASS_ENV.iter().copied().chain(extra).filter_map(|name| std::env::var_os(name).map(|value| (name, value)))
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
}
