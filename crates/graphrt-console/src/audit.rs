//! The audit log: who did what and when, append only.
//!
//! One JSON line per entry, in one file per UTC day (`audit/YYYY-MM-DD.jsonl`),
//! so retention deletes whole days. Each entry carries the hash of the one
//! before it (`prev`) and its own (`hash`, SHA-256 of the entry without it):
//! an edited or removed line breaks the chain, which [`AuditLog::verify`]
//! finds. Appends take an exclusive file lock, so servers that share the
//! directory write one chain.

use std::fs::File;
use std::io::{BufRead as _, BufReader, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{ConsoleError, Result};
use crate::util::{MS_PER_DAY, ensure_dir, now_ms, sha256_hex, utc_date};

/// The `prev` of the very first entry.
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";
/// The most entries one query answers.
pub const MAX_PAGE: usize = 1_000;
/// The file whose lock serializes appends across processes.
const LOCK_FILE: &str = "append.lock";
/// The longest line read back (a longer one is not an entry of this log).
const MAX_LINE_BYTES: usize = 64 * 1024;

/// Who did it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    /// `user` (signed in), `api-key`, `device` or `system`.
    pub kind: String,
    /// The subject of a user, the id of a device; empty for the API key.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// The role the user had then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

impl Actor {
    /// The holder of the API key.
    pub fn api_key() -> Self {
        Self { kind: "api-key".into(), id: String::new(), email: None, role: Some("owner".into()) }
    }

    /// The server itself (retention, startup).
    pub fn system() -> Self {
        Self { kind: "system".into(), id: String::new(), email: None, role: None }
    }
}

/// What happened, before it is written.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub actor: Actor,
    /// E.g. `auth.sign_in`, `secret.set`, `registry.promote`, `approval.resolve`.
    pub action: String,
    /// What it was done to: a workflow, a secret name, a run id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// `ok`, `denied` or `failed`.
    pub outcome: String,
    /// The HTTP status of the request, when it was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// More about it (never a secret value).
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub details: serde_json::Value,
}

impl Event {
    /// A successful `action` of `actor`.
    pub fn ok(actor: Actor, action: &str) -> Self {
        Self {
            actor,
            action: action.to_owned(),
            target: None,
            outcome: "ok".into(),
            status: None,
            details: serde_json::Value::Null,
        }
    }

    pub fn target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
    }

    pub fn details(mut self, details: serde_json::Value) -> Self {
        self.details = details;
        self
    }

    pub fn outcome(mut self, outcome: &str) -> Self {
        outcome.clone_into(&mut self.outcome);
        self
    }

    /// The outcome and status of an HTTP answer.
    pub fn status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self.outcome = match status {
            401 | 403 => "denied",
            s if s >= 400 => "failed",
            _ => "ok",
        }
        .into();
        self
    }
}

/// An entry of the log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// 1, 2, 3... across every file of the log.
    pub seq: u64,
    pub at_ms: u64,
    #[serde(flatten)]
    pub event: Event,
    /// `hash` of the entry before.
    pub prev: String,
    /// SHA-256 of this entry with `hash` empty.
    #[serde(default)]
    pub hash: String,
}

impl Entry {
    fn computed_hash(&self) -> Result<String> {
        let mut unhashed = self.clone();
        unhashed.hash = String::new();
        let bytes = serde_json::to_vec(&unhashed).map_err(|error| ConsoleError::Io(error.to_string()))?;
        Ok(sha256_hex(&bytes))
    }
}

/// What a query asks for. Every field narrows it.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Query {
    /// Entries at or after this time.
    pub since_ms: Option<u64>,
    /// Entries before this time.
    pub until_ms: Option<u64>,
    /// The actor's id or email, exactly.
    pub actor: Option<String>,
    /// The action, or with a trailing `.` every action of that group (`secret.`).
    pub action: Option<String>,
    /// The target, exactly.
    pub target: Option<String>,
    /// `ok`, `denied` or `failed`.
    pub outcome: Option<String>,
    /// Entries before this `seq` (the cursor of the previous page).
    pub before_seq: Option<u64>,
    /// At most this many (default 100, at most [`MAX_PAGE`]).
    pub limit: Option<usize>,
}

impl Query {
    fn matches(&self, entry: &Entry) -> bool {
        let event = &entry.event;
        self.since_ms.is_none_or(|since| entry.at_ms >= since)
            && self.until_ms.is_none_or(|until| entry.at_ms < until)
            && self.before_seq.is_none_or(|before| entry.seq < before)
            && self.actor.as_deref().is_none_or(|actor| {
                event.actor.id == actor || event.actor.email.as_deref() == Some(actor) || event.actor.kind == actor
            })
            && self.action.as_deref().is_none_or(|action| match action.strip_suffix('.') {
                Some(group) => event.action.strip_prefix(group).is_some_and(|rest| rest.starts_with('.')),
                None => event.action == action,
            })
            && self.target.as_deref().is_none_or(|target| event.target.as_deref() == Some(target))
            && self.outcome.as_deref().is_none_or(|outcome| event.outcome == outcome)
    }
}

