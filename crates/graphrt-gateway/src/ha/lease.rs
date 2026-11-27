//! The lease of active/passive gateways on a shared volume: who *should* be
//! active. Safety does not depend on it (that is the store fence,
//! [`crate::ha::fence`]); it decides liveness: how fast a passive takes over.
//!
//! Files in the lease directory (next to the store):
//!
//! | File | Role |
//! | --- | --- |
//! | `graphrt-gateway.lease` | JSON [`LeaseRecord`]: token, holder, heartbeat counter. Replaced atomically (write + rename) |
//! | `graphrt-gateway.lease.lock` | `flock` serialising read-modify-write of the lease (held for one write + fsync + rename: milliseconds) |
//! | `graphrt-gateway.active.lock` | `flock` held by the active for its whole tenure: the kernel drops it when the process dies, and a dropped [`FileLease`] unlocks it |
//! | `graphrt-gateway.promote` | manual promotion request (`graphrt gateway ha promote`) |
//! | `graphrt-gateway.beat` | the active's liveness [`Mark`]: its token, a counter and a checksum (24 bytes), overwritten in place every beat ([`MarkFile`]) |
//!
//! A passive ([`Candidate`]) takes the lease when:
//!
//! 1. there is none, or it was released (graceful shutdown): at once;
//! 2. the holder held the tenure lock and it is now free (the holder process
//!    is gone, `kill -9` included): at once;
//! 3. a promotion was requested for it: at once (the holder is killed first
//!    when it provably still runs on this host);
//! 4. `takeover: auto` and neither the holder's heartbeat counter nor its
//!    liveness mark moved for `stale_after` on the *passive's* monotonic
//!    clock (frozen or partitioned holder; audit D1): same as 3. This also holds when the lease lock
//!    stays taken for `stale_after` (the holder froze inside a heartbeat):
//!    the passive then decides without the lock.
//!
//! Liveness and the durable lease are written apart. Replacing the lease
//! (create, write, fsync, rename) can stall for seconds on a busy volume or
//! under a virus scanner, and a holder stuck there is alive: it must not look
//! frozen. The mark is a fixed size record overwritten in place, with no new
//! file, no rename and no fsync on the way, so it keeps moving while a lease
//! write is stuck. A holder that writes no mark (an older version) is judged
//! by its counter alone, as before.
//!
//! Every take bumps the token. The active ([`Holder`]) increments the
//! heartbeat counter every beat and steps down as soon as the lease names
//! another token.

use std::fs::File;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Lease file name.
pub const LEASE_FILE: &str = "graphrt-gateway.lease";
/// Read-modify-write lock file name.
pub const MUTEX_FILE: &str = "graphrt-gateway.lease.lock";
/// Tenure lock file name.
pub const TENURE_FILE: &str = "graphrt-gateway.active.lock";
/// Manual promotion request file name.
pub const PROMOTE_FILE: &str = "graphrt-gateway.promote";
/// Liveness mark file name.
pub const BEAT_FILE: &str = "graphrt-gateway.beat";

/// Who a gateway process is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// Node id (`ha.node_id`, default `<host>-<pid>`).
    pub node: String,
    /// Host name (a holder is only killed by a passive on the same host).
    pub host: String,
    /// Process id.
    pub pid: u32,
}

/// The content of the lease file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseRecord {
    /// Fencing token of the holder; strictly increasing across takes.
    pub token: u64,
    /// Holder node id.
    pub node: String,
    /// Holder host.
    pub host: String,
    /// Holder pid.
    pub pid: u32,
    /// Heartbeat counter.
    pub seq: u64,
    /// Whether the holder holds the tenure lock.
    pub tenure: bool,
    /// Set by a graceful shutdown: the next passive takes over at once.
    pub released: bool,
    /// Wall clock of the last write (informational only, never compared).
    pub updated_unix_ms: u64,
}

impl LeaseRecord {
    /// Whether this lease names `identity` with `token`.
    pub fn is_held_by(&self, identity: &Identity, token: u64) -> bool {
        self.token == token && self.node == identity.node && self.pid == identity.pid
    }
}

