//! Workspace paths: the only way to name a file in a [`crate::FileStore`].
//!
//! A [`WorkspacePath`] is a relative path that has already been checked, so
//! code that receives one never has to think about traversal again. The rules
//! are the same on every operating system: a path a model writes on Linux
//! names the same file on Windows, or is refused on both.

use std::fmt;

/// Longest path, in bytes.
pub const MAX_PATH_BYTES: usize = 512;
/// Longest single component (file or directory name), in bytes.
pub const MAX_COMPONENT_BYTES: usize = 128;
/// Most components in one path (directory depth plus the file name).
pub const MAX_PATH_DEPTH: usize = 12;

/// Why a path was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PathError {
    #[error("the path is empty")]
    Empty,
    #[error("the path is longer than {MAX_PATH_BYTES} bytes")]
    TooLong,
    #[error("the path has more than {MAX_PATH_DEPTH} components")]
    TooDeep,
    #[error("absolute paths are not allowed (paths are relative to the workspace)")]
    Absolute,
    #[error("`..` and `.` components are not allowed")]
    Traversal,
    #[error("the path has an empty component (`//` or a trailing `/`)")]
    EmptyComponent,
    #[error("component `{0}` is longer than {MAX_COMPONENT_BYTES} bytes")]
    ComponentTooLong(String),
    #[error("character {0:?} is not allowed in a path")]
    ForbiddenCharacter(char),
    #[error("component `{0}` may not start with a dot")]
    Hidden(String),
    #[error("component `{0}` may not end with a dot or a space")]
    TrailingDotOrSpace(String),
    #[error("`{0}` is a reserved device name on Windows")]
    ReservedName(String),
}

/// A checked relative path inside a workspace, with `/` separators.
///
/// ```
/// use graphrt_files::WorkspacePath;
///
/// let path = WorkspacePath::parse("reports/2026/q3.csv")?;
/// assert_eq!(path.file_name(), "q3.csv");
/// assert_eq!(path.extension(), Some("csv"));
/// assert!(WorkspacePath::parse("../etc/passwd").is_err());
/// assert!(WorkspacePath::parse("/etc/passwd").is_err());
/// assert!(WorkspacePath::parse("C:\\Windows\\win.ini").is_err());
/// # Ok::<(), graphrt_files::PathError>(())
/// ```
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkspacePath(String);

impl WorkspacePath {
    /// Check `raw` and return it as a workspace path.
    ///
    /// Refused: empty paths, absolute paths, `.` and `..`, backslashes, drive
    /// letters and stream names (`:`), control and invisible characters,
    /// characters Windows forbids, hidden names (leading dot), names ending in
    /// a dot or a space, and Windows device names such as `CON` or `nul.txt`.
    pub fn parse(raw: &str) -> Result<Self, PathError> {
        if raw.is_empty() {
            return Err(PathError::Empty);
        }
        if raw.len() > MAX_PATH_BYTES {
            return Err(PathError::TooLong);
        }
        if raw.starts_with('/') {
            return Err(PathError::Absolute);
        }
        if let Some(forbidden) = raw.chars().find(|c| is_forbidden(*c)) {
            return Err(PathError::ForbiddenCharacter(forbidden));
        }
        let mut depth = 0usize;
        for component in raw.split('/') {
            check_component(component)?;
            depth += 1;
            if depth > MAX_PATH_DEPTH {
                return Err(PathError::TooDeep);
            }
        }
        Ok(Self(raw.to_owned()))
    }

    /// The path as text, with `/` separators.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The components, in order.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// The last component.
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// The extension of the file name, without the dot.
    pub fn extension(&self) -> Option<&str> {
        let (stem, extension) = self.file_name().rsplit_once('.')?;
        if stem.is_empty() || extension.is_empty() { None } else { Some(extension) }
    }

    /// The directory holding this path, or `None` at the workspace root.
    pub fn parent(&self) -> Option<WorkspacePath> {
        self.0.rsplit_once('/').map(|(parent, _)| Self(parent.to_owned()))
    }

