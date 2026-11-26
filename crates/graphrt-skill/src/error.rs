//! Errors of the skill store.

use std::path::PathBuf;

use crate::scan::Finding;

/// Why a skill could not be read, installed or served.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A file or directory could not be read or written.
    #[error("{}: {message}", path.display())]
    Io {
        /// The path the operation failed on.
        path: PathBuf,
        /// The operating system's message.
        message: String,
    },
    /// `SKILL.md` is missing, has no frontmatter, or a required field is wrong.
    #[error("not a valid skill: {0}")]
    Format(String),
    /// The directory holds neither a `SKILL.md` nor an Agent Plugins package with skills.
    #[error("{}: no SKILL.md here and no plugin.json with a skills/ directory", .0.display())]
    NotASkill(PathBuf),
    /// A file path inside the package is not safe to install.
    #[error("unsafe path in the skill package: {0}")]
    UnsafePath(String),
    /// The package is larger than the store accepts.
    #[error("the skill package is too large: {0}")]
    TooLarge(String),
    /// No skill of that name is installed.
    #[error("skill `{0}` is not installed")]
    NotFound(String),
    /// A skill of that name is installed and replacing it was not asked for.
    #[error("skill `{0}` is already installed (replace it to update)")]
    AlreadyInstalled(String),
    /// The skill is installed but turned off.
    #[error("skill `{0}` is turned off")]
    Disabled(String),
    /// The scan found something that must be accepted before installing.
    #[error("skill `{skill}` has findings that must be accepted before it is installed: {}", summarize(.findings))]
    Blocked {
        /// The skill that was scanned.
        skill: String,
        /// Every finding of the scan, the blocking ones included.
        findings: Vec<Finding>,
    },
    /// A grant names a tool the skill does not ask for.
    #[error("skill `{skill}` does not ask for tool `{tool}`, so it cannot be granted")]
    GrantNotRequested {
        /// The skill.
        skill: String,
        /// The tool of the refused grant.
        tool: String,
    },
    /// A file changed on disk after the skill was installed.
    #[error(
        "skill `{skill}`: `{file}` changed on disk since the skill was approved. Install the skill again to review it"
    )]
    Tampered {
        /// The skill.
        skill: String,
        /// The file whose content no longer matches its pinned hash.
        file: String,
    },
    /// The skill has no such file.
    #[error("skill `{skill}` has no file `{file}`")]
    NoSuchFile {
        /// The skill.
        skill: String,
        /// The path that was asked for.
        file: String,
    },
    /// The file is not text.
    #[error("skill `{skill}`: `{file}` is not a text file")]
    NotText {
        /// The skill.
        skill: String,
        /// The binary file.
        file: String,
    },
    /// The store's index file cannot be understood.
    #[error("the skill index is damaged: {0}")]
    Index(String),
}

impl Error {
    pub(crate) fn io(path: impl Into<PathBuf>, error: &std::io::Error) -> Self {
        Self::Io { path: path.into(), message: error.to_string() }
    }
}

fn summarize(findings: &[Finding]) -> String {
    let blocking: Vec<&str> = findings.iter().filter(|f| f.is_blocking()).map(|f| f.message.as_str()).collect();
    blocking.join(" ")
}
