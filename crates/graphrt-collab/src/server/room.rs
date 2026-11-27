//! Rooms: the live document of one project, the people in it, its files.
//!
//! A room is opened when the first person joins a project and closed a
//! little after the last one leaves. It keeps:
//!
//! - the document, loaded from `state.bin` plus the updates appended to
//!   `updates.log` since, and compacted back into `state.bin` now and then;
//! - who is connected, and the presence each replica announced (the server
//!   writes the `user` part itself, so nobody can pose as someone else);
//! - versions: `versions/<n>.bin` with `versions/index.json`, saved by hand
//!   (named) or every few minutes of editing (automatic, the oldest pruned).
//!
//! An update is checked before anyone else sees it: it must decode, apply
//! and keep the project under its size limit. Only then is it appended to
//! the log and sent to the others.

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use graphrt_console::Role;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::error::{ServerError, ServerResult};
use super::util::{ensure_dir, now_ms, read_json, replace_file, write_json};
use crate::ProjectDoc;
use crate::protocol::{Control, Frame, Person, Presence};

/// Compact the log into `state.bin` above this many bytes...
const COMPACT_LOG_BYTES: u64 = 2 * 1024 * 1024;
/// ...or this many updates.
const COMPACT_LOG_UPDATES: u64 = 2_000;
/// How long an empty room stays open (people who reconnect find it warm).
const IDLE_CLOSE: Duration = Duration::from_secs(30);
/// The largest presence state of one replica, in bytes.
pub const MAX_PRESENCE_BYTES: usize = 4 * 1024;
/// Messages waiting for one connection before it is dropped as too slow.
pub const PEER_QUEUE: usize = 512;

/// A message to one connection.
#[derive(Clone, Debug)]
pub enum Outgoing {
    Frame(Frame),
    /// Close the session with this reason.
    Close(String),
    /// The member's role changed.
    Role(Role),
}

/// A connection in a room.
#[derive(Debug)]
struct Peer {
    person: Person,
    account: String,
    role: Role,
    tx: mpsc::Sender<Outgoing>,
}

#[derive(Debug)]
struct PresenceEntry {
    conn: u64,
    clock: u32,
    state: String,
}

/// One saved version of a project.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VersionInfo {
    pub version: u64,
    pub at_ms: u64,
    #[serde(default)]
    pub label: String,
    /// Saved by the server while people edited (pruned), not by hand.
    pub auto: bool,
    /// Who edited since the version before (names).
    #[serde(default)]
    pub by: Vec<String>,
    pub bytes: usize,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct VersionIndex {
    next: u64,
    versions: Vec<VersionInfo>,
}

/// Limits of a room, from the config.
#[derive(Clone, Copy, Debug)]
pub struct RoomLimits {
    pub max_doc_bytes: usize,
    pub auto_versions: usize,
    pub version_every_ms: u64,
}

#[derive(Debug)]
struct State {
    doc: ProjectDoc,
    log: std::fs::File,
    log_bytes: u64,
    log_updates: u64,
    /// Bytes of `state.bin` at the last compaction.
    base_bytes: u64,
    peers: HashMap<u64, Peer>,
    presence: HashMap<u64, PresenceEntry>,
    /// Changed since the last version, and by whom.
    editors_since_version: Vec<String>,
    last_version_ms: u64,
    /// A change since the last compaction.
    dirty: bool,
}

/// The live document of one project.
#[derive(Debug)]
pub struct Room {
    pub workspace: String,
    pub project: String,
    dir: PathBuf,
    limits: RoomLimits,
    state: Mutex<State>,
}

/// What happened to an update.
#[derive(Debug, PartialEq, Eq)]
pub enum Applied {
    /// Applied, stored and sent to the others.
    Yes,
    /// Nothing new in it.
    Nothing,
}