/// Where the lease lives: files on a volume ([`FileLease`]) or a simulation.
pub trait LeaseMedium {
    /// Tries to take the read-modify-write lock.
    ///
    /// # Errors
    /// I/O errors of the volume.
    fn try_lock_mutex(&mut self) -> io::Result<bool>;
    /// Releases the read-modify-write lock.
    fn unlock_mutex(&mut self);
    /// Reads the lease (`None` when absent).
    ///
    /// # Errors
    /// I/O errors; [`io::ErrorKind::InvalidData`] for an unreadable file.
    fn read(&mut self) -> io::Result<Option<LeaseRecord>>;
    /// Replaces the lease atomically.
    ///
    /// # Errors
    /// I/O errors.
    fn write(&mut self, record: &LeaseRecord) -> io::Result<()>;
    /// Tries to take the tenure lock (true when held after the call).
    ///
    /// # Errors
    /// I/O errors.
    fn try_tenure(&mut self) -> io::Result<bool>;
    /// Releases the tenure lock.
    fn release_tenure(&mut self);
    /// Consumes a promotion request addressed to `node` (or to any node).
    ///
    /// # Errors
    /// I/O errors.
    fn take_promote_request(&mut self, node: &str) -> io::Result<bool>;
    /// Wall clock, informational.
    fn unix_ms(&self) -> u64;
    /// Reads the holder's liveness mark: `None` when there is none, or when
    /// the read caught the record mid write.
    ///
    /// # Errors
    /// I/O errors.
    fn read_mark(&mut self) -> io::Result<Option<Mark>> {
        Ok(None)
    }
}

/// The active's liveness mark: its token and a counter that moves every beat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    /// Token of the holder that wrote it.
    pub token: u64,
    /// Counter of its beats.
    pub seq: u64,
}

/// Bytes of an encoded [`Mark`] (token, counter, checksum, little endian):
/// always the same, so a write in place replaces the whole record.
const MARK_LEN: usize = 24;

impl Mark {
    /// A checksum, so that a read that caught a write halfway is not taken for a mark.
    fn check(self) -> u64 {
        (self.token.rotate_left(17) ^ self.seq).wrapping_mul(0x9E37_79B9_7F4A_7C15)
    }

    fn encode(self) -> [u8; MARK_LEN] {
        let mut bytes = [0; MARK_LEN];
        for (word, value) in bytes.as_chunks_mut::<8>().0.iter_mut().zip([self.token, self.seq, self.check()]) {
            *word = value.to_le_bytes();
        }
        bytes
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let ([token, seq, check], []) = bytes.as_chunks::<8>() else { return None };
        let mark = Self { token: u64::from_le_bytes(*token), seq: u64::from_le_bytes(*seq) };
        (mark.check() == u64::from_le_bytes(*check)).then_some(mark)
    }
}

/// The active's side of the liveness mark: one file, kept open and
/// overwritten in place.
#[derive(Debug)]
pub struct MarkFile {
    file: File,
}

impl MarkFile {
    /// Opens (creating, emptying) the mark file in `dir`.
    ///
    /// # Errors
    /// When it cannot be created.
    pub fn open(dir: &Path) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new().create(true).truncate(true).write(true).open(dir.join(BEAT_FILE))?;
        Ok(Self { file })
    }

    /// Overwrites the mark. No fsync, so it never waits on the volume: a
    /// passive on this host (the only one that may kill the holder) reads it
    /// from the page cache at once, one elsewhere when its view of the volume
    /// catches up. The lease counter stays the durable signal for those.
    ///
    /// # Errors
    /// I/O errors.
    pub fn write(&mut self, mark: Mark) -> io::Result<()> {
        use std::io::{Seek as _, SeekFrom};
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&mark.encode())
    }
}

fn read_mark(path: &Path) -> io::Result<Option<Mark>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Mark::decode(&bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// When a passive may take over a live-looking holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Takeover {
    /// Also when the heartbeat is stale (frozen / partitioned holder).
    #[default]
    Auto,
    /// Only on a vacant / released lease, an exited holder or `ha promote`.
    Manual,
}

/// Lease timing.
#[derive(Debug, Clone, Copy)]
pub struct LeaseSettings {
    /// Takeover policy.
    pub takeover: Takeover,
    /// Unchanged heartbeat duration making a holder stale (`auto`).
    pub stale_after_ms: u64,
    /// Kill a stale holder that provably runs on this host before taking over.
    pub kill_stale_holder: bool,
}

/// Why a passive took the lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TakeReason {
    /// No lease yet.
    Vacant,
    /// The holder released it.
    Released,
    /// The holder process is gone (tenure lock free).
    HolderExited,
    /// `graphrt gateway ha promote`.
    Requested,
    /// The heartbeat stopped for `stale_after`.
    Stale,
}

impl TakeReason {
    /// Stable name for logs and metrics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vacant => "vacant",
            Self::Released => "released",
            Self::HolderExited => "holder_exited",
            Self::Requested => "requested",
            Self::Stale => "stale",
        }
    }
}

/// A successful take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Promotion {
    /// New token.
    pub token: u64,
    /// Whether the tenure lock is held.
    pub tenure: bool,
    /// Why.
    pub reason: TakeReason,
    /// Pid of a stale holder on this host to kill before serving.
    pub kill: Option<u32>,
    /// The lease before the take.
    pub previous: Option<LeaseRecord>,
}