/// A page of entries, newest first.
#[derive(Debug, Serialize)]
pub struct Page {
    pub entries: Vec<Entry>,
    /// `before_seq` of the next page, when there may be one.
    pub next_before_seq: Option<u64>,
}

/// What [`AuditLog::verify`] found.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Verification {
    pub entries: u64,
    /// The first entry whose hash or link does not hold.
    pub broken_at_seq: Option<u64>,
    pub first_seq: Option<u64>,
    pub last_seq: Option<u64>,
}

/// The audit log in `<dir>/audit`.
#[derive(Debug)]
pub struct AuditLog {
    dir: PathBuf,
    retention_days: u64,
    /// One append at a time in this process; the file lock covers the others.
    appending: std::sync::Mutex<()>,
}

impl AuditLog {
    /// The log in `<data_dir>/audit`.
    ///
    /// @throws ConsoleError::Io - When the directory cannot be created.
    pub fn open(data_dir: &Path, retention_days: u64) -> Result<Self> {
        let dir = data_dir.join("audit");
        ensure_dir(&dir)?;
        Ok(Self { dir, retention_days, appending: std::sync::Mutex::new(()) })
    }

    /// Days an entry is kept.
    pub fn retention_days(&self) -> u64 {
        self.retention_days
    }

    /// The day files, oldest first.
    fn days(&self) -> Result<Vec<(String, PathBuf)>> {
        let entries = std::fs::read_dir(&self.dir)
            .map_err(|error| ConsoleError::Io(format!("cannot read {}: {error}", self.dir.display())))?;
        let mut days: Vec<(String, PathBuf)> = entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                let name = path.file_name()?.to_str()?;
                let day = name.strip_suffix(".jsonl")?;
                is_day(day).then(|| (day.to_owned(), path.clone()))
            })
            .collect();
        days.sort();
        Ok(days)
    }

    /// Append `event` now.
    ///
    /// @returns The written entry.
    /// @throws ConsoleError::Io - When the log cannot be written.
    pub fn append(&self, event: Event) -> Result<Entry> {
        self.append_at(event, now_ms())
    }

    fn append_at(&self, event: Event, at_ms: u64) -> Result<Entry> {
        let _one_at_a_time = self.appending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = self.dir.join(format!("{}.jsonl", utc_date(at_ms)));
        let io = |error: std::io::Error| ConsoleError::Io(format!("cannot write {}: {error}", path.display()));
        // The lock is on a file of its own: Windows locks are mandatory, and a
        // locked day file could not be read, by this append or by a query.
        let lock_path = self.dir.join(LOCK_FILE);
        // A write handle: `LockFileEx` refuses an append-only one on Windows.
        let lock = graphrt_core::private_fs::create_file(&lock_path)
            .map_err(|error| ConsoleError::Io(format!("cannot open {}: {error}", lock_path.display())))?;
        lock.lock().map_err(|error| ConsoleError::Io(format!("cannot lock {}: {error}", lock_path.display())))?;
        let written = (|| {
            let last = match last_entry(&path)? {
                Some(last) => Some(last),
                None => self.last_entry_before(&path)?,
            };
            let (seq, prev) = last.map_or((1, GENESIS.to_owned()), |last| (last.seq + 1, last.hash));
            let mut entry = Entry { seq, at_ms, event, prev, hash: String::new() };
            entry.hash = entry.computed_hash()?;
            let mut line = serde_json::to_vec(&entry).map_err(|error| ConsoleError::Io(error.to_string()))?;
            line.push(b'\n');
            let mut file = graphrt_core::private_fs::append_file(&path).map_err(io)?;
            file.write_all(&line).map_err(io)?;
            file.sync_data().map_err(io)?;
            Ok(entry)
        })();
        let _unlocked = lock.unlock();
        written
    }

    /// The last entry of the newest day file before `path`.
    fn last_entry_before(&self, path: &Path) -> Result<Option<Entry>> {
        for (_, earlier) in self.days()?.iter().rev() {
            if earlier.as_path() >= path {
                continue;
            }
            if let Some(entry) = last_entry(earlier)? {
                return Ok(Some(entry));
            }
        }
        Ok(None)
    }

    /// Entries that match `query`, newest first.
    ///
    /// @throws ConsoleError::Io - When a file cannot be read.
    pub fn query(&self, query: &Query) -> Result<Page> {
        let limit = query.limit.unwrap_or(100).clamp(1, MAX_PAGE);
        let mut entries = Vec::new();
        let mut more = false;
        'days: for (day, path) in self.days()?.iter().rev() {
            if query.since_ms.is_some_and(|since| day.as_str() < utc_date(since).as_str()) {
                break;
            }
            if query.until_ms.is_some_and(|until| day.as_str() > utc_date(until).as_str()) {
                continue;
            }
            let mut found: Vec<Entry> = read_entries(path)?.into_iter().filter(|e| query.matches(e)).collect();
            found.reverse();
            for entry in found {
                if entries.len() == limit {
                    more = true;
                    break 'days;
                }
                entries.push(entry);
            }
        }
        let next_before_seq = if more { entries.last().map(|entry| entry.seq) } else { None };
        Ok(Page { entries, next_before_seq })
    }

    /// The day files a JSONL export of `query` reads, oldest first: the
    /// matching lines of each, as they are stored, are the export.
    ///
    /// @throws ConsoleError::Io - When the directory cannot be read.
    pub fn export_days(&self, query: &Query) -> Result<Vec<PathBuf>> {
        Ok(self
            .days()?
            .into_iter()
            .filter(|(day, _)| query.since_ms.is_none_or(|since| day.as_str() >= utc_date(since).as_str()))
            .filter(|(day, _)| query.until_ms.is_none_or(|until| day.as_str() <= utc_date(until).as_str()))
            .map(|(_, path)| path)
            .collect())
    }

    /// The matching entries of one day file, as JSON lines.
    ///
    /// @throws ConsoleError::Io - When the file cannot be read.
    pub fn export_day(&self, path: &Path, query: &Query) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        for entry in read_entries(path)?.into_iter().filter(|entry| query.matches(entry)) {
            serde_json::to_writer(&mut out, &entry).map_err(|error| ConsoleError::Io(error.to_string()))?;
            out.push(b'\n');
        }
        Ok(out)
    }

    /// Check every hash and link of the entries kept. The first entry kept
    /// may link to one retention removed.
    ///
    /// @throws ConsoleError::Io - When a file cannot be read.
    pub fn verify(&self) -> Result<Verification> {
        let mut result = Verification { entries: 0, broken_at_seq: None, first_seq: None, last_seq: None };
        let mut previous: Option<Entry> = None;
        for (_, path) in self.days()? {
            for entry in read_entries(&path)? {
                result.entries += 1;
                result.first_seq.get_or_insert(entry.seq);
                result.last_seq = Some(entry.seq);
                let linked =
                    previous.as_ref().is_none_or(|before| entry.prev == before.hash && entry.seq == before.seq + 1);
                if result.broken_at_seq.is_none() && (!linked || entry.computed_hash()? != entry.hash) {
                    result.broken_at_seq = Some(entry.seq);
                }
                previous = Some(entry);
            }
        }
        Ok(result)
    }

    /// Delete the day files older than the retention.
    ///
    /// @returns How many files were deleted.
    /// @throws ConsoleError::Io - When the directory cannot be read.
    pub fn prune(&self) -> Result<usize> {
        self.prune_at(now_ms())
    }

    fn prune_at(&self, now: u64) -> Result<usize> {
        let oldest_kept = utc_date(now.saturating_sub(self.retention_days.saturating_mul(MS_PER_DAY)));
        let mut removed = 0;
        for (day, path) in self.days()? {
            if day < oldest_kept {
                std::fs::remove_file(&path)
                    .map_err(|error| ConsoleError::Io(format!("cannot remove {}: {error}", path.display())))?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

fn is_day(name: &str) -> bool {
    name.len() == 10 && name.char_indices().all(|(i, c)| if i == 4 || i == 7 { c == '-' } else { c.is_ascii_digit() })
}

/// Every entry of a day file. A line that is not an entry (a torn write) is skipped.
fn read_entries(path: &Path) -> Result<Vec<Entry>> {
    let file =
        File::open(path).map_err(|error| ConsoleError::Io(format!("cannot read {}: {error}", path.display())))?;
    let mut entries = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|error| ConsoleError::Io(format!("cannot read {}: {error}", path.display())))?;
        if line.len() <= MAX_LINE_BYTES
            && let Ok(entry) = serde_json::from_str::<Entry>(&line)
        {
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// The last entry of a day file, read from its end.
fn last_entry(path: &Path) -> Result<Option<Entry>> {
    let io = |error: std::io::Error| ConsoleError::Io(format!("cannot read {}: {error}", path.display()));
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io(error)),
    };
    let length = file.metadata().map_err(io)?.len();
    let start = length.saturating_sub(2 * MAX_LINE_BYTES as u64);
    file.seek(SeekFrom::Start(start)).map_err(io)?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).map_err(io)?;
    Ok(tail
        .split(|byte| *byte == b'\n')
        .rev()
        .filter(|line| !line.is_empty())
        .find_map(|line| serde_json::from_slice::<Entry>(line).ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(name: &str) -> (PathBuf, AuditLog) {
        let dir = graphrt_core::private_fs::create_fresh_temp_dir(name).unwrap();
        let log = AuditLog::open(&dir, 30).unwrap();
        (dir, log)
    }

    fn user(id: &str) -> Actor {
        Actor { kind: "user".into(), id: id.into(), email: Some(format!("{id}@example.com")), role: None }
    }

    const DAY_1: u64 = 1_800_000_000_000;

    #[test]
    fn entries_chain_across_days_and_verify() {
        let (dir, log) = log("graphrt-console-audit-chain");
        log.append_at(Event::ok(user("a"), "auth.sign_in"), DAY_1).unwrap();
        log.append_at(Event::ok(user("a"), "secret.set").target("OPENAI_API_KEY"), DAY_1 + 1).unwrap();
        let third = log.append_at(Event::ok(user("b"), "registry.promote"), DAY_1 + MS_PER_DAY).unwrap();
        assert_eq!(third.seq, 3);
        let verified = log.verify().unwrap();
        assert_eq!(verified, Verification { entries: 3, broken_at_seq: None, first_seq: Some(1), last_seq: Some(3) });
        // Editing a stored line breaks the chain at that entry.
        let first_day = dir.join("audit").join(format!("{}.jsonl", utc_date(DAY_1)));
        let text = std::fs::read_to_string(&first_day).unwrap();
        std::fs::write(&first_day, text.replace("OPENAI_API_KEY", "OTHER")).unwrap();
        assert_eq!(log.verify().unwrap().broken_at_seq, Some(2));
        let _cleanup = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn queries_filter_and_page_newest_first() {
        let (dir, log) = log("graphrt-console-audit-query");
        for i in 0..5 {
            log.append_at(Event::ok(user("a"), "secret.set").target(format!("S{i}")), DAY_1 + i).unwrap();
        }
        log.append_at(Event::ok(user("b"), "auth.sign_in").status(403), DAY_1 + 10).unwrap();
        let page = log.query(&Query { action: Some("secret.".into()), limit: Some(2), ..Query::default() }).unwrap();
        assert_eq!(page.entries.iter().map(|e| e.seq).collect::<Vec<_>>(), [5, 4]);
        let next = Query { action: Some("secret.".into()), before_seq: page.next_before_seq, ..Query::default() };
        assert_eq!(log.query(&next).unwrap().entries.len(), 3);
        let denied = log.query(&Query { outcome: Some("denied".into()), ..Query::default() }).unwrap();
        assert_eq!(denied.entries.len(), 1);
        assert_eq!(denied.entries[0].event.actor.id, "b");
        let by_email = Query { actor: Some("b@example.com".into()), ..Query::default() };
        assert_eq!(log.query(&by_email).unwrap().entries.len(), 1);
        assert!(log.query(&Query { action: Some("secret".into()), ..Query::default() }).unwrap().entries.is_empty());
        let _cleanup = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn exports_are_json_lines_of_the_range() {
        let (dir, log) = log("graphrt-console-audit-export");
        log.append_at(Event::ok(user("a"), "auth.sign_in"), DAY_1).unwrap();
        log.append_at(Event::ok(user("a"), "auth.sign_out"), DAY_1 + MS_PER_DAY).unwrap();
        let query = Query { since_ms: Some(DAY_1 + MS_PER_DAY), ..Query::default() };
        let days = log.export_days(&query).unwrap();
        assert_eq!(days.len(), 1);
        let text = String::from_utf8(log.export_day(&days[0], &query).unwrap()).unwrap();
        let entry: Entry = serde_json::from_str(text.trim_end()).unwrap();
        assert_eq!(entry.event.action, "auth.sign_out");
        let _cleanup = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn retention_removes_whole_old_days_and_the_chain_goes_on() {
        let (dir, log) = log("graphrt-console-audit-retention");
        log.append_at(Event::ok(Actor::system(), "server.start"), DAY_1).unwrap();
        log.append_at(Event::ok(Actor::system(), "server.start"), DAY_1 + 40 * MS_PER_DAY).unwrap();
        assert_eq!(log.prune_at(DAY_1 + 40 * MS_PER_DAY).unwrap(), 1);
        let next = log.append_at(Event::ok(Actor::system(), "server.start"), DAY_1 + 40 * MS_PER_DAY + 1).unwrap();
        assert_eq!(next.seq, 3);
        assert_eq!(log.verify().unwrap().broken_at_seq, None);
        let _cleanup = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn http_statuses_set_the_outcome() {
        assert_eq!(Event::ok(Actor::api_key(), "x").status(204).outcome, "ok");
        assert_eq!(Event::ok(Actor::api_key(), "x").status(403).outcome, "denied");
        assert_eq!(Event::ok(Actor::api_key(), "x").status(500).outcome, "failed");
    }
}