impl Room {
    fn open(workspace: &str, project: &str, dir: PathBuf, limits: RoomLimits) -> ServerResult<Self> {
        ensure_dir(&dir)?;
        let (doc, base_bytes) = load_doc(&dir)?;
        let log_path = dir.join("updates.log");
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|error| ServerError::Io(format!("cannot open {}: {error}", log_path.display())))?;
        let log_bytes = log.metadata().map(|m| m.len()).unwrap_or(0);
        let index: VersionIndex = read_json(&dir.join("versions").join("index.json"))?.unwrap_or_default();
        let last_version_ms = index.versions.last().map_or(0, |v| v.at_ms);
        Ok(Self {
            workspace: workspace.to_owned(),
            project: project.to_owned(),
            dir,
            limits,
            state: Mutex::new(State {
                doc,
                log,
                log_bytes,
                log_updates: 0,
                base_bytes,
                peers: HashMap::new(),
                presence: HashMap::new(),
                editors_since_version: Vec::new(),
                last_version_ms,
                dirty: false,
            }),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Add a connection. Its queue gets nothing until [`Self::welcome`].
    pub fn join(&self, conn: u64, person: Person, account: &str, role: Role, tx: mpsc::Sender<Outgoing>) {
        self.lock().peers.insert(conn, Peer { person, account: account.to_owned(), role, tx });
    }

    /// What a new connection is sent first: the server's state vector and everyone's presence.
    pub fn welcome(&self) -> (Vec<u8>, Vec<Presence>) {
        let state = self.lock();
        let presence = state
            .presence
            .iter()
            .map(|(id, p)| Presence { client_id: *id, clock: p.clock, state: p.state.clone() })
            .collect();
        (state.doc.state_vector(), presence)
    }

    /// Remove a connection and its presence.
    ///
    /// @returns Whether the room is now empty.
    pub fn leave(&self, conn: u64) -> bool {
        let mut state = self.lock();
        state.peers.remove(&conn);
        let gone: Vec<Presence> = state
            .presence
            .iter()
            .filter(|(_, p)| p.conn == conn)
            .map(|(id, p)| Presence { client_id: *id, clock: p.clock.saturating_add(1), state: "null".into() })
            .collect();
        state.presence.retain(|_, p| p.conn != conn);
        if !gone.is_empty() {
            broadcast(&state, None, &Outgoing::Frame(Frame::Awareness(gone)));
        }
        state.peers.is_empty()
    }

    /// The number of connections.
    pub fn peers(&self) -> usize {
        self.lock().peers.len()
    }

    /// The people connected, once each.
    pub fn people(&self) -> Vec<Person> {
        let state = self.lock();
        let mut people: Vec<Person> = Vec::new();
        for peer in state.peers.values() {
            if !people.iter().any(|p| p.id == peer.person.id) {
                people.push(peer.person.clone());
            }
        }
        people.sort_by(|a, b| a.name.cmp(&b.name));
        people
    }

    /// What `state_vector` lacks.
    ///
    /// @throws ServerError::BadRequest - For a state vector that cannot be decoded.
    pub fn diff(&self, state_vector: &[u8]) -> ServerResult<Vec<u8>> {
        self.lock().doc.diff(state_vector).map_err(|error| ServerError::BadRequest(error.to_string()))
    }

    /// Whether `update` holds anything the room does not have.
    pub fn is_new(&self, update: &[u8]) -> bool {
        self.lock().doc.is_new(update).unwrap_or(false)
    }

    /// Apply an update of connection `conn` (by `editor`), store it and send it to the others.
    ///
    /// @throws ServerError::BadRequest - For an update that cannot be applied, `Limit` when the project would be too large.
    pub fn apply(&self, conn: Option<u64>, editor: &str, update: &[u8]) -> ServerResult<Applied> {
        let mut state = self.lock();
        if !state.doc.is_new(update).map_err(|error| ServerError::BadRequest(error.to_string()))?
            && update_is_insert_only(update)
        {
            return Ok(Applied::Nothing);
        }
        let projected = state.base_bytes + state.log_bytes + update.len() as u64;
        if projected > self.limits.max_doc_bytes as u64 {
            return Err(ServerError::Limit(format!(
                "the project reached its size limit of {} bytes",
                self.limits.max_doc_bytes
            )));
        }
        // yrs may panic on a crafted update: then the document is reloaded from disk
        // (what was stored before this update) and the update is refused.
        let doc = state.doc.clone();
        let applied = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| doc.apply_update(update)));
        match applied {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                drop(state);
                self.reload()?;
                return Err(ServerError::BadRequest(error.to_string()));
            }
            Err(_) => {
                drop(state);
                self.reload()?;
                return Err(ServerError::BadRequest("the update could not be applied".into()));
            }
        }
        append_log(&mut state, update, &self.dir)?;
        if !state.editors_since_version.iter().any(|e| e == editor) {
            state.editors_since_version.push(editor.to_owned());
        }
        state.dirty = true;
        broadcast(&state, conn, &Outgoing::Frame(Frame::Update(update.to_vec())));
        let due = now_ms().saturating_sub(state.last_version_ms) >= self.limits.version_every_ms;
        if state.log_bytes >= COMPACT_LOG_BYTES || state.log_updates >= COMPACT_LOG_UPDATES {
            compact(&mut state, &self.dir)?;
        }
        if due {
            self.save_version_locked(&mut state, "", true)?;
        }
        Ok(Applied::Yes)
    }

    fn reload(&self) -> ServerResult<()> {
        let mut state = self.lock();
        let (doc, base_bytes) = load_doc(&self.dir)?;
        state.doc = doc;
        state.base_bytes = base_bytes;
        Ok(())
    }

    /// Record presence entries of connection `conn`, with the user part set to `person`,
    /// and send them to the others.
    ///
    /// @returns How many entries were refused (another connection's client id, too large, not JSON).
    pub fn presence(&self, conn: u64, person: &Person, entries: Vec<Presence>) -> usize {
        let mut state = self.lock();
        let mut accepted = Vec::new();
        let mut refused = 0;
        for entry in entries {
            if state.presence.get(&entry.client_id).is_some_and(|p| p.conn != conn) {
                refused += 1;
                continue;
            }
            if entry.state == "null" {
                if state.presence.remove(&entry.client_id).is_some() {
                    accepted.push(entry);
                }
                continue;
            }
            let Some(rewritten) = rewrite_presence(&entry.state, person) else {
                refused += 1;
                continue;
            };
            let at_most_per_conn = state.presence.values().filter(|p| p.conn == conn).count() >= 4
                && !state.presence.contains_key(&entry.client_id);
            if at_most_per_conn {
                refused += 1;
                continue;
            }
            state
                .presence
                .insert(entry.client_id, PresenceEntry { conn, clock: entry.clock, state: rewritten.clone() });
            accepted.push(Presence { client_id: entry.client_id, clock: entry.clock, state: rewritten });
        }
        if !accepted.is_empty() {
            broadcast(&state, Some(conn), &Outgoing::Frame(Frame::Awareness(accepted)));
        }
        refused
    }

    /// Everyone's presence.
    pub fn all_presence(&self) -> Vec<Presence> {
        self.lock()
            .presence
            .iter()
            .map(|(id, p)| Presence { client_id: *id, clock: p.clock, state: p.state.clone() })
            .collect()
    }

    /// Send `message` to the connections of `account` (all when `None`).
    pub fn notify(&self, account: Option<&str>, message: &Outgoing) {
        let mut state = self.lock();
        for peer in state.peers.values_mut() {
            if account.is_none_or(|a| a == peer.account) {
                if let Outgoing::Role(role) = message {
                    peer.role = *role;
                    peer.person.role = role.name().to_owned();
                }
                let _full = peer.tx.try_send(message.clone());
            }
        }
    }

    /// Send a frame to every connection.
    pub fn broadcast_all(&self, frame: Frame) {
        let state = self.lock();
        broadcast(&state, None, &Outgoing::Frame(frame));
    }

    /// The project as JSON.
    pub fn json(&self) -> serde_json::Value {
        self.lock().doc.to_json()
    }

    /// The whole document as one update.
    pub fn encode_state(&self) -> Vec<u8> {
        self.lock().doc.encode_state()
    }

    /// Save the current state as version `label` (named when not empty).
    ///
    /// @returns The version.
    pub fn save_version(&self, label: &str, by: &str) -> ServerResult<VersionInfo> {
        let mut state = self.lock();
        if !state.editors_since_version.iter().any(|e| e == by) {
            state.editors_since_version.push(by.to_owned());
        }
        self.save_version_locked(&mut state, label, false)
    }

    fn save_version_locked(&self, state: &mut State, label: &str, auto: bool) -> ServerResult<VersionInfo> {
        let dir = self.dir.join("versions");
        ensure_dir(&dir)?;
        let index_path = dir.join("index.json");
        let mut index: VersionIndex = read_json(&index_path)?.unwrap_or_default();
        let bytes = state.doc.encode_state();
        index.next = index.next.max(index.versions.last().map_or(0, |v| v.version)) + 1;
        let info = VersionInfo {
            version: index.next,
            at_ms: now_ms(),
            label: label.chars().take(120).collect(),
            auto,
            by: std::mem::take(&mut state.editors_since_version),
            bytes: bytes.len(),
        };
        replace_file(&dir.join(format!("{}.bin", info.version)), &bytes)?;
        index.versions.push(info.clone());
        // Prune automatic versions beyond the limit, oldest first.
        let autos = index.versions.iter().filter(|v| v.auto).count();
        if autos > self.limits.auto_versions {
            let mut excess = autos - self.limits.auto_versions;
            let mut kept = Vec::with_capacity(index.versions.len());
            for version in index.versions.drain(..) {
                if version.auto && excess > 0 {
                    excess -= 1;
                    let _gone = std::fs::remove_file(dir.join(format!("{}.bin", version.version)));
                } else {
                    kept.push(version);
                }
            }
            index.versions = kept;
        }
        write_json(&index_path, &index)?;
        state.last_version_ms = info.at_ms;
        Ok(info)
    }

    /// Save an automatic version when something changed since the last one.
    pub fn checkpoint(&self) -> ServerResult<()> {
        let mut state = self.lock();
        if !state.editors_since_version.is_empty() {
            self.save_version_locked(&mut state, "", true)?;
        }
        if state.dirty {
            compact(&mut state, &self.dir)?;
        }
        Ok(())
    }
}