#[derive(Debug, Clone, Copy)]
struct Observation {
    token: u64,
    seq: u64,
    /// The counter of the holder's mark, when it has one of this token.
    mark: Option<u64>,
    since_ms: u64,
}

/// A passive gateway waiting for the lease.
#[derive(Debug)]
pub struct Candidate {
    identity: Identity,
    settings: LeaseSettings,
    observed: Option<Observation>,
    /// Start of the current run of polls that found the lease lock taken.
    lock_busy_since: Option<u64>,
}

impl Candidate {
    /// A candidate.
    pub fn new(identity: Identity, settings: LeaseSettings) -> Self {
        Self { identity, settings, observed: None, lock_busy_since: None }
    }

    /// Who this is.
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// One poll at monotonic time `now_ms`: the promotion, or `None` to wait.
    ///
    /// # Errors
    /// I/O errors of the volume (the caller logs and polls again).
    ///
    /// The lease lock is held for one read-modify-write (a few ms: write +
    /// fsync + rename). When it stays taken for `stale_after`, its holder was
    /// frozen (`SIGSTOP`, VM pause) inside that window and keeps the lock
    /// until it dies; waiting for the lock would then never displace it. The
    /// candidate decides without the lock instead (the lease file is replaced
    /// atomically, so the read is whole): the rules are the same, and the
    /// store fence still orders the tokens if the frozen process ever
    /// completes its write.
    pub fn step(&mut self, medium: &mut dyn LeaseMedium, now_ms: u64) -> io::Result<Option<Promotion>> {
        if medium.try_lock_mutex()? {
            self.lock_busy_since = None;
            let result = self.take_if_due(medium, now_ms, None);
            medium.unlock_mutex();
            return result;
        }
        let busy_since = *self.lock_busy_since.get_or_insert(now_ms);
        if now_ms.saturating_sub(busy_since) < self.settings.stale_after_ms {
            // Keep watching the mark meanwhile: one that moved just before a
            // freeze must not restart the count when the decision comes.
            if let Ok(Some(record)) = medium.read() {
                self.stale_for(medium, &record, now_ms, Some(busy_since));
            }
            return Ok(None);
        }
        let promotion = self.take_if_due(medium, now_ms, Some(busy_since))?;
        if promotion.is_some() {
            self.lock_busy_since = None;
        }
        Ok(promotion)
    }

    /// How long neither the holder's heartbeat counter nor its mark moved.
    /// `quiet_since` is when the lease lock became busy: no heartbeat counter
    /// can be written without it.
    fn stale_for(
        &mut self,
        medium: &mut dyn LeaseMedium,
        record: &LeaseRecord,
        now_ms: u64,
        quiet_since: Option<u64>,
    ) -> u64 {
        // A mark that cannot be read, or of another token, says nothing.
        let mark = medium.read_mark().ok().flatten().filter(|mark| mark.token == record.token).map(|mark| mark.seq);
        match self.observed {
            // No mark (vanished, or caught mid write) is no news: the clock runs on.
            Some(seen)
                if seen.token == record.token && seen.seq == record.seq && (mark.is_none() || seen.mark == mark) =>
            {
                now_ms.saturating_sub(seen.since_ms)
            }
            _ => {
                // The mark moves while the lock is busy: the holder is alive, only its lease write is slow.
                let since_ms = if mark.is_some() { now_ms } else { quiet_since.unwrap_or(now_ms) };
                self.observed = Some(Observation { token: record.token, seq: record.seq, mark, since_ms });
                now_ms.saturating_sub(since_ms)
            }
        }
    }

    fn decide(
        &mut self,
        medium: &mut dyn LeaseMedium,
        current: Option<&LeaseRecord>,
        now_ms: u64,
        quiet_since: Option<u64>,
    ) -> io::Result<Option<(TakeReason, bool)>> {
        let Some(record) = current else { return Ok(Some((TakeReason::Vacant, false))) };
        if record.released {
            return Ok(Some((TakeReason::Released, false)));
        }
        if record.tenure && medium.try_tenure()? {
            return Ok(Some((TakeReason::HolderExited, true)));
        }
        let stale_for = self.stale_for(medium, record, now_ms, quiet_since);
        if medium.take_promote_request(&self.identity.node)? {
            return Ok(Some((TakeReason::Requested, false)));
        }
        if self.settings.takeover == Takeover::Auto && stale_for >= self.settings.stale_after_ms {
            return Ok(Some((TakeReason::Stale, false)));
        }
        Ok(None)
    }

