//! Agents larger than a Machine config carries ([`MAX_AGENT_FILE_BYTES`]),
//! typically bundles with their knowledge, go onto the deployment's store
//! volume. The Machine that mounts the volume runs `graphrt internal
//! store-file` through the Machines API `exec` (the graphrt image has no shell):
//! the agent arrives in pieces of [`UPLOAD_PIECE_BYTES`], hex-encoded on stdin,
//! at an explicit offset (a retried call never writes twice), and only becomes
//! the served file once its SHA-256 matches.
//!
//! Each agent gets a directory named after its SHA-256 under [`AGENTS_DIR`], so
//! the version that serves while a new one uploads keeps its own file, a deploy
//! that does not get healthy can go back to it, and a redeploy of the same
//! agent uploads nothing.
//!
//! [`MAX_AGENT_FILE_BYTES`]: super::MAX_AGENT_FILE_BYTES

use sha2::{Digest, Sha256};

use super::machine_config::{AGENTS_DIR, UPLOAD_PIECE_BYTES, volume_agent_file_name};
use super::{DeployRequest, DirectError, ErrorCode, FlyDirect};
use crate::model::MachineState;

/// Seconds Fly lets one `exec` call run.
const EXEC_TIMEOUT_SECS: u64 = 60;

/// The agent's place on the volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct VolumeAgent {
    /// Directory name under [`AGENTS_DIR`] (the first 16 hex digits of the SHA-256).
    pub(super) name: String,
    /// `<AGENTS_DIR>/<name>`, the directory `graphrt serve` serves.
    pub(super) dir: String,
    /// `<name>/<file>`, relative to [`AGENTS_DIR`].
    pub(super) relative_path: String,
    pub(super) sha256: String,
}

fn to_hex(bytes: &[u8]) -> String {
    let digit = |nibble: u8| char::from_digit(u32::from(nibble), 16).unwrap_or('0');
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(digit(byte >> 4));
        out.push(digit(byte & 0x0f));
    }
    out
}

/// Where `request`'s agent lives on the volume.
pub(super) fn volume_agent(request: &DeployRequest, workflow: &str) -> VolumeAgent {
    let sha256 = to_hex(&Sha256::digest(&request.agent.contents));
    let name = sha256.get(..16).unwrap_or(&sha256).to_owned();
    let relative_path = format!("{name}/{}", volume_agent_file_name(&request.agent, workflow));
    VolumeAgent { dir: format!("{AGENTS_DIR}/{name}"), name, relative_path, sha256 }
}

fn store_file_command(arguments: &[&str]) -> Vec<String> {
    ["graphrt", "internal", "store-file"].into_iter().chain(arguments.iter().copied()).map(str::to_owned).collect()
}

fn megabytes(bytes: usize) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}

impl FlyDirect {
    /// Runs one `graphrt internal store-file` step in the Machine; its stdout on success.
    async fn store_file(
        &self,
        machine_id: &str,
        arguments: &[&str],
        stdin: Option<&str>,
    ) -> Result<String, DirectError> {
        let command = store_file_command(arguments);
        let answer = self.client.exec_machine(machine_id, &command, stdin, EXEC_TIMEOUT_SECS).await?;
        if answer.exit_code != 0 {
            let detail = if answer.stderr.trim().is_empty() { answer.stdout.trim() } else { answer.stderr.trim() };
            return Err(DirectError::new(
                ErrorCode::FlyApi,
                format!(
                    "writing the agent onto the store volume failed ({}): {detail}. The Machine's image must come \
                     from this graphrt version or a newer one",
                    arguments.first().copied().unwrap_or("store-file")
                ),
            ));
        }
        Ok(answer.stdout.trim().to_owned())
    }

    /// Boots the Machine (its current config) when it is not running: `exec` needs a running Machine.
    async fn ensure_running(&self, machine_id: &str) -> Result<(), DirectError> {
        let machine = self.client.get_machine(machine_id).await?;
        if machine.state == MachineState::Started {
            return Ok(());
        }
        if machine.state.is_startable() {
            self.client.start_machine(machine_id).await?;
        }
        self.wait_started(machine_id).await
    }

    /// Grows the store volume to `size_gb` when it is smaller; restarts its Machine
    /// when Fly says the filesystem only sees the new size after a restart.
    pub(super) async fn ensure_volume_room(
        &self,
        volume_id: &str,
        current_gb: u32,
        size_gb: u32,
        machine_id: Option<&str>,
    ) -> Result<bool, DirectError> {
        if current_gb >= size_gb {
            return Ok(false);
        }
        let needs_restart = self.client.extend_volume(volume_id, size_gb).await?;
        if let (true, Some(machine_id)) = (needs_restart, machine_id) {
            let machine = self.client.get_machine(machine_id).await?;
            if machine.state == MachineState::Started {
                self.halt(machine_id).await?;
                self.client.start_machine(machine_id).await?;
                self.wait_started(machine_id).await?;
            }
        }
        Ok(true)
    }

    /// Puts the agent onto the volume of `machine_id` (unless it is there already).
    ///
    /// @param report - receives a line per uploaded piece
    /// @returns Where it is, and whether anything was uploaded
    pub(super) async fn upload_agent(
        &self,
        machine_id: &str,
        request: &DeployRequest,
        workflow: &str,
        report: &mut dyn FnMut(String),
    ) -> Result<(VolumeAgent, bool), DirectError> {
        let target = volume_agent(request, workflow);
        self.ensure_running(machine_id).await?;
        if self.store_file(machine_id, &["hash", &target.relative_path], None).await? == target.sha256 {
            return Ok((target, false));
        }
        self.store_file(machine_id, &["begin", &target.relative_path], None).await?;
        let total = request.agent.contents.len();
        let mut written = 0usize;
        for piece in request.agent.contents.chunks(UPLOAD_PIECE_BYTES) {
            let at = written.to_string();
            let hex = to_hex(piece);
            let size = self.store_file(machine_id, &["append", &target.relative_path, "--at", &at], Some(&hex)).await?;
            written += piece.len();
            if size != written.to_string() {
                return Err(DirectError::new(
                    ErrorCode::FlyApi,
                    format!("the store volume holds {size} bytes of the agent after {written} were sent"),
                ));
            }
            report(format!(
                "Uploading the agent onto the store volume: {} of {}",
                megabytes(written),
                megabytes(total)
            ));
        }
        self.store_file(machine_id, &["commit", &target.relative_path, &target.sha256], None).await?;
        Ok((target, true))
    }

    /// Deletes the agents on the volume other than `keep` (older versions). Best effort.
    pub(super) async fn prune_volume_agents(&self, machine_id: &str, keep: &[&str]) -> Result<String, DirectError> {
        let mut arguments = vec!["prune"];
        for name in keep {
            arguments.extend(["--keep", name]);
        }
        self.store_file(machine_id, &arguments, None).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_is_lowercase_and_two_digits_per_byte() {
        assert_eq!(to_hex(&[0x00, 0xab, 0xff]), "00abff");
    }
}