    /// This path followed by `child` (which is checked like any other path).
    pub fn join(&self, child: &str) -> Result<WorkspacePath, PathError> {
        Self::parse(&format!("{}/{child}", self.0))
    }

    /// Whether this path is `ancestor` or lies below it.
    pub fn starts_with(&self, ancestor: &WorkspacePath) -> bool {
        self.0 == ancestor.0 || self.0.strip_prefix(ancestor.0.as_str()).is_some_and(|rest| rest.starts_with('/'))
    }

    /// The part of this path below `ancestor`, when it lies below it.
    pub fn strip_prefix(&self, ancestor: &WorkspacePath) -> Option<&str> {
        self.0.strip_prefix(ancestor.0.as_str())?.strip_prefix('/')
    }
}

impl std::str::FromStr for WorkspacePath {
    type Err = PathError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse(raw)
    }
}

impl fmt::Debug for WorkspacePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WorkspacePath({:?})", self.0)
    }
}

impl fmt::Display for WorkspacePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Characters that are never part of a path.
///
/// Backslash and colon would be a separator, a drive letter or an alternate
/// data stream on Windows. Control characters hide in logs. Bidirectional and
/// zero width characters make a name display as something else
/// (`report\u{202e}txt.exe` shows as `reportexe.txt`).
fn is_forbidden(c: char) -> bool {
    matches!(c, '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')
        || c.is_control()
        || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}')
}

fn check_component(component: &str) -> Result<(), PathError> {
    if component.is_empty() {
        return Err(PathError::EmptyComponent);
    }
    if component == "." || component == ".." {
        return Err(PathError::Traversal);
    }
    if component.len() > MAX_COMPONENT_BYTES {
        return Err(PathError::ComponentTooLong(component.to_owned()));
    }
    if component.starts_with('.') {
        return Err(PathError::Hidden(component.to_owned()));
    }
    if component.ends_with('.') || component.ends_with(' ') || component.starts_with(' ') {
        return Err(PathError::TrailingDotOrSpace(component.to_owned()));
    }
    if is_windows_device(component) {
        return Err(PathError::ReservedName(component.to_owned()));
    }
    Ok(())
}