    fn take_if_due(
        &mut self,
        medium: &mut dyn LeaseMedium,
        now_ms: u64,
        quiet_since: Option<u64>,
    ) -> io::Result<Option<Promotion>> {
        let current = match medium.read() {
            Ok(current) => current,
            // An unreadable lease is vacant: the store fence still orders the tokens.
            Err(error) if error.kind() == io::ErrorKind::InvalidData => None,
            Err(error) => return Err(error),
        };
        let Some((reason, mut tenure)) = self.decide(medium, current.as_ref(), now_ms, quiet_since)? else {
            return Ok(None);
        };
        if !tenure {
            tenure = medium.try_tenure()?;
        }
        // Killing is only safe when the holder provably still runs here: it
        // held the tenure lock, the lock is still taken, and it is our host.
        let kill = current
            .as_ref()
            .filter(|record| {
                self.settings.kill_stale_holder
                    && matches!(reason, TakeReason::Stale | TakeReason::Requested)
                    && record.tenure
                    && !tenure
                    && record.host == self.identity.host
                    && record.pid != self.identity.pid
            })
            .map(|record| record.pid);
        let token = current.as_ref().map_or(0, |record| record.token) + 1;
        let record = LeaseRecord {
            token,
            node: self.identity.node.clone(),
            host: self.identity.host.clone(),
            pid: self.identity.pid,
            seq: 0,
            tenure,
            released: false,
            updated_unix_ms: medium.unix_ms(),
        };
        if let Err(error) = medium.write(&record) {
            if tenure {
                medium.release_tenure();
            }
            return Err(error);
        }
        self.observed = None;
        Ok(Some(Promotion { token, tenure, reason, kill, previous: current }))
    }
}

/// Result of a heartbeat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Beat {
    /// The lease is still ours and the counter moved.
    Confirmed,
    /// The volume could not be written (or the lock was busy): not confirmed.
    Unconfirmed(String),
    /// Another process holds the lease (or it vanished): step down.
    Lost(Option<LeaseRecord>),
}

/// The active gateway's side of the lease.
#[derive(Debug)]
pub struct Holder {
    identity: Identity,
    token: u64,
    seq: u64,
    tenure: bool,
}

impl Holder {
    /// The holder after `promotion`.
    pub fn new(identity: Identity, promotion: &Promotion) -> Self {
        Self { identity, token: promotion.token, seq: 0, tenure: promotion.tenure }
    }

    /// Current token.
    pub fn token(&self) -> u64 {
        self.token
    }

    /// Whether the tenure lock is held.
    pub fn has_tenure(&self) -> bool {
        self.tenure
    }

    /// Who holds the lease.
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    fn record(&self, medium: &dyn LeaseMedium, released: bool) -> LeaseRecord {
        LeaseRecord {
            token: self.token,
            node: self.identity.node.clone(),
            host: self.identity.host.clone(),
            pid: self.identity.pid,
            seq: self.seq,
            tenure: self.tenure && !released,
            released,
            updated_unix_ms: medium.unix_ms(),
        }
    }

    /// One heartbeat.
    pub fn beat(&mut self, medium: &mut dyn LeaseMedium) -> Beat {
        match medium.try_lock_mutex() {
            Ok(true) => {}
            Ok(false) => return Beat::Unconfirmed("lease lock busy".to_owned()),
            Err(error) => return Beat::Unconfirmed(error.to_string()),
        }
        let beat = self.beat_locked(medium);
        medium.unlock_mutex();
        beat
    }

    fn beat_locked(&mut self, medium: &mut dyn LeaseMedium) -> Beat {
        let current = match medium.read() {
            Ok(current) => current,
            Err(error) if error.kind() == io::ErrorKind::InvalidData => None,
            Err(error) => return Beat::Unconfirmed(error.to_string()),
        };
        match current {
            Some(record) if record.is_held_by(&self.identity, self.token) => {}
            other => return Beat::Lost(other),
        }
        if !self.tenure {
            self.tenure = medium.try_tenure().unwrap_or(false);
        }
        self.seq += 1;
        match medium.write(&self.record(medium, false)) {
            Ok(()) => Beat::Confirmed,
            Err(error) => Beat::Unconfirmed(error.to_string()),
        }
    }

    /// Moves the lease to `token` if it is still ours (the store was ahead of
    /// the lease file). Returns whether it did.
    ///
    /// # Errors
    /// I/O errors.
    pub fn raise_token(&mut self, medium: &mut dyn LeaseMedium, token: u64) -> io::Result<bool> {
        if !medium.try_lock_mutex()? {
            return Ok(false);
        }
        let result = (|| {
            let ours = medium.read()?.is_some_and(|record| record.is_held_by(&self.identity, self.token));
            if !ours || token <= self.token {
                return Ok(false);
            }
            self.token = token;
            medium.write(&self.record(medium, false))?;
            Ok(true)
        })();
        medium.unlock_mutex();
        result
    }

