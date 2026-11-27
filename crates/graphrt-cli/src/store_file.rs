//! `graphrt internal store-file`: writes an agent bundle onto the Machine's
//! volume in pieces. Direct deploys to Fly (graphrt-provider-fly) use it
//! through the Machines API `exec` for agents larger than a Machine config can
//! carry: the graphrt image has no shell, so the binary does the file work.
//!
//! Every path is a relative name under one root ([`ROOT_ENV`], default
//! [`DEFAULT_ROOT`]); `..`, absolute paths and empty names are refused. Pieces
//! arrive hex-encoded on stdin and are appended to `<path>.part`, which only
//! becomes `<path>` once its SHA-256 matches the one the deployer computed.

use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use clap::Subcommand;
use sha2::{Digest, Sha256};

use crate::error::CliError;

/// Environment variable naming the root directory (tests point it elsewhere).
pub const ROOT_ENV: &str = "GRAPHRT_STORE_FILE_ROOT";
/// Root on a direct deploy's store volume (`/data`, graphrt-provider-fly `STORE_PATH`).
pub const DEFAULT_ROOT: &str = "/data/agents";
/// Largest piece accepted in one `append` (hex text on stdin).
pub const MAX_PIECE_HEX_BYTES: usize = 16 * 1024 * 1024;

/// `graphrt internal` subcommands (not for people: used by deploy tooling).
#[derive(Debug, Subcommand)]
pub enum InternalCommand {
    /// Write a file onto this Machine's volume in pieces (direct deploys).
    #[command(subcommand)]
    StoreFile(StoreFileCommand),
}

/// Steps of writing one file.
#[derive(Debug, Subcommand)]
pub enum StoreFileCommand {
    /// Print the SHA-256 (hex) of the file, or `missing`.
    Hash { path: String },
    /// Start over: an empty `<path>.part`.
    Begin { path: String },
    /// Append the hex-encoded bytes read from stdin to `<path>.part` at byte `at`; print its size.
    /// A piece that is already there (a retried call) is not written twice.
    Append {
        path: String,
        #[arg(long)]
        at: u64,
    },
    /// Check `<path>.part` against `sha256` and move it to `<path>`; print `ok <bytes>`.
    Commit { path: String, sha256: String },
    /// Delete every entry of the root except the named ones (old versions).
    Prune {
        #[arg(long = "keep", value_name = "NAME")]
        keep: Vec<String>,
    },
}

/// Runs `command` under the root of [`ROOT_ENV`] and prints its one-line answer.
///
/// @throws CliError - unusable path, I/O failure, bad hex, or a hash mismatch
pub fn internal(command: InternalCommand) -> Result<u8, CliError> {
    let root = std::env::var_os(ROOT_ENV).map_or_else(|| PathBuf::from(DEFAULT_ROOT), PathBuf::from);
    let InternalCommand::StoreFile(command) = command;
    let mut stdin = std::io::stdin().lock();
    let answer = store_file(&root, command, &mut stdin)?;
    let mut out = std::io::stdout().lock();
    writeln!(out, "{answer}").map_err(|e| CliError::failed(e.to_string()))?;
    Ok(0)
}

fn io_error(what: &str, path: &Path, error: std::io::Error) -> CliError {
    CliError::failed(format!("{what} `{}`: {error}", path.display()))
}

/// `root/name` for a relative name made of plain components only.
fn resolve(root: &Path, name: &str) -> Result<PathBuf, CliError> {
    let relative = Path::new(name);
    let plain = !name.is_empty() && relative.components().all(|component| matches!(component, Component::Normal(_)));
    if !plain {
        return Err(CliError::usage(format!("`{name}` must be a relative path without `..`")));
    }
    Ok(root.join(relative))
}

fn part_of(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".part");
    PathBuf::from(name)
}

fn sha256_of(path: &Path) -> Result<(String, u64), CliError> {
    let mut file = std::fs::File::open(path).map_err(|e| io_error("cannot open", path, e))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let read = file.read(&mut buffer).map_err(|e| io_error("cannot read", path, e))?;
        if read == 0 {
            break;
        }
        hasher.update(buffer.get(..read).unwrap_or_default());
        total += read as u64;
    }
    Ok((crate::hex::to_hex(&hasher.finalize()), total))
}