/// Whether an update holds no deletions (an update of only known insertions changes nothing).
fn update_is_insert_only(update: &[u8]) -> bool {
    // A v1 update ends with its delete set: a zero count of clients when it deletes nothing.
    update.last() == Some(&0)
}

/// The versions of the project in `dir`, newest first.
pub fn versions(dir: &Path) -> ServerResult<Vec<VersionInfo>> {
    let index: VersionIndex = read_json(&dir.join("versions").join("index.json"))?.unwrap_or_default();
    let mut versions = index.versions;
    versions.reverse();
    Ok(versions)
}

/// The document of version `version` of the project in `dir`.
///
/// @throws ServerError::NotFound - When there is no such version.
pub fn version_doc(dir: &Path, version: u64) -> ServerResult<ProjectDoc> {
    let path = dir.join("versions").join(format!("{version}.bin"));
    let bytes = std::fs::read(&path).map_err(|_| ServerError::NotFound("no such version".into()))?;
    ProjectDoc::from_state(&bytes).map_err(|error| ServerError::Internal(error.to_string()))
}

fn load_doc(dir: &Path) -> ServerResult<(ProjectDoc, u64)> {
    let doc = ProjectDoc::new();
    let state_path = dir.join("state.bin");
    let mut base_bytes = 0;
    match std::fs::read(&state_path) {
        Ok(bytes) => {
            base_bytes = bytes.len() as u64;
            doc.apply_update(&bytes).map_err(|error| ServerError::Io(format!("{}: {error}", state_path.display())))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(ServerError::Io(format!("cannot read {}: {error}", state_path.display()))),
    }
    let log_path = dir.join("updates.log");
    if let Ok(mut file) = std::fs::File::open(&log_path) {
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| ServerError::Io(format!("cannot read {}: {error}", log_path.display())))?;
        let mut at = 0usize;
        // Length prefixed updates. A torn last one (a crash mid write) is skipped.
        while let Some(len_bytes) = bytes.get(at..at + 4) {
            let Ok(len_bytes) = <[u8; 4]>::try_from(len_bytes) else { break };
            let len = u32::from_le_bytes(len_bytes) as usize;
            let Some(update) = bytes.get(at + 4..at + 4 + len) else { break };
            if let Err(error) = doc.apply_update(update) {
                eprintln!("warning: collab: skipped a stored update of {}: {error}", dir.display());
            }
            at += 4 + len;
        }
    }
    Ok((doc, base_bytes))
}