    /// Gives the lease up (graceful shutdown) so a passive takes over at once.
    /// Returns whether the lease was still ours.
    ///
    /// # Errors
    /// I/O errors (the tenure lock is released anyway).
    pub fn release(&mut self, medium: &mut dyn LeaseMedium) -> io::Result<bool> {
        let mut locked = false;
        for _ in 0..50 {
            if medium.try_lock_mutex()? {
                locked = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let result = if locked {
            let outcome = (|| {
                let ours = medium.read()?.is_some_and(|record| record.is_held_by(&self.identity, self.token));
                if ours {
                    medium.write(&self.record(medium, true))?;
                }
                Ok(ours)
            })();
            medium.unlock_mutex();
            outcome
        } else {
            Ok(false)
        };
        medium.release_tenure();
        self.tenure = false;
        result
    }
}

/// The lease as files in a directory of the shared volume.
#[derive(Debug)]
pub struct FileLease {
    dir: PathBuf,
    temp_name: String,
    mutex: File,
    tenure: File,
    mutex_held: bool,
    tenure_held: bool,
}

fn lock_file(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(path)
}

fn try_flock(file: &File) -> io::Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(std::fs::TryLockError::WouldBlock) => Ok(false),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

impl FileLease {
    /// Opens (creating) the lease files in `dir` for node `node`.
    ///
    /// # Errors
    /// When the directory or the lock files cannot be created.
    pub fn open(dir: &Path, node: &str) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let safe: String =
            node.chars().map(|character| if character.is_ascii_alphanumeric() { character } else { '_' }).collect();
        Ok(Self {
            dir: dir.to_path_buf(),
            temp_name: format!(".{LEASE_FILE}.{safe}.{}.tmp", std::process::id()),
            mutex: lock_file(&dir.join(MUTEX_FILE))?,
            tenure: lock_file(&dir.join(TENURE_FILE))?,
            mutex_held: false,
            tenure_held: false,
        })
    }

    /// Reads the lease of `dir` without locking (status).
    ///
    /// # Errors
    /// I/O errors and unreadable files.
    pub fn peek(dir: &Path) -> io::Result<Option<LeaseRecord>> {
        read_lease(&dir.join(LEASE_FILE))
    }

    /// Asks the passive `node` (any passive when `None`) to take over.
    ///
    /// # Errors
    /// When the request cannot be written.
    pub fn request_promotion(dir: &Path, node: Option<&str>) -> io::Result<()> {
        let temporary = dir.join(format!(".{PROMOTE_FILE}.{}.tmp", std::process::id()));
        std::fs::write(&temporary, node.unwrap_or("*"))?;
        std::fs::rename(temporary, dir.join(PROMOTE_FILE))
    }

    /// Whether some process holds the tenure lock of `dir` right now.
    ///
    /// # Errors
    /// I/O errors.
    pub fn tenure_held(dir: &Path) -> io::Result<bool> {
        let file = lock_file(&dir.join(TENURE_FILE))?;
        let free = try_flock(&file)?;
        if free {
            file.unlock()?;
        }
        Ok(!free)
    }
}

fn read_lease(path: &Path) -> io::Result<Option<LeaseRecord>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).map(Some).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

impl LeaseMedium for FileLease {
    fn try_lock_mutex(&mut self) -> io::Result<bool> {
        self.mutex_held = try_flock(&self.mutex)?;
        Ok(self.mutex_held)
    }

    fn unlock_mutex(&mut self) {
        if self.mutex_held {
            let _unlocked = self.mutex.unlock();
            self.mutex_held = false;
        }
    }

    fn read(&mut self) -> io::Result<Option<LeaseRecord>> {
        read_lease(&self.dir.join(LEASE_FILE))
    }