/// One step; `input` is stdin (read by `append` only).
pub fn store_file(root: &Path, command: StoreFileCommand, input: &mut dyn Read) -> Result<String, CliError> {
    match command {
        StoreFileCommand::Hash { path } => {
            let path = resolve(root, &path)?;
            if !path.is_file() {
                return Ok("missing".to_owned());
            }
            Ok(sha256_of(&path)?.0)
        }
        StoreFileCommand::Begin { path } => {
            let path = resolve(root, &path)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| io_error("cannot create", parent, e))?;
            }
            let part = part_of(&path);
            std::fs::File::create(&part).map_err(|e| io_error("cannot create", &part, e))?;
            Ok("ok 0".to_owned())
        }
        StoreFileCommand::Append { path, at } => {
            let part = part_of(&resolve(root, &path)?);
            let mut hex = Vec::new();
            input
                .take(MAX_PIECE_HEX_BYTES as u64 + 1)
                .read_to_end(&mut hex)
                .map_err(|e| CliError::failed(format!("cannot read stdin: {e}")))?;
            if hex.len() > MAX_PIECE_HEX_BYTES {
                return Err(CliError::usage(format!("a piece is at most {MAX_PIECE_HEX_BYTES} hex characters")));
            }
            let text = std::str::from_utf8(&hex).map_err(|_| CliError::usage("the piece is not hex text"))?;
            let bytes =
                crate::hex::from_hex(text.trim()).ok_or_else(|| CliError::usage("the piece is not hex text"))?;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&part)
                .map_err(|e| io_error("cannot open (run `begin` first)", &part, e))?;
            let size = file.metadata().map_err(|e| io_error("cannot stat", &part, e))?.len();
            let end = at + bytes.len() as u64;
            if size == end {
                // The same piece again: the deployer retried a call that had already arrived.
                return Ok(size.to_string());
            }
            if size != at {
                return Err(CliError::failed(format!(
                    "`{}` holds {size} bytes, the piece starts at {at}: start the upload over",
                    part.display()
                )));
            }
            file.write_all(&bytes).map_err(|e| io_error("cannot write", &part, e))?;
            file.sync_data().map_err(|e| io_error("cannot sync", &part, e))?;
            Ok(end.to_string())
        }
        StoreFileCommand::Commit { path, sha256 } => {
            let path = resolve(root, &path)?;
            let part = part_of(&path);
            let (actual, size) = sha256_of(&part)?;
            if !actual.eq_ignore_ascii_case(sha256.trim()) {
                return Err(CliError::failed(format!(
                    "`{}` has SHA-256 {actual}, expected {}: the upload is incomplete or damaged",
                    part.display(),
                    sha256.trim()
                )));
            }
            std::fs::rename(&part, &path).map_err(|e| io_error("cannot move into place", &path, e))?;
            Ok(format!("ok {size}"))
        }
        StoreFileCommand::Prune { keep } => {
            for name in &keep {
                resolve(root, name)?;
            }
            let entries = match std::fs::read_dir(root) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok("ok 0".to_owned()),
                Err(error) => return Err(io_error("cannot read", root, error)),
            };
            let mut removed = 0;
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if keep.contains(&name) {
                    continue;
                }
                let path = entry.path();
                let result = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
                result.map_err(|e| io_error("cannot delete", &path, e))?;
                removed += 1;
            }
            Ok(format!("ok {removed}"))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn run(root: &Path, command: StoreFileCommand, stdin: &[u8]) -> Result<String, CliError> {
        store_file(root, command, &mut &stdin[..])
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("graphrt-store-file-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn writes_a_file_in_pieces_and_moves_it_into_place_when_the_hash_matches() {
        let root = scratch("pieces");
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let sha = crate::hex::to_hex(&Sha256::digest(&data));
        let path = || "abc123/agent.grt".to_owned();
        assert_eq!(run(&root, StoreFileCommand::Hash { path: path() }, b"").unwrap(), "missing");
        run(&root, StoreFileCommand::Begin { path: path() }, b"").unwrap();
        let mut size = String::new();
        for (index, piece) in data.chunks(100_000).enumerate() {
            let at = index as u64 * 100_000;
            let hex = crate::hex::to_hex(piece);
            size = run(&root, StoreFileCommand::Append { path: path(), at }, hex.as_bytes()).unwrap();
            // A retried call writes nothing more.
            assert_eq!(run(&root, StoreFileCommand::Append { path: path(), at }, hex.as_bytes()).unwrap(), size);
        }
        assert_eq!(size, "300000");
        let gap = run(&root, StoreFileCommand::Append { path: path(), at: 999 }, b"00").unwrap_err();
        assert!(gap.message.contains("start the upload over"), "{}", gap.message);
        assert_eq!(
            run(&root, StoreFileCommand::Commit { path: path(), sha256: sha.clone() }, b"").unwrap(),
            "ok 300000"
        );
        assert_eq!(std::fs::read(root.join("abc123/agent.grt")).unwrap(), data);
        assert_eq!(run(&root, StoreFileCommand::Hash { path: path() }, b"").unwrap(), sha);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_damaged_upload_is_never_moved_into_place() {
        let root = scratch("damaged");
        run(&root, StoreFileCommand::Begin { path: "v/a.grt".into() }, b"").unwrap();
        run(&root, StoreFileCommand::Append { path: "v/a.grt".into(), at: 0 }, b"00ff").unwrap();
        let wrong = "0".repeat(64);
        let error = run(&root, StoreFileCommand::Commit { path: "v/a.grt".into(), sha256: wrong }, b"").unwrap_err();
        assert!(error.message.contains("incomplete or damaged"), "{}", error.message);
        assert!(!root.join("v/a.grt").exists());
        let error = run(&root, StoreFileCommand::Append { path: "v/a.grt".into(), at: 2 }, b"not hex").unwrap_err();
        assert!(error.message.contains("hex"), "{}", error.message);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn paths_stay_under_the_root() {
        let root = scratch("paths");
        for bad in ["../etc/passwd", "/etc/passwd", "", "a/../../b"] {
            assert!(run(&root, StoreFileCommand::Begin { path: bad.into() }, b"").is_err(), "{bad}");
        }
        assert!(run(&root, StoreFileCommand::Prune { keep: vec!["../x".into()] }, b"").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prune_keeps_only_the_named_versions() {
        let root = scratch("prune");
        for name in ["old", "current", "previous"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        let answer = run(&root, StoreFileCommand::Prune { keep: vec!["current".into(), "previous".into()] }, b"");
        assert_eq!(answer.unwrap(), "ok 1");
        assert!(!root.join("old").exists() && root.join("current").exists());
        assert_eq!(run(&scratch("none"), StoreFileCommand::Prune { keep: vec![] }, b"").unwrap(), "ok 0");
        let _ = std::fs::remove_dir_all(&root);
    }
}
