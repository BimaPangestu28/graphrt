//! CLI errors and exit codes.

use std::fmt;
use std::process::ExitCode;

/// Exit code: the run succeeded.
pub const EXIT_OK: u8 = 0;
/// Exit code: the run failed.
pub const EXIT_FAILED: u8 = 1;
/// Exit code: invalid workflow or command-line usage.
pub const EXIT_INVALID: u8 = 2;
/// Exit code: the run paused (a snapshot was written).
pub const EXIT_PAUSED: u8 = 3;
/// Exit code: the run was aborted.
pub const EXIT_ABORTED: u8 = 4;

/// An error that ends the command with a specific exit code.
#[derive(Debug)]
pub struct CliError {
    pub code: u8,
    pub message: String,
}

impl CliError {
    /// Bad command-line usage or unreadable input files.
    pub fn usage(message: impl Into<String>) -> Self {
        Self { code: EXIT_INVALID, message: message.into() }
    }

    /// The workflow document is invalid.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self { code: EXIT_INVALID, message: message.into() }
    }

    /// A runtime failure (I/O while running, server errors...).
    pub fn failed(message: impl Into<String>) -> Self {
        Self { code: EXIT_FAILED, message: message.into() }
    }

    pub fn exit_code(&self) -> ExitCode {
        ExitCode::from(self.code)
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}