fn append_log(state: &mut State, update: &[u8], dir: &Path) -> ServerResult<()> {
    let len = u32::try_from(update.len()).map_err(|_| ServerError::BadRequest("update too large".into()))?;
    let mut record = Vec::with_capacity(update.len() + 4);
    record.extend_from_slice(&len.to_le_bytes());
    record.extend_from_slice(update);
    state
        .log
        .write_all(&record)
        .map_err(|error| ServerError::Io(format!("cannot write {}/updates.log: {error}", dir.display())))?;
    state.log_bytes += record.len() as u64;
    state.log_updates += 1;
    Ok(())
}

/// Write the document to `state.bin` and empty the log.
fn compact(state: &mut State, dir: &Path) -> ServerResult<()> {
    let bytes = state.doc.encode_state();
    replace_file(&dir.join("state.bin"), &bytes)?;
    let log_path = dir.join("updates.log");
    state.log = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)
        .map_err(|error| ServerError::Io(format!("cannot open {}: {error}", log_path.display())))?;
    state.log_bytes = 0;
    state.log_updates = 0;
    state.base_bytes = bytes.len() as u64;
    state.dirty = false;
    Ok(())
}

fn broadcast(state: &State, except: Option<u64>, message: &Outgoing) {
    for (conn, peer) in &state.peers {
        if Some(*conn) != except && peer.tx.try_send(message.clone()).is_err() {
            // A full queue: the connection is too slow and is closed by its own task
            // when it sees the channel closed. Here it just misses this message and
            // resyncs on reconnect.
            eprintln!("note: collab: dropped a message to a slow connection");
        }
    }
}