/// Whether Windows would open a device for this name (`CON`, `nul.txt`, `COM1.log`...).
fn is_windows_device(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or(component).trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") {
        return true;
    }
    let numbered = |prefix: &str| {
        upper.strip_prefix(prefix).is_some_and(|rest| {
            matches!(rest, "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "\u{b9}" | "\u{b2}" | "\u{b3}")
        })
    };
    numbered("COM") || numbered("LPT")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(raw: &str) -> PathError {
        WorkspacePath::parse(raw).unwrap_err()
    }

    #[test]
    fn accepts_ordinary_relative_paths() {
        for raw in ["a.txt", "reports/2026/q3.csv", "Laporan Keuangan/ringkasan akhir.md", "データ/表.csv", "a-b_c.d.e"]
        {
            assert_eq!(WorkspacePath::parse(raw).unwrap().as_str(), raw);
        }
    }

    #[test]
    fn refuses_traversal_in_every_spelling() {
        assert_eq!(refused(".."), PathError::Traversal);
        assert_eq!(refused("../secret"), PathError::Traversal);
        assert_eq!(refused("a/../../secret"), PathError::Traversal);
        assert_eq!(refused("a/./b"), PathError::Traversal);
        assert_eq!(refused("a/.."), PathError::Traversal);
        // A backslash is a separator on Windows only, so it is refused everywhere.
        assert_eq!(refused("..\\secret"), PathError::ForbiddenCharacter('\\'));
        assert_eq!(refused("a\\..\\..\\secret"), PathError::ForbiddenCharacter('\\'));
        // Percent-encoded dots stay literal text: they are a plain (odd) file name, never `..`.
        assert_eq!(WorkspacePath::parse("%2e%2e/secret").unwrap().components().next(), Some("%2e%2e"));
    }

    #[test]
    fn refuses_absolute_paths_drives_and_streams() {
        assert_eq!(refused("/etc/passwd"), PathError::Absolute);
        assert_eq!(refused("//server/share/x"), PathError::Absolute);
        assert_eq!(refused("C:/Windows/win.ini"), PathError::ForbiddenCharacter(':'));
        assert_eq!(refused("C:\\Windows\\win.ini"), PathError::ForbiddenCharacter(':'));
        assert_eq!(refused("notes.txt:hidden"), PathError::ForbiddenCharacter(':'));
        assert_eq!(refused("\\\\?\\C:\\x"), PathError::ForbiddenCharacter('\\'));
    }

    #[test]
    fn refuses_control_and_invisible_characters() {
        assert_eq!(refused("a\0b"), PathError::ForbiddenCharacter('\0'));
        assert_eq!(refused("a\nb"), PathError::ForbiddenCharacter('\n'));
        assert_eq!(refused("report\u{202e}txt.exe"), PathError::ForbiddenCharacter('\u{202e}'));
        assert_eq!(refused("a\u{200b}b"), PathError::ForbiddenCharacter('\u{200b}'));
        assert_eq!(refused("what?.txt"), PathError::ForbiddenCharacter('?'));
    }

    #[test]
    fn refuses_hidden_names_trailing_dots_and_empty_components() {
        assert_eq!(refused(".env"), PathError::Hidden(".env".into()));
        assert_eq!(refused("a/.git/config"), PathError::Hidden(".git".into()));
        assert_eq!(refused("a."), PathError::TrailingDotOrSpace("a.".into()));
        assert_eq!(refused("a /b"), PathError::TrailingDotOrSpace("a ".into()));
        assert_eq!(refused("a//b"), PathError::EmptyComponent);
        assert_eq!(refused("a/"), PathError::EmptyComponent);
        assert_eq!(refused(""), PathError::Empty);
    }

    #[test]
    fn refuses_windows_device_names() {
        for raw in ["CON", "con", "nul.txt", "a/PRN", "COM1", "lpt9.log", "aux.tar.gz", "CONIN$"] {
            assert!(matches!(refused(raw), PathError::ReservedName(_)), "{raw}");
        }
        for raw in ["console.txt", "com10.txt", "nullable.md", "COM"] {
            assert!(WorkspacePath::parse(raw).is_ok(), "{raw}");
        }
    }

    #[test]
    fn refuses_oversized_paths() {
        assert_eq!(refused(&"a".repeat(MAX_PATH_BYTES + 1)), PathError::TooLong);
        assert!(matches!(refused(&"a".repeat(MAX_COMPONENT_BYTES + 1)), PathError::ComponentTooLong(_)));
        let deep = vec!["d"; MAX_PATH_DEPTH + 1].join("/");
        assert_eq!(refused(&deep), PathError::TooDeep);
        assert!(WorkspacePath::parse(&["d"; MAX_PATH_DEPTH].join("/")).is_ok());
    }

    #[test]
    fn parts_and_prefixes() {
        let path = WorkspacePath::parse("out/charts/sales.final.png").unwrap();
        assert_eq!(path.file_name(), "sales.final.png");
        assert_eq!(path.extension(), Some("png"));
        assert_eq!(path.parent().unwrap().as_str(), "out/charts");
        assert_eq!(WorkspacePath::parse("README").unwrap().extension(), None);
        assert!(WorkspacePath::parse("README").unwrap().parent().is_none());
        let out = WorkspacePath::parse("out").unwrap();
        assert!(path.starts_with(&out));
        assert_eq!(path.strip_prefix(&out), Some("charts/sales.final.png"));
        // `output` is not below `out`.
        assert!(!WorkspacePath::parse("output/x").unwrap().starts_with(&out));
        assert!(out.join("../x").is_err());
        assert_eq!(out.join("x.txt").unwrap().as_str(), "out/x.txt");
    }
}