    fn write(&mut self, record: &LeaseRecord) -> io::Result<()> {
        let temporary = self.dir.join(&self.temp_name);
        let bytes = serde_json::to_vec(record).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        {
            let mut file = File::create(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_data()?;
        }
        std::fs::rename(&temporary, self.dir.join(LEASE_FILE))
    }

    fn try_tenure(&mut self) -> io::Result<bool> {
        if !self.tenure_held {
            self.tenure_held = try_flock(&self.tenure)?;
        }
        Ok(self.tenure_held)
    }

    fn release_tenure(&mut self) {
        if self.tenure_held {
            let _unlocked = self.tenure.unlock();
            self.tenure_held = false;
        }
    }

    fn take_promote_request(&mut self, node: &str) -> io::Result<bool> {
        let path = self.dir.join(PROMOTE_FILE);
        let target = match std::fs::read_to_string(&path) {
            Ok(text) => text.trim().to_owned(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        if !(target.is_empty() || target == "*" || target == node) {
            return Ok(false);
        }
        std::fs::remove_file(path)?;
        Ok(true)
    }

    fn unix_ms(&self) -> u64 {
        graphrt_core::store::now_ms()
    }

    fn read_mark(&mut self) -> io::Result<Option<Mark>> {
        read_mark(&self.dir.join(BEAT_FILE))
    }
}

/// Unlocks instead of leaving it to the close of the lock files. A `flock`
/// belongs to the open file description, and a child process forked by
/// another thread shares that description until it execs: closing our
/// descriptors in that window would leave the locks taken, and a passive
/// polling right then would see a holder that is gone as still running.
impl Drop for FileLease {
    fn drop(&mut self) {
        self.unlock_mutex();
        self.release_tenure();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(node: &str, pid: u32) -> Identity {
        Identity { node: node.into(), host: "h1".into(), pid }
    }

    fn settings(takeover: Takeover) -> LeaseSettings {
        LeaseSettings { takeover, stale_after_ms: 5_000, kill_stale_holder: true }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("graphrt-gw-lease-{tag}-{}", std::process::id()));
        let _cleanup = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn exited_holder_is_replaced_at_once_and_steps_down() {
        let dir = temp_dir("exit");
        let mut first = FileLease::open(&dir, "a").unwrap();
        let mut candidate_a = Candidate::new(identity("a", 1), settings(Takeover::Manual));
        let promotion = candidate_a.step(&mut first, 0).unwrap().unwrap();
        assert_eq!((promotion.token, promotion.reason, promotion.tenure), (1, TakeReason::Vacant, true));
        let mut holder = Holder::new(identity("a", 1), &promotion);
        assert_eq!(holder.beat(&mut first), Beat::Confirmed);

        let mut second = FileLease::open(&dir, "b").unwrap();
        let mut candidate_b = Candidate::new(identity("b", 2), settings(Takeover::Manual));
        assert_eq!(candidate_b.step(&mut second, 0).unwrap(), None, "a live holder keeps the lease");
        drop(first); // the process exits: the kernel drops its locks
        let taken = candidate_b.step(&mut second, 1).unwrap().unwrap();
        assert_eq!((taken.token, taken.reason, taken.tenure, taken.kill), (2, TakeReason::HolderExited, true, None));

        let mut resumed = FileLease::open(&dir, "a").unwrap();
        assert!(matches!(holder.beat(&mut resumed), Beat::Lost(Some(record)) if record.token == 2));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stale_holder_is_stolen_only_in_auto_mode_or_on_request() {
        let dir = temp_dir("stale");
        let mut holder_medium = FileLease::open(&dir, "a").unwrap();
        let promotion =
            Candidate::new(identity("a", 1), settings(Takeover::Auto)).step(&mut holder_medium, 0).unwrap().unwrap();
        let _holder = Holder::new(identity("a", 1), &promotion);

        let mut medium = FileLease::open(&dir, "b").unwrap();
        let mut manual = Candidate::new(identity("b", 2), settings(Takeover::Manual));
        assert_eq!(manual.step(&mut medium, 0).unwrap(), None);
        assert_eq!(manual.step(&mut medium, 60_000).unwrap(), None, "manual never steals on staleness");
        let mut auto = Candidate::new(identity("c", 3), settings(Takeover::Auto));
        assert_eq!(auto.step(&mut medium, 0).unwrap(), None);
        assert_eq!(auto.step(&mut medium, 4_999).unwrap(), None);
        let stolen = auto.step(&mut medium, 5_000).unwrap().unwrap();
        assert_eq!((stolen.reason, stolen.tenure, stolen.kill), (TakeReason::Stale, false, Some(1)));

        FileLease::request_promotion(&dir, Some("b")).unwrap();
        let requested = manual.step(&mut medium, 60_001).unwrap().unwrap();
        assert_eq!((requested.token, requested.reason), (3, TakeReason::Requested));
        assert!(!dir.join(PROMOTE_FILE).exists(), "the request is consumed");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn holder_frozen_inside_the_lease_lock_is_still_displaced() {
        // SIGSTOP between `flock(lease.lock)` and its release (the beat's
        // write + fsync): the lock stays taken until the holder dies.
        let dir = temp_dir("frozen-lock");
        let mut holder_medium = FileLease::open(&dir, "a").unwrap();
        let promotion =
            Candidate::new(identity("a", 1), settings(Takeover::Auto)).step(&mut holder_medium, 0).unwrap().unwrap();
        let mut holder = Holder::new(identity("a", 1), &promotion);
        assert_eq!(holder.beat(&mut holder_medium), Beat::Confirmed);
        assert!(holder_medium.try_lock_mutex().unwrap(), "frozen mid-beat");

        let mut medium = FileLease::open(&dir, "b").unwrap();
        let mut auto = Candidate::new(identity("b", 2), settings(Takeover::Auto));
        assert_eq!(auto.step(&mut medium, 1_000).unwrap(), None, "a busy lock is normal for one beat");
        assert_eq!(auto.step(&mut medium, 5_999).unwrap(), None);
        let stolen = auto.step(&mut medium, 6_000).unwrap().unwrap();
        assert_eq!((stolen.token, stolen.reason, stolen.kill), (2, TakeReason::Stale, Some(1)));
        assert_eq!(FileLease::peek(&dir).unwrap().unwrap().node, "b");

        // Manual mode: `ha promote` still goes through the frozen lock.
        let mut manual = Candidate::new(identity("c", 3), settings(Takeover::Manual));
        assert_eq!(manual.step(&mut medium, 10_000).unwrap(), None);
        assert_eq!(manual.step(&mut medium, 60_000).unwrap(), None, "manual never steals on staleness");
        FileLease::request_promotion(&dir, Some("c")).unwrap();
        let requested = manual.step(&mut medium, 60_001).unwrap().unwrap();
        assert_eq!((requested.token, requested.reason), (3, TakeReason::Requested));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_mark_reads_back_and_a_torn_one_is_no_mark() {
        let mark = Mark { token: 7, seq: 123_456 };
        let bytes = mark.encode();
        assert_eq!(bytes.len(), MARK_LEN);
        assert_eq!(Mark::decode(&bytes), Some(mark));
        assert_eq!(Mark::decode(&bytes[..16]), None, "cut short");
        assert_eq!(Mark::decode(&[bytes.as_slice(), &[0]].concat()), None, "too long");
        let mut torn = bytes;
        torn[..8].copy_from_slice(&Mark { token: 8, seq: 1 }.encode()[..8]);
        assert_eq!(Mark::decode(&torn), None, "half of a newer mark");
        assert_eq!(Mark::decode(b""), None);
    }

    /// The failure seen on Windows CI: the active's lease write stalled for
    /// seconds inside the lease lock while the process was alive and serving,
    /// and a passive took it for frozen and killed it.
    #[test]
    fn a_holder_stuck_in_a_lease_write_keeps_the_lease_while_its_mark_moves() {
        let dir = temp_dir("slow-write");
        let mut holder_medium = FileLease::open(&dir, "a").unwrap();
        let promotion =
            Candidate::new(identity("a", 1), settings(Takeover::Auto)).step(&mut holder_medium, 0).unwrap().unwrap();
        let mut holder = Holder::new(identity("a", 1), &promotion);
        assert_eq!(holder.beat(&mut holder_medium), Beat::Confirmed);
        let mut mark = MarkFile::open(&dir).unwrap();
        mark.write(Mark { token: promotion.token, seq: 1 }).unwrap();

        let mut medium = FileLease::open(&dir, "b").unwrap();
        let mut auto = Candidate::new(identity("b", 2), settings(Takeover::Auto));
        assert_eq!(auto.step(&mut medium, 0).unwrap(), None);
        // The lease write is stuck: the lock stays taken and the counter does not move.
        assert!(holder_medium.try_lock_mutex().unwrap());
        for (seq, now) in (2..).zip((500..=20_000).step_by(500)) {
            mark.write(Mark { token: promotion.token, seq }).unwrap();
            assert_eq!(auto.step(&mut medium, now).unwrap(), None, "a live holder at {now} ms");
        }
        // Then the holder freezes for real: the mark stops too, and it is displaced.
        assert_eq!(auto.step(&mut medium, 24_999).unwrap(), None);
        let stolen = auto.step(&mut medium, 25_000).unwrap().unwrap();
        assert_eq!((stolen.token, stolen.reason, stolen.kill), (2, TakeReason::Stale, Some(1)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A holder that moved its mark and then froze inside the lease lock is
    /// displaced `stale_after` after the freeze, not twice that.
    #[test]
    fn a_mark_moved_just_before_a_freeze_in_the_lock_does_not_delay_the_takeover() {
        let dir = temp_dir("mark-then-freeze");
        let mut holder_medium = FileLease::open(&dir, "a").unwrap();
        let promotion =
            Candidate::new(identity("a", 1), settings(Takeover::Auto)).step(&mut holder_medium, 0).unwrap().unwrap();
        let mut mark = MarkFile::open(&dir).unwrap();
        mark.write(Mark { token: promotion.token, seq: 1 }).unwrap();
        let mut medium = FileLease::open(&dir, "b").unwrap();
        let mut auto = Candidate::new(identity("b", 2), settings(Takeover::Auto));
        assert_eq!(auto.step(&mut medium, 0).unwrap(), None);
        // The last mark, then the freeze inside a heartbeat.
        mark.write(Mark { token: promotion.token, seq: 2 }).unwrap();
        assert!(holder_medium.try_lock_mutex().unwrap());
        for now in (500..5_500).step_by(500) {
            assert_eq!(auto.step(&mut medium, now).unwrap(), None);
        }
        let stolen = auto.step(&mut medium, 5_500).unwrap().unwrap();
        assert_eq!((stolen.reason, stolen.kill), (TakeReason::Stale, Some(1)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_mark_of_another_token_or_a_torn_one_does_not_keep_a_holder() {
        let dir = temp_dir("old-mark");
        let mut holder_medium = FileLease::open(&dir, "a").unwrap();
        let promotion =
            Candidate::new(identity("a", 1), settings(Takeover::Auto)).step(&mut holder_medium, 0).unwrap().unwrap();
        let mut mark = MarkFile::open(&dir).unwrap();
        let mut medium = FileLease::open(&dir, "b").unwrap();
        let mut auto = Candidate::new(identity("b", 2), settings(Takeover::Auto));
        assert_eq!(auto.step(&mut medium, 0).unwrap(), None);
        // A resumed old holder writes marks of its old token: they say nothing of this one.
        for (seq, now) in (1..).zip((1_000..=4_000).step_by(1_000)) {
            mark.write(Mark { token: promotion.token + 5, seq }).unwrap();
            assert_eq!(auto.step(&mut medium, now).unwrap(), None);
        }
        std::fs::write(dir.join(BEAT_FILE), b"garbage").unwrap();
        let stolen = auto.step(&mut medium, 5_000).unwrap().unwrap();
        assert_eq!(stolen.reason, TakeReason::Stale);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn frozen_lock_holder_that_left_frees_the_tenure_and_the_lease() {
        // A frozen passive holds the lease lock; the active stepped down and exited.
        let dir = temp_dir("frozen-passive");
        let mut active_medium = FileLease::open(&dir, "a").unwrap();
        Candidate::new(identity("a", 1), settings(Takeover::Manual)).step(&mut active_medium, 0).unwrap().unwrap();
        let mut frozen = FileLease::open(&dir, "f").unwrap();
        assert!(frozen.try_lock_mutex().unwrap());
        drop(active_medium);
        let mut medium = FileLease::open(&dir, "b").unwrap();
        let mut manual = Candidate::new(identity("b", 2), settings(Takeover::Manual));
        assert_eq!(manual.step(&mut medium, 0).unwrap(), None);
        let taken = manual.step(&mut medium, 5_000).unwrap().unwrap();
        assert_eq!((taken.token, taken.reason, taken.tenure, taken.kill), (2, TakeReason::HolderExited, true, None));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn release_hands_over_at_once() {
        let dir = temp_dir("release");
        let mut medium_a = FileLease::open(&dir, "a").unwrap();
        let promotion =
            Candidate::new(identity("a", 1), settings(Takeover::Manual)).step(&mut medium_a, 0).unwrap().unwrap();
        let mut holder = Holder::new(identity("a", 1), &promotion);
        assert!(holder.raise_token(&mut medium_a, 9).unwrap());
        assert_eq!(FileLease::peek(&dir).unwrap().unwrap().token, 9);
        assert!(holder.release(&mut medium_a).unwrap());
        assert!(!FileLease::tenure_held(&dir).unwrap());
        let mut medium_b = FileLease::open(&dir, "b").unwrap();
        let taken =
            Candidate::new(identity("b", 2), settings(Takeover::Manual)).step(&mut medium_b, 0).unwrap().unwrap();
        assert_eq!((taken.token, taken.reason, taken.tenure), (10, TakeReason::Released, true));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A process spawned by another thread inherits the lock descriptors
    /// until it execs. Closing them alone would then leave the locks taken
    /// for that window, so a dropped lease must unlock them itself.
    #[cfg(unix)]
    #[test]
    fn dropped_lease_frees_its_locks_while_other_threads_spawn_processes() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let stop = Arc::new(AtomicBool::new(false));
        let spawners: Vec<_> = (0..4)
            .map(|_| {
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let _status = std::process::Command::new("true").status();
                    }
                })
            })
            .collect();
        let dir = temp_dir("drop-spawn");
        let mut still_locked = 0;
        for _ in 0..300 {
            // Each round drops the lease of the round before, locks held.
            let mut lease = FileLease::open(&dir, "a").unwrap();
            if !(lease.try_tenure().unwrap() && lease.try_lock_mutex().unwrap()) {
                still_locked += 1;
            }
        }
        stop.store(true, Ordering::Relaxed);
        for spawner in spawners {
            spawner.join().unwrap();
        }
        assert_eq!(still_locked, 0, "rounds where a dropped lease kept a lock");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