/// The presence `state` (a JSON object) with `user` replaced by `person`.
fn rewrite_presence(state: &str, person: &Person) -> Option<String> {
    if state.len() > MAX_PRESENCE_BYTES {
        return None;
    }
    let mut value: serde_json::Value = serde_json::from_str(state).ok()?;
    let object = value.as_object_mut()?;
    object.insert("user".into(), serde_json::to_value(person).ok()?);
    serde_json::to_string(&value).ok()
}

/// The open rooms.
#[derive(Debug)]
pub struct Rooms {
    limits: RoomLimits,
    open: Mutex<HashMap<(String, String), Arc<Room>>>,
    next_conn: AtomicU64,
}

impl Rooms {
    pub fn new(limits: RoomLimits) -> Self {
        Self { limits, open: Mutex::new(HashMap::new()), next_conn: AtomicU64::new(1) }
    }

    /// A new connection id.
    pub fn next_conn(&self) -> u64 {
        self.next_conn.fetch_add(1, Ordering::Relaxed)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), Arc<Room>>> {
        self.open.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The room of a project, opened from `dir` when it is not open.
    pub fn get_or_open(&self, workspace: &str, project: &str, dir: PathBuf) -> ServerResult<Arc<Room>> {
        let key = (workspace.to_owned(), project.to_owned());
        let mut open = self.lock();
        if let Some(room) = open.get(&key) {
            return Ok(Arc::clone(room));
        }
        let room = Arc::new(Room::open(workspace, project, dir, self.limits)?);
        open.insert(key, Arc::clone(&room));
        Ok(room)
    }

    /// The room of a project when it is open.
    pub fn get(&self, workspace: &str, project: &str) -> Option<Arc<Room>> {
        self.lock().get(&(workspace.to_owned(), project.to_owned())).cloned()
    }

    /// The open rooms of `workspace`.
    pub fn of_workspace(&self, workspace: &str) -> Vec<Arc<Room>> {
        self.lock().values().filter(|r| r.workspace == workspace).cloned().collect()
    }

    /// Close a room when it stays empty for a little while.
    pub fn close_when_idle(self: &Arc<Self>, room: &Arc<Room>) {
        let rooms: Weak<Self> = Arc::downgrade(self);
        let room = Arc::clone(room);
        tokio::spawn(async move {
            tokio::time::sleep(IDLE_CLOSE).await;
            let Some(rooms) = rooms.upgrade() else { return };
            rooms.close_if_empty(&room);
        });
    }

    fn close_if_empty(&self, room: &Arc<Room>) {
        let mut open = self.lock();
        if room.peers() > 0 {
            return;
        }
        let key = (room.workspace.clone(), room.project.clone());
        if open.get(&key).is_some_and(|r| Arc::ptr_eq(r, room)) {
            open.remove(&key);
        }
        drop(open);
        if let Err(error) = room.checkpoint() {
            eprintln!("warning: collab: closing {}/{}: {error}", room.workspace, room.project);
        }
    }

    /// Close a room now (its project is being deleted): everyone is sent `reason`.
    pub fn close_now(&self, workspace: &str, project: &str, reason: &str) {
        let room = self.lock().remove(&(workspace.to_owned(), project.to_owned()));
        if let Some(room) = room {
            room.notify(None, &Outgoing::Close(reason.to_owned()));
        }
    }

    /// Save every open room (at shutdown).
    pub fn checkpoint_all(&self) {
        let rooms: Vec<Arc<Room>> = self.lock().values().cloned().collect();
        for room in rooms {
            room.notify(None, &Outgoing::Close("the server is restarting".into()));
            if let Err(error) = room.checkpoint() {
                eprintln!("warning: collab: saving {}/{}: {error}", room.workspace, room.project);
            }
        }
    }
}

/// Control frame telling a connection about its new role.
pub fn role_frame(role: Role) -> Frame {
    Frame::Control(Control::Role { role: role.name().to_owned(), read_only: role < Role::Operator })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("graphrt-collab-room-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn limits() -> RoomLimits {
        RoomLimits { max_doc_bytes: 1024 * 1024, auto_versions: 2, version_every_ms: u64::MAX }
    }

    fn person(id: &str) -> Person {
        Person { id: id.into(), name: id.into(), role: "operator".into(), color: "#000".into() }
    }

    fn edit(value: serde_json::Value) -> (ProjectDoc, Vec<u8>) {
        let doc = ProjectDoc::new();
        let update = doc.apply_json(value.as_object().unwrap()).unwrap().unwrap();
        (doc, update)
    }

    #[tokio::test]
    async fn applies_stores_and_broadcasts() {
        let dir = temp_dir("apply");
        let room = Room::open("ws", "pr", dir.clone(), limits()).unwrap();
        let (tx_a, _rx_a) = mpsc::channel(8);
        let (tx_b, mut rx_b) = mpsc::channel(8);
        room.join(1, person("a"), "acct_a", Role::Operator, tx_a);
        room.join(2, person("b"), "acct_b", Role::Operator, tx_b);
        let (_, update) = edit(json!({"name": "Bot"}));
        assert_eq!(room.apply(Some(1), "a", &update).unwrap(), Applied::Yes);
        assert!(matches!(rx_b.recv().await, Some(Outgoing::Frame(Frame::Update(_)))));
        // The same update again is nothing new.
        assert_eq!(room.apply(Some(1), "a", &update).unwrap(), Applied::Nothing);
        drop(room);
        // Reopened from the log.
        let room = Room::open("ws", "pr", dir.clone(), limits()).unwrap();
        assert_eq!(room.json(), json!({"name": "Bot"}));
        room.checkpoint().unwrap();
        let room = Room::open("ws", "pr", dir, limits()).unwrap();
        assert_eq!(room.json(), json!({"name": "Bot"}));
    }

    #[test]
    fn refuses_garbage_and_oversized_updates() {
        let dir = temp_dir("refuse");
        let small = RoomLimits { max_doc_bytes: 64, ..limits() };
        let room = Room::open("ws", "pr", dir, small).unwrap();
        assert!(room.apply(None, "a", &[1, 2, 3, 4, 5]).is_err());
        let (_, update) = edit(json!({"notes": "x".repeat(200)}));
        assert!(matches!(room.apply(None, "a", &update), Err(ServerError::Limit(_))));
        assert_eq!(room.json(), json!({}));
    }

    #[test]
    fn presence_is_rewritten_and_bound_to_its_connection() {
        let dir = temp_dir("presence");
        let room = Room::open("ws", "pr", dir, limits()).unwrap();
        let (tx, _rx) = mpsc::channel(8);
        room.join(1, person("a"), "acct_a", Role::Operator, tx.clone());
        room.join(2, person("b"), "acct_b", Role::Operator, tx);
        let forged = r#"{"user":{"id":"acct_admin","name":"Admin"},"focus":"llm"}"#;
        assert_eq!(room.presence(1, &person("a"), vec![Presence { client_id: 10, clock: 1, state: forged.into() }]), 0);
        let stored = room.all_presence();
        assert!(stored[0].state.contains("\"id\":\"a\""));
        assert!(stored[0].state.contains("\"focus\":\"llm\""));
        // Connection 2 cannot write client 10's presence.
        assert_eq!(room.presence(2, &person("b"), vec![Presence { client_id: 10, clock: 2, state: "{}".into() }]), 1);
        assert_eq!(room.presence(2, &person("b"), vec![Presence { client_id: 11, clock: 1, state: "[1]".into() }]), 1);
        let big = format!("{{\"x\":\"{}\"}}", "y".repeat(MAX_PRESENCE_BYTES));
        assert_eq!(room.presence(2, &person("b"), vec![Presence { client_id: 12, clock: 1, state: big }]), 1);
        room.leave(1);
        assert!(room.all_presence().is_empty());
    }

    #[test]
    fn versions_are_saved_listed_and_pruned() {
        let dir = temp_dir("versions");
        let room = Room::open("ws", "pr", dir.clone(), limits()).unwrap();
        let (doc, update) = edit(json!({"name": "One"}));
        room.apply(None, "a", &update).unwrap();
        let named = room.save_version("first", "a").unwrap();
        for name in ["Two", "Three", "Four"] {
            let update = doc.apply_json(json!({"name": name}).as_object().unwrap()).unwrap().unwrap();
            room.apply(None, "a", &update).unwrap();
            room.checkpoint().unwrap();
        }
        let list = versions(&dir).unwrap();
        // The named one stays, automatic ones are pruned to 2.
        assert_eq!(list.len(), 3);
        assert!(list.iter().any(|v| v.label == "first" && !v.auto));
        assert_eq!(version_doc(&dir, named.version).unwrap().to_json(), json!({"name": "One"}));
        assert!(version_doc(&dir, 999).is_err());
    }
}
