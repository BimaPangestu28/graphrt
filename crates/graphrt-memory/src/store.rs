//! The file store: one JSON document per scope, replaced atomically under an
//! OS lock, an append only audit log per agent, and a bounded cache of
//! loaded scopes with their keyword index.
//!
//! ```text
//! <root>/_/<agent>/                  a server without tenants
//! <root>/t/<tenant>/<agent>/         one tenant
//!   agent.json                       the agent's own notes
//!   audit.jsonl                      one line per change, no note text
//!   users/<sha256 of user id>/notes.json
//! ```

use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use futures::future::BoxFuture;
use graphrt_core::memory::{
    ForgetTarget, MemoryBank, MemoryError, MemoryHit, MemoryKind, MemoryLevel, MemoryQuery, MemoryRecord, MemoryScope,
    MemorySource, MemoryStatus, MemoryTrust, NewMemory,
};
use graphrt_core::private_fs;
use graphrt_rag::Embedder;
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};

use crate::gate::{clean_text, decide, mask_secrets, reads_like_instruction};
use crate::search::{Index, SAME_NOTE_OVERLAP, age_factor, fuse, normalized, overlap, trust_factor};

const DAY_MS: u64 = 24 * 3600 * 1000;

/// Bounds of the store. Every one holds per scope, whoever writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Characters per note.
    pub note_chars: usize,
    /// Notes of one user.
    pub user_notes: usize,
    /// Notes of the agent's own memory.
    pub agent_notes: usize,
    /// Bytes of one scope file.
    pub scope_bytes: usize,
    /// Notes one recall returns at most.
    pub recall_notes: usize,
    /// Characters of all notes one recall returns.
    pub recall_chars: usize,
    /// Time to live of `event` and `note` notes. `fact` and `preference` live until forgotten.
    pub short_lived_ms: u64,
    /// Tags per note, and characters per tag.
    pub tags: usize,
    pub tag_chars: usize,
    /// Users one agent keeps notes for. With `scope_bytes` it bounds the disk
    /// an agent can use (10 000 users of 1 MiB: 10 GiB at most).
    pub users: usize,
}

impl Limits {
    /// The same limits, none below what the store needs to work.
    fn sane(self) -> Self {
        Self {
            note_chars: self.note_chars.max(1),
            user_notes: self.user_notes.max(1),
            agent_notes: self.agent_notes.max(1),
            scope_bytes: self.scope_bytes.max(16 * 1024),
            recall_notes: self.recall_notes.max(1),
            recall_chars: self.recall_chars.max(100),
            short_lived_ms: self.short_lived_ms.max(1),
            tags: self.tags,
            tag_chars: self.tag_chars.max(1),
            users: self.users.max(1),
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            note_chars: 1_000,
            user_notes: 500,
            agent_notes: 2_000,
            scope_bytes: 1024 * 1024,
            recall_notes: 8,
            recall_chars: 4_000,
            short_lived_ms: 90 * DAY_MS,
            tags: 8,
            tag_chars: 32,
            users: 10_000,
        }
    }
}

/// How many loaded scopes stay in memory.
const CACHE_SCOPES: usize = 64;
/// Format of the scope files.
const FORMAT: u32 = 1;

/// The memory store. Cheap to clone: clones share the cache and the lock.
#[derive(Clone)]
pub struct MemoryStore {
    inner: Arc<Inner>,
}

struct Inner {
    root: PathBuf,
    limits: Limits,
    embedder: Option<Arc<dyn Embedder>>,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    cache: Mutex<Cache>,
    /// One writer at a time in this process. The OS lock covers other processes.
    writing: Mutex<()>,
    counter: AtomicU64,
    /// When the embedder last failed (0: never): recall rests it for a minute.
    embedder_failed_ms: AtomicU64,
    /// Loaded graphs with their indexes, by file.
    #[cfg(feature = "graph")]
    graphs: Mutex<HashMap<PathBuf, CachedGraph>>,
}

#[cfg(feature = "graph")]
#[derive(Clone)]
struct CachedGraph {
    stamp: Option<(SystemTime, u64)>,
    graph: Arc<crate::graph::GraphFile>,
    index: Arc<crate::graph::GraphIndex>,
}

impl std::fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryStore").field("root", &self.inner.root).finish_non_exhaustive()
    }
}

#[derive(Default)]
struct Cache {
    scopes: HashMap<PathBuf, Loaded>,
    tick: u64,
}

#[derive(Clone)]
struct Loaded {
    file: Arc<ScopeFile>,
    index: Arc<Index>,
    stamp: Option<(SystemTime, u64)>,
    used: u64,
}

/// One scope on disk.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ScopeFile {
    format: u32,
    /// The user id itself, for export. Never part of a path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user: Option<String>,
    #[serde(default)]
    notes: Vec<Stored>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Stored {
    #[serde(flatten)]
    record: MemoryRecord,
    /// Embedding of the text, when the store has an embedder.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    vector: Vec<f32>,
}

/// What the owner lists.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListFilter {
    pub status: Option<MemoryStatus>,
    pub query: Option<String>,
}

fn store_error(context: &str, error: &dyn std::fmt::Display) -> MemoryError {
    MemoryError::new("memory_store", format!("{context}: {error}"))
}

pub use graphrt_core::memory::{check_name, check_user};

/// An audit log past this size is moved to `audit.1.jsonl` (the one before is
/// dropped), so the log of an agent stays under twice this.
const AUDIT_BYTES: u64 = 4 * 1024 * 1024;
/// A recall marks a note as used again only after this long: a busy agent
/// does not rewrite its scope on every run.
const TOUCH_AFTER_MS: u64 = 60_000;
/// How long recall goes by keyword alone after the embedder failed.
const EMBEDDER_REST_MS: u64 = 60_000;

fn sha256_hex(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().fold(String::with_capacity(64), |mut out, byte| {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn system_now_ms() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0))
}

impl MemoryStore {
    /// A store under `root`. Opening does no I/O: a scope is read when it is first used.
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                root: root.into(),
                limits: Limits::default(),
                embedder: None,
                clock: Arc::new(system_now_ms),
                cache: Mutex::new(Cache::default()),
                writing: Mutex::new(()),
                counter: AtomicU64::new(0),
                embedder_failed_ms: AtomicU64::new(0),
                #[cfg(feature = "graph")]
                graphs: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// `$GRAPHRT_MEMORY_DIR`, else `~/.graphrt/memory`.
    pub fn default_root() -> PathBuf {
        if let Some(dir) = std::env::var_os("GRAPHRT_MEMORY_DIR").filter(|d| !d.is_empty()) {
            return PathBuf::from(dir);
        }
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).unwrap_or_default();
        PathBuf::from(home).join(".graphrt").join("memory")
    }

    fn rebuild(self, change: impl FnOnce(&mut Inner)) -> Self {
        let inner = match Arc::try_unwrap(self.inner) {
            Ok(mut inner) => {
                change(&mut inner);
                inner
            }
            Err(shared) => {
                let mut inner = Inner {
                    root: shared.root.clone(),
                    limits: shared.limits.clone(),
                    embedder: shared.embedder.clone(),
                    clock: Arc::clone(&shared.clock),
                    cache: Mutex::new(Cache::default()),
                    writing: Mutex::new(()),
                    counter: AtomicU64::new(0),
                    embedder_failed_ms: AtomicU64::new(0),
                    #[cfg(feature = "graph")]
                    graphs: Mutex::new(HashMap::new()),
                };
                change(&mut inner);
                inner
            }
        };
        Self { inner: Arc::new(inner) }
    }

    /// Other bounds than the defaults. Bounds below what the store needs to
    /// work (no note at all, a scope smaller than 16 KiB) are raised to it.
    pub fn with_limits(self, limits: Limits) -> Self {
        self.rebuild(|inner| inner.limits = limits.sane())
    }

    /// Give notes a vector (at recall, for the notes in use that have none)
    /// and add similarity to recall.
    pub fn with_embedder(self, embedder: Arc<dyn Embedder>) -> Self {
        self.rebuild(|inner| inner.embedder = Some(embedder))
    }

    /// Another clock, in milliseconds since the epoch (for tests).
    pub fn with_clock(self, clock: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        self.rebuild(|inner| inner.clock = clock)
    }

    /// The root directory.
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// The limits in force.
    pub fn limits(&self) -> &Limits {
        &self.inner.limits
    }

    /// This store as the service nodes look for.
    pub fn into_service(self) -> graphrt_core::memory::Memories {
        graphrt_core::memory::Memories::new(Arc::new(self))
    }

    fn now(&self) -> u64 {
        (self.inner.clock)()
    }

    /// The directory of an agent, after checking the names.
    fn agent_dir(&self, tenant: Option<&str>, agent: &str) -> Result<PathBuf, MemoryError> {
        check_name("agent", agent)?;
        if let Some(tenant) = tenant {
            check_name("tenant", tenant)?;
        }
        Ok(match tenant {
            None => self.inner.root.join("_").join(agent),
            Some(tenant) => {
                check_name("tenant", tenant)?;
                self.inner.root.join("t").join(tenant).join(agent)
            }
        })
    }

    fn scope_path(&self, scope: &MemoryScope) -> Result<PathBuf, MemoryError> {
        scope.validate()?;
        let dir = self.agent_dir(scope.tenant.as_deref(), &scope.agent)?;
        Ok(match &scope.user {
            None => dir.join("agent.json"),
            Some(user) => dir.join("users").join(sha256_hex(user)).join("notes.json"),
        })
    }

    fn limit_of(&self, scope: &MemoryScope) -> usize {
        match scope.user {
            Some(_) => self.inner.limits.user_notes,
            None => self.inner.limits.agent_notes,
        }
    }

    /// The scope at `path`, from the cache when the file did not change.
    fn load(&self, path: &Path) -> Result<Loaded, MemoryError> {
        let stamp = match fs::metadata(path) {
            Ok(meta) => Some((meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), meta.len())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(store_error("cannot read a memory file", &e)),
        };
        {
            let mut cache = self.inner.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.tick += 1;
            let tick = cache.tick;
            if let Some(loaded) = cache.scopes.get_mut(path)
                && loaded.stamp == stamp
            {
                loaded.used = tick;
                return Ok(loaded.clone());
            }
        }
        let file = match stamp {
            None => ScopeFile { format: FORMAT, ..ScopeFile::default() },
            Some(_) => {
                let bytes = fs::read(path).map_err(|e| store_error("cannot read a memory file", &e))?;
                let file = serde_json::from_slice::<ScopeFile>(&bytes).map_err(|e| {
                    MemoryError::new("memory_corrupt", format!("{} cannot be read: {e}", path.display()))
                })?;
                // A newer format may hold what this build would drop when it writes the file back.
                if file.format > FORMAT {
                    return Err(MemoryError::new(
                        "memory_version",
                        format!(
                            "{} is of memory format {}, this build reads up to {FORMAT}",
                            path.display(),
                            file.format
                        ),
                    ));
                }
                file
            }
        };
        Ok(self.remember_loaded(path, file, stamp))
    }

    fn remember_loaded(&self, path: &Path, file: ScopeFile, stamp: Option<(SystemTime, u64)>) -> Loaded {
        let documents: Vec<String> =
            file.notes.iter().map(|n| format!("{} {}", n.record.text, n.record.tags.join(" "))).collect();
        let index = Index::build(documents.iter().map(String::as_str));
        let mut cache = self.inner.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.tick += 1;
        let loaded = Loaded { file: Arc::new(file), index: Arc::new(index), stamp, used: cache.tick };
        if cache.scopes.len() >= CACHE_SCOPES && !cache.scopes.contains_key(path) {
            let oldest = cache.scopes.iter().min_by_key(|(_, l)| l.used).map(|(p, _)| p.clone());
            if let Some(oldest) = oldest {
                cache.scopes.remove(&oldest);
            }
        }
        cache.scopes.insert(path.to_path_buf(), loaded.clone());
        loaded
    }

    fn forget_cached(&self, path: &Path) {
        let mut cache = self.inner.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.scopes.remove(path);
    }

    /// Change the scope at `path` under both locks and write it back. `change`
    /// returns the audit lines of what it did. Nothing is written when it
    /// returns none, and nothing is created on disk for a scope that does not
    /// exist and that `change` leaves empty (a lookup of an unknown agent).
    ///
    /// The audit lines are written before the file: a failure in between can
    /// leave a line for a change that did not happen, never a change without
    /// its line.
    fn change<T>(
        &self,
        scope: &MemoryScope,
        change: impl Fn(&mut ScopeFile, u64) -> Result<(T, Vec<Json>), MemoryError>,
    ) -> Result<T, MemoryError> {
        let path = self.scope_path(scope)?;
        let agent_dir = self.agent_dir(scope.tenant.as_deref(), &scope.agent)?;
        let _writing = self.inner.writing.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !path.exists() {
            let mut empty = ScopeFile { format: FORMAT, ..ScopeFile::default() };
            let (out, lines) = change(&mut empty, self.now())?;
            if lines.is_empty() {
                return Ok(out);
            }
        }
        private_fs::create_dir_all(&agent_dir).map_err(|e| store_error("cannot create a memory directory", &e))?;
        let _lock = lock(&agent_dir)?;
        // Under the lock, so two processes cannot both take the last place.
        if !path.exists()
            && let Some(user_dir) = path.parent().filter(|_| scope.user.is_some())
        {
            self.check_user_room(user_dir)?;
        }
        // Read again under the lock: another process may have written.
        self.forget_cached(&path);
        let mut file = (*self.load(&path)?.file).clone();
        file.format = FORMAT;
        if let Some(user) = &scope.user {
            file.user = Some(user.clone());
        }
        let now = self.now();
        let mut audit = expire(&mut file, now, scope);
        let (out, mut lines) = change(&mut file, now)?;
        audit.append(&mut lines);
        if audit.is_empty() {
            return Ok(out);
        }
        // Keep the file within its byte bound: the note used least and longest
        // ago goes first, never one this change wrote.
        let fresh: Vec<String> = audit
            .iter()
            .filter(|line| matches!(line["action"].as_str(), Some("remember" | "update")))
            .filter_map(|line| line["id"].as_str().map(str::to_owned))
            .collect();
        // Only a write of a note makes room: a use or a vector never retires anything.
        let writes_a_note = !fresh.is_empty();
        let mut bytes = serde_json::to_vec(&file).map_err(|e| store_error("cannot encode memory", &e))?;
        while writes_a_note && bytes.len() > self.inner.limits.scope_bytes {
            // Who writes: an expired owner note does not make the writer the owner.
            let by_owner = audit
                .iter()
                .any(|line| line["trust"] == "owner" && matches!(line["action"].as_str(), Some("remember" | "update")));
            match retire_one(&mut file, scope, &fresh, by_owner) {
                Some(line) => audit.push(line),
                None => {
                    return Err(MemoryError::new(
                        "memory_full",
                        format!(
                            "the note does not fit in this scope ({} bytes at most)",
                            self.inner.limits.scope_bytes
                        ),
                    ));
                }
            }
            bytes = serde_json::to_vec(&file).map_err(|e| store_error("cannot encode memory", &e))?;
        }
        if let Some(parent) = path.parent() {
            private_fs::create_dir_all(parent).map_err(|e| store_error("cannot create a memory directory", &e))?;
        }
        // The graph follows its notes: relations of a removed note go, those of
        // an approved one come into use.
        // A graph that cannot be read must not stop the notes: recall checks
        // every fact against its notes anyway.
        #[cfg(feature = "graph")]
        let graph = self.graph_follows(scope, &audit, now).unwrap_or_else(|e| {
            eprintln!("warning: memory graph of `{}` not updated: {e}", scope.agent);
            None
        });
        #[cfg(feature = "graph")]
        if let Some((_, lines)) = &graph {
            audit.extend(lines.iter().cloned());
        }
        self.append_audit(&agent_dir, &audit, now)?;
        // The graph first: if the notes file then fails, a fact may be missing
        // until the next build, but no fact outlives the note it came from.
        #[cfg(feature = "graph")]
        if let Some((graph, _)) = graph {
            self.write_graph(scope, &graph)?;
        }
        self.write_atomic(&path, &bytes)?;
        let stamp = fs::metadata(&path).ok().map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()));
        self.remember_loaded(&path, file, stamp);
        Ok(out)
    }

    /// Where the graph of `scope` is kept: next to its notes.
    #[cfg(feature = "graph")]
    fn graph_path(&self, scope: &MemoryScope) -> Result<PathBuf, MemoryError> {
        let notes = self.scope_path(scope)?;
        let dir = notes.parent().unwrap_or(&self.inner.root);
        Ok(match scope.user {
            Some(_) => dir.join("graph.json"),
            None => dir.join("agent.graph.json"),
        })
    }

    #[cfg(feature = "graph")]
    fn load_graph(&self, scope: &MemoryScope) -> Result<crate::graph::GraphFile, MemoryError> {
        let path = self.graph_path(scope)?;
        let graph: crate::graph::GraphFile = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| MemoryError::new("memory_corrupt", format!("{} cannot be read: {e}", path.display())))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => crate::graph::GraphFile::default(),
            Err(e) => return Err(store_error("cannot read a memory graph", &e)),
        };
        if graph.format > FORMAT {
            return Err(MemoryError::new(
                "memory_version",
                format!("{} is of memory format {}, this build reads up to {FORMAT}", path.display(), graph.format),
            ));
        }
        Ok(graph)
    }

    #[cfg(feature = "graph")]
    fn write_graph(&self, scope: &MemoryScope, graph: &crate::graph::GraphFile) -> Result<(), MemoryError> {
        let path = self.graph_path(scope)?;
        let mut graph = graph.clone();
        graph.format = FORMAT;
        let bytes = serde_json::to_vec(&graph).map_err(|e| store_error("cannot encode the memory graph", &e))?;
        self.write_atomic(&path, &bytes)?;
        let mut graphs = self.inner.graphs.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        graphs.remove(&path);
        Ok(())
    }

    /// The graph of `scope` with its indexes, from the cache when the file did not change.
    #[cfg(feature = "graph")]
    fn cached_graph(&self, scope: &MemoryScope) -> Result<CachedGraph, MemoryError> {
        let path = self.graph_path(scope)?;
        let stamp = fs::metadata(&path).ok().map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()));
        {
            let graphs = self.inner.graphs.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cached) = graphs.get(&path).filter(|c| c.stamp == stamp) {
                return Ok(cached.clone());
            }
        }
        let graph = self.load_graph(scope)?;
        let cached =
            CachedGraph { stamp, index: Arc::new(crate::graph::GraphIndex::build(&graph)), graph: Arc::new(graph) };
        let mut graphs = self.inner.graphs.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if graphs.len() >= CACHE_SCOPES {
            graphs.clear();
        }
        graphs.insert(path, cached.clone());
        Ok(cached)
    }

    /// What the notes changes of `audit` do to the graph of `scope`, when it has one.
    #[cfg(feature = "graph")]
    fn graph_follows(
        &self,
        scope: &MemoryScope,
        audit: &[Json],
        now: u64,
    ) -> Result<Option<(crate::graph::GraphFile, Vec<Json>)>, MemoryError> {
        let ids = |actions: &[&str]| -> Vec<String> {
            audit
                .iter()
                .filter(|line| line["action"].as_str().is_some_and(|a| actions.contains(&a)))
                .filter_map(|line| line["id"].as_str().map(str::to_owned))
                .collect()
        };
        let gone = ids(&["forget", "reject", "retire", "expire"]);
        let undone = ids(&["forget", "reject"]);
        // A note comes into use by approval, or by an update from a writer whose notes are used at once.
        let mut approved = ids(&["approve"]);
        approved.extend(
            audit
                .iter()
                .filter(|line| line["action"] == "update" && line["activated"] == true)
                .filter_map(|line| line["id"].as_str().map(str::to_owned)),
        );
        if (gone.is_empty() && approved.is_empty()) || !self.graph_path(scope)?.exists() {
            return Ok(None);
        }
        let mut graph = self.load_graph(scope)?;
        let mut lines = graph.notes_changed(&gone, &undone, &approved, now);
        for line in &mut lines {
            if let Some(fields) = line.as_object_mut() {
                fields.insert("tenant".to_owned(), json!(scope.tenant));
                fields.insert("agent".to_owned(), json!(scope.agent));
                fields.insert("user_sha256".to_owned(), json!(scope.user.as_deref().map(sha256_hex)));
                fields.insert("actor".to_owned(), json!("graph"));
            }
        }
        Ok(Some((graph, lines)))
    }

    /// Refuse a new user of an agent that keeps notes for as many users as it may.
    fn check_user_room(&self, user_dir: &Path) -> Result<(), MemoryError> {
        let Some(users) = user_dir.parent() else { return Ok(()) };
        let count = match fs::read_dir(users) {
            Ok(entries) => entries.count(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(store_error("cannot list the users of an agent", &e)),
        };
        if count >= self.inner.limits.users {
            return Err(MemoryError::new(
                "memory_full",
                format!("this agent keeps notes for at most {} users", self.inner.limits.users),
            ));
        }
        Ok(())
    }

    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> Result<(), MemoryError> {
        let n = self.inner.counter.fetch_add(1, Ordering::Relaxed);
        let dir = path.parent().unwrap_or(&self.inner.root);
        let tmp = dir.join(format!(".notes.{}.{n}.tmp", std::process::id()));
        let result = (|| {
            let mut file =
                private_fs::create_file(&tmp).map_err(|e| store_error("cannot create a temporary file", &e))?;
            file.write_all(bytes).map_err(|e| store_error("cannot write a temporary file", &e))?;
            file.sync_all().map_err(|e| store_error("cannot sync a temporary file", &e))?;
            fs::rename(&tmp, path).map_err(|e| store_error("cannot replace a memory file", &e))
        })();
        if result.is_err() {
            let _cleanup = fs::remove_file(&tmp);
        }
        result
    }

    fn append_audit(&self, agent_dir: &Path, lines: &[Json], now: u64) -> Result<(), MemoryError> {
        if lines.iter().all(|line| line["action"] == "use") {
            return Ok(());
        }
        let mut out = Vec::new();
        // A use is written to the scope file, not to the audit log.
        for line in lines.iter().filter(|line| line["action"] != "use") {
            let mut line = line.clone();
            if let Some(fields) = line.as_object_mut() {
                fields.insert("ts_ms".to_owned(), json!(now));
            }
            serde_json::to_writer(&mut out, &line).map_err(|e| store_error("cannot encode the audit log", &e))?;
            out.push(b'\n');
        }
        let log = agent_dir.join("audit.jsonl");
        if fs::metadata(&log).is_ok_and(|meta| meta.len() > AUDIT_BYTES) {
            fs::rename(&log, agent_dir.join("audit.1.jsonl"))
                .map_err(|e| store_error("cannot rotate the audit log", &e))?;
        }
        let mut file = private_fs::append_file(&log).map_err(|e| store_error("cannot open the audit log", &e))?;
        file.write_all(&out).map_err(|e| store_error("cannot write the audit log", &e))?;
        file.sync_data().map_err(|e| store_error("cannot sync the audit log", &e))
    }

    async fn blocking<T: Send + 'static>(
        &self,
        job: impl FnOnce(&MemoryStore) -> Result<T, MemoryError> + Send + 'static,
    ) -> Result<T, MemoryError> {
        let store = self.clone();
        graphrt_core::task::spawn_blocking(move || job(&store))
            .await
            .map_err(|e| MemoryError::new("memory_store", format!("memory task failed: {e}")))?
    }

    /// Save a note through the gate. See [`MemoryBank::remember`].
    async fn save(&self, scope: MemoryScope, note: NewMemory) -> Result<MemoryRecord, MemoryError> {
        self.scope_path(&scope)?;
        let limits = self.inner.limits.clone();
        let original = clean_text(&note.text);
        if original.is_empty() {
            return Err(MemoryError::new("memory_invalid", "a note needs text"));
        }
        let chars = original.chars().count();
        if chars > limits.note_chars {
            return Err(MemoryError::new(
                "memory_too_long",
                format!("a note holds at most {} characters, this one has {chars}: shorten it", limits.note_chars),
            ));
        }
        let masked = mask_secrets(&original, &note.secrets);
        if masked.mostly_secret() {
            return Err(MemoryError::new(
                "memory_secret",
                "the note is mostly a secret (a password, a key, a token): secrets are never saved",
            ));
        }
        let text = masked.text;
        let mut tags: Vec<String> = Vec::new();
        for tag in &note.tags {
            let tag: String =
                mask_secrets(&clean_text(tag), &note.secrets).text.chars().take(limits.tag_chars).collect();
            if !tag.is_empty() && !tags.contains(&tag) && tags.len() < limits.tags {
                tags.push(tag);
            }
        }
        // The raw text too: cleaning joins lines, which hides a line that starts like a speaker.
        let instruction = reads_like_instruction(&text) || reads_like_instruction(&note.text);
        let mut status = decide(note.trust, note.policy, instruction);
        // A run never writes the agent's own memory directly: it proposes, the owner decides.
        if scope.user.is_none() && note.trust != MemoryTrust::Owner {
            status = MemoryStatus::Pending;
        }
        // Vectors are made at recall, for every active note that has none
        // (see `embed_missing`): a note approved later, or one written while
        // the embedder failed, gets one too.
        let kind = note.kind;
        let trust = note.trust;
        let source = note.source;
        let max = self.limit_of(&scope);
        self.blocking(move |store| {
            store.change(&scope, |file, now| {
                let expires_ms = match kind {
                    MemoryKind::Fact | MemoryKind::Preference => None,
                    MemoryKind::Event | MemoryKind::Note => Some(now.saturating_add(limits.short_lived_ms)),
                };
                let key = normalized(&text);
                // The same meaning as a stored note of the same kind updates it, unless
                // that would let a note waiting for review change one in use, or a
                // writer of less trust change a note of more.
                let same = file.notes.iter().position(|stored| {
                    let r = &stored.record;
                    r.kind == kind
                        && !(status == MemoryStatus::Pending && r.status == MemoryStatus::Active)
                        && trust_rank(trust) >= trust_rank(r.trust)
                        && (normalized(&r.text) == key || overlap(&r.text, &text) >= SAME_NOTE_OVERLAP)
                });
                let mut lines = Vec::new();
                let record = match same.and_then(|at| file.notes.get_mut(at)) {
                    Some(stored) => {
                        let r = &mut stored.record;
                        r.text.clone_from(&text);
                        for tag in &tags {
                            if !r.tags.contains(tag) && r.tags.len() < limits.tags {
                                r.tags.push(tag.clone());
                            }
                        }
                        let activated = status == MemoryStatus::Active && r.status == MemoryStatus::Pending;
                        if status == MemoryStatus::Active {
                            r.status = MemoryStatus::Active;
                        }
                        r.trust = trust;
                        r.updated_ms = now;
                        r.expires_ms = expires_ms;
                        r.source = source.clone();
                        // The text changed: the vector is made again at the next recall.
                        stored.vector.clear();
                        let mut line = audit_line("update", &stored.record, &scope);
                        if activated && let Some(fields) = line.as_object_mut() {
                            fields.insert("activated".to_owned(), Json::Bool(true));
                        }
                        lines.push(line);
                        stored.record.clone()
                    }
                    None => {
                        while file.notes.len() >= max {
                            match retire_one(file, &scope, &[], trust == MemoryTrust::Owner) {
                                Some(line) => lines.push(line),
                                None => {
                                    return Err(MemoryError::new(
                                        "memory_full",
                                        format!("this scope holds {max} notes and none can make room"),
                                    ));
                                }
                            }
                        }
                        let id = new_id(&text, now, store.inner.counter.fetch_add(1, Ordering::Relaxed));
                        let mut record = MemoryRecord::new(id, text.clone(), kind, status, trust, source.clone(), now);
                        record.tags.clone_from(&tags);
                        record.expires_ms = expires_ms;
                        lines.push(audit_line("remember", &record, &scope));
                        file.notes.push(Stored { record: record.clone(), vector: Vec::new() });
                        record
                    }
                };
                Ok((record, lines))
            })
        })
        .await
    }

    /// Search one scope (exactly that one).
    async fn search_scope(
        &self,
        scope: &MemoryScope,
        query: &str,
        query_vector: Option<&[f32]>,
        status: Option<MemoryStatus>,
    ) -> Result<Vec<(MemoryRecord, f32)>, MemoryError> {
        let path = self.scope_path(scope)?;
        let loaded = self.blocking(move |store| store.load(&path)).await?;
        let now = self.now();
        let alive = |stored: &Stored| {
            status.is_none_or(|status| stored.record.status == status)
                && stored.record.expires_ms.is_none_or(|expires| expires > now)
        };
        let notes = &loaded.file.notes;
        if query.trim().is_empty() {
            let mut all: Vec<(MemoryRecord, f32)> = notes
                .iter()
                .filter(|s| alive(s))
                .map(|s| (s.record.clone(), age_factor(s.record.last_used_ms, now)))
                .collect();
            all.sort_by_key(|a| std::cmp::Reverse(a.0.last_used_ms));
            return Ok(all);
        }
        let keyword: Vec<(usize, f32)> =
            loaded.index.search(query).into_iter().filter(|(doc, _)| notes.get(*doc).is_some_and(alive)).collect();
        let mut lists = vec![keyword];
        if let Some(query_vector) = query_vector {
            let mut similar: Vec<(usize, f32)> = notes
                .iter()
                .enumerate()
                .filter(|(_, s)| alive(s) && s.vector.len() == query_vector.len())
                .map(|(doc, s)| (doc, graphrt_rag::cosine(query_vector, &s.vector)))
                .filter(|(_, score)| *score >= 0.3)
                .collect();
            similar.sort_by(|a, b| b.1.total_cmp(&a.1));
            lists.push(similar);
        }
        let mut hits: Vec<(MemoryRecord, f32)> = fuse(&lists)
            .into_iter()
            .filter_map(|(doc, score)| notes.get(doc).map(|s| (s, score)))
            .map(|(s, score)| {
                let weight = age_factor(s.record.last_used_ms, now) * trust_factor(s.record.trust);
                (s.record.clone(), score * weight)
            })
            .collect();
        hits.sort_by(|a, b| b.1.total_cmp(&a.1));
        Ok(hits)
    }

    /// Mark `ids` of `scope` as used now. Best effort: recall works without it.
    ///
    /// A note used less than a minute ago is not marked again, so a busy agent
    /// does not rewrite its scope on every run.
    ///
    /// @returns Whether the scope could not be written (the caller reports it).
    async fn touch(&self, scope: MemoryScope, ids: Vec<String>) -> Result<(), MemoryError> {
        if ids.is_empty() {
            return Ok(());
        }
        self.blocking(move |store| {
            store.change(&scope, |file, now| {
                let mut touched = 0;
                for stored in file.notes.iter_mut().filter(|s| ids.contains(&s.record.id)) {
                    if now.saturating_sub(stored.record.last_used_ms) >= TOUCH_AFTER_MS || stored.record.uses == 0 {
                        stored.record.last_used_ms = now;
                        stored.record.uses = stored.record.uses.saturating_add(1);
                        touched += 1;
                    }
                }
                // A use is not a change the audit log records, but the file must be written.
                let lines = if touched > 0 { vec![json!({"action": "use", "count": touched})] } else { Vec::new() };
                Ok(((), lines))
            })
        })
        .await
    }

    /// Give the active notes of `scope` that have no vector one. Best effort,
    /// at most 32 notes a call: a note without a vector is still found by keyword.
    ///
    /// @returns How many notes got a vector.
    async fn embed_missing(&self, scope: &MemoryScope) -> Result<usize, MemoryError> {
        let Some(embedder) = self.inner.embedder.clone() else { return Ok(0) };
        if self.embedder_resting() {
            return Ok(0);
        }
        let path = self.scope_path(scope)?;
        let loaded = self.blocking(move |store| store.load(&path)).await?;
        let missing: Vec<(String, String)> = loaded
            .file
            .notes
            .iter()
            .filter(|s| s.record.status == MemoryStatus::Active && s.vector.is_empty())
            .take(32)
            .map(|s| (s.record.id.clone(), s.record.text.clone()))
            .collect();
        if missing.is_empty() {
            return Ok(0);
        }
        let texts: Vec<String> = missing.iter().map(|(_, text)| text.clone()).collect();
        let vectors = match embedder.embed_documents(&texts).await {
            Ok(vectors) if vectors.len() == texts.len() && vectors.iter().all(|v| !v.is_empty()) => vectors,
            Ok(_) => {
                self.rest_embedder();
                return Err(MemoryError::new("memory_embedder", "the embedder returned no vector for some notes"));
            }
            Err(e) => {
                self.rest_embedder();
                return Err(MemoryError::new("memory_embedder", format!("the embedder failed: {e}")));
            }
        };
        let made: Vec<(String, String, Vec<f32>)> = missing
            .into_iter()
            .zip(vectors)
            .filter(|(_, vector)| !vector.is_empty())
            .map(|((id, text), vector)| (id, text, vector))
            .collect();
        let count = made.len();
        let scope = scope.clone();
        self.blocking(move |store| {
            store.change(&scope, |file, _| {
                // Vectors take room: they stop where the scope would pass its bound.
                let mut bytes = serde_json::to_vec(&*file).map_or(0, |b| b.len());
                let mut stored_any = false;
                for (id, text, vector) in &made {
                    // As serde_json writes it: up to 15 bytes a number with its comma.
                    let size = vector.len() * 16 + 16;
                    if bytes + size > store.inner.limits.scope_bytes {
                        break;
                    }
                    // Only while the note still says what was embedded.
                    if let Some(stored) = file.notes.iter_mut().find(|s| s.record.id == *id && s.record.text == *text) {
                        stored.vector.clone_from(vector);
                        bytes += size;
                        stored_any = true;
                    }
                }
                // Nothing to write when no vector fit.
                Ok(((), if stored_any { vec![json!({"action": "use", "count": 0})] } else { Vec::new() }))
            })
        })
        .await?;
        Ok(count)
    }

    /// Graph facts of exactly `scope` about `query`, true at `at_ms`, each with
    /// the notes it came from. Scored by rank like the notes, so they fuse.
    #[cfg(feature = "graph")]
    async fn graph_hits(
        &self,
        scope: &MemoryScope,
        level: MemoryLevel,
        query: &str,
        at_ms: u64,
        limit: usize,
    ) -> Result<Vec<(MemoryHit, Vec<String>)>, MemoryError> {
        if !self.graph_path(scope)?.exists() {
            return Ok(Vec::new());
        }
        let (scope, query) = (scope.clone(), query.to_owned());
        let notes_path = self.scope_path(&scope)?;
        self.blocking(move |store| {
            let now = store.now();
            // A fact stands while one of its notes is in use and not expired.
            let notes = store.load(&notes_path)?;
            let live: std::collections::HashSet<&str> = notes
                .file
                .notes
                .iter()
                .filter(|s| s.record.status == MemoryStatus::Active && s.record.expires_ms.is_none_or(|e| e > now))
                .map(|s| s.record.id.as_str())
                .collect();
            let cached = store.cached_graph(&scope)?;
            let graph = &cached.graph;
            let alive = |r: &crate::graph::Relation| r.notes.iter().any(|n| live.contains(n.as_str()));
            let found = graph.search(&cached.index, &query, at_ms, limit, &alive);
            let top = found.first().map_or(1.0, |(_, score)| score.max(f32::EPSILON));
            Ok(found
                .into_iter()
                .enumerate()
                .filter_map(|(rank, (at, score))| {
                    let relation = graph.relations.get(at)?;
                    let source = MemorySource { actor: "graph".to_owned(), ..MemorySource::default() };
                    let record = MemoryRecord::new(
                        relation.id.clone(),
                        relation.sentence(),
                        MemoryKind::Fact,
                        MemoryStatus::Active,
                        relation.trust,
                        source,
                        relation.recorded_ms,
                    );
                    let fused = (score / top) / (61.0 + rank as f32);
                    Some((MemoryHit::fact(record, level, fused), relation.notes.clone()))
                })
                .collect())
        })
        .await
    }

    /// Whether the embedder failed less than a minute ago: it is not asked again yet.
    fn embedder_resting(&self) -> bool {
        let failed = self.inner.embedder_failed_ms.load(Ordering::Relaxed);
        failed > 0 && self.now().saturating_sub(failed) < EMBEDDER_REST_MS
    }

    fn rest_embedder(&self) {
        self.inner.embedder_failed_ms.store(self.now().max(1), Ordering::Relaxed);
    }

    /// The notes of exactly `scope`, newest first, for the owner.
    pub async fn list(&self, scope: &MemoryScope, filter: &ListFilter) -> Result<Vec<MemoryRecord>, MemoryError> {
        let query = filter.query.clone().unwrap_or_default();
        let mut hits = self.search_scope(scope, &query, None, filter.status).await?;
        if query.trim().is_empty() {
            hits.sort_by_key(|a| std::cmp::Reverse(a.0.updated_ms));
        }
        Ok(hits.into_iter().map(|(record, _)| record).collect())
    }

    /// Approve (`true`) or reject (`false`) a pending note. A rejected note is removed.
    pub async fn decide(
        &self,
        scope: &MemoryScope,
        id: &str,
        approve: bool,
        source: &MemorySource,
    ) -> Result<Option<MemoryRecord>, MemoryError> {
        let (scope, id, source) = (scope.clone(), id.to_owned(), source.clone());
        self.blocking(move |store| {
            store.change(&scope, |file, _| {
                let Some(at) = file.notes.iter().position(|s| s.record.id == id) else { return Ok((None, Vec::new())) };
                if file.notes.get(at).is_some_and(|s| s.record.status != MemoryStatus::Pending) {
                    return Err(MemoryError::new(
                        "memory_invalid",
                        "this note is not waiting for review (remove a note in use with forget)",
                    ));
                }
                let action = if approve { "approve" } else { "reject" };
                let record = if approve {
                    let stored = file.notes.get_mut(at).ok_or_else(|| MemoryError::new("memory_store", "lost"))?;
                    stored.record.status = MemoryStatus::Active;
                    stored.record.clone()
                } else {
                    file.notes.remove(at).record
                };
                let mut line = audit_line(action, &record, &scope);
                with_actor(&mut line, &source);
                Ok((Some(record), vec![line]))
            })
        })
        .await
    }

    /// Everything kept about one user of an agent.
    pub async fn export_user(&self, scope: &MemoryScope) -> Result<Json, MemoryError> {
        let path = self.scope_path(scope)?;
        let loaded = self.blocking(move |store| store.load(&path)).await?;
        let notes: Vec<&MemoryRecord> = loaded.file.notes.iter().map(|s| &s.record).collect();
        #[allow(unused_mut)]
        let mut export = json!({ "tenant": scope.tenant, "agent": scope.agent, "user": scope.user, "notes": notes });
        #[cfg(feature = "graph")]
        if let Some(fields) = export.as_object_mut() {
            match self.load_graph(scope) {
                Ok(graph) => {
                    fields.insert(
                        "graph".to_owned(),
                        json!({ "entities": graph.entities, "relations": graph.relations }),
                    );
                }
                // The notes are exported anyway, and the export says what is missing.
                Err(e) => {
                    fields.insert("graph_error".to_owned(), json!(e.kind));
                }
            }
        }
        Ok(export)
    }

    /// Remove everything kept about one user of an agent: the directory goes.
    /// Returns how many notes there were.
    pub async fn erase_user(&self, scope: &MemoryScope, source: &MemorySource) -> Result<usize, MemoryError> {
        let path = self.scope_path(scope)?;
        let agent_dir = self.agent_dir(scope.tenant.as_deref(), &scope.agent)?;
        let (scope, source) = (scope.clone(), source.clone());
        self.blocking(move |store| {
            let _writing = store.inner.writing.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if !agent_dir.exists() {
                return Ok(0);
            }
            let _lock = lock(&agent_dir)?;
            store.forget_cached(&path);
            // Counted when the file can be read: a damaged file must not stop an erase.
            let count = store.load(&path).ok().map(|loaded| loaded.file.notes.len());
            store.forget_cached(&path);
            let mut line = json!({
                "action": "erase_user", "tenant": scope.tenant, "agent": scope.agent,
                "user_sha256": scope.user.as_deref().map(sha256_hex), "count": count,
            });
            with_actor(&mut line, &source);
            store.append_audit(&agent_dir, &[line], store.now())?;
            #[cfg(feature = "graph")]
            if let Ok(graph) = store.graph_path(&scope) {
                store.inner.graphs.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&graph);
            }
            if let Some(user_dir) = path.parent() {
                match fs::remove_dir_all(user_dir) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(store_error("cannot erase a user", &e)),
                }
            }
            Ok(count.unwrap_or(0))
        })
        .await
    }

    /// The newest `limit` lines of an agent's audit log, oldest first.
    pub async fn audit(&self, tenant: Option<&str>, agent: &str, limit: usize) -> Result<Vec<Json>, MemoryError> {
        let dir = self.agent_dir(tenant, agent)?;
        self.blocking(move |_| {
            // Under the lock, so a rotation cannot happen between the two files.
            let _lock = if dir.exists() { Some(lock(&dir)?) } else { None };
            let mut lines: Vec<Json> = Vec::new();
            // The rotated log first: it holds the older lines.
            for name in ["audit.1.jsonl", "audit.jsonl"] {
                let text = match fs::read_to_string(dir.join(name)) {
                    Ok(text) => text,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(store_error("cannot read the audit log", &e)),
                };
                lines.extend(text.lines().filter_map(|line| serde_json::from_str::<Json>(line).ok()));
            }
            let skip = lines.len().saturating_sub(limit);
            Ok(lines.into_iter().skip(skip).collect())
        })
        .await
    }
}

fn lock(dir: &Path) -> Result<LockGuard, MemoryError> {
    let path = dir.join(".lock");
    #[cfg(windows)]
    let opened = private_fs::create_file(&path);
    #[cfg(not(windows))]
    let opened = private_fs::append_file(&path);
    let file = opened.map_err(|e| store_error("cannot open the memory lock", &e))?;
    file.lock().map_err(|e| store_error("cannot lock the memory directory", &e))?;
    Ok(LockGuard(file))
}

struct LockGuard(fs::File);

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Trust in order: a writer never changes the note of a writer above it.
fn trust_rank(trust: MemoryTrust) -> u8 {
    match trust {
        MemoryTrust::Owner => 4,
        MemoryTrust::UserStated => 3,
        MemoryTrust::ModelInferred => 2,
        MemoryTrust::ModelAfterTools => 1,
        MemoryTrust::ToolDerived => 0,
    }
}

fn new_id(text: &str, now: u64, n: u64) -> String {
    let hash = sha256_hex(&format!("{now}:{n}:{}:{text}", std::process::id()));
    format!("m_{}", hash.get(..20).unwrap_or(&hash))
}

/// One audit line for a change of `record`. Never the text: its hash and size only.
fn audit_line(action: &str, record: &MemoryRecord, scope: &MemoryScope) -> Json {
    json!({
        "action": action,
        "id": record.id,
        "tenant": scope.tenant,
        "agent": scope.agent,
        "user_sha256": scope.user.as_deref().map(sha256_hex),
        "actor": record.source.actor,
        "run_id": record.source.run_id,
        "node_id": record.source.node_id,
        "trust": record.trust.as_str(),
        "status": record.status.as_str(),
        "chars": record.text.chars().count(),
        "text_sha256": sha256_hex(&record.text),
    })
}

fn with_actor(line: &mut Json, source: &MemorySource) {
    if let Some(fields) = line.as_object_mut() {
        fields.insert("actor".to_owned(), json!(source.actor));
        fields.insert("run_id".to_owned(), json!(source.run_id));
    }
}

/// Drop expired notes, with an audit line each.
fn expire(file: &mut ScopeFile, now: u64, scope: &MemoryScope) -> Vec<Json> {
    let mut lines = Vec::new();
    file.notes.retain(|stored| {
        let alive = stored.record.expires_ms.is_none_or(|expires| expires > now);
        if !alive {
            lines.push(audit_line("expire", &stored.record, scope));
        }
        alive
    });
    lines
}

/// Retire the note used least and longest ago, pending notes first, never
/// one of `keep`, and the owner's notes only for the owner (`by_owner`).
fn retire_one(file: &mut ScopeFile, scope: &MemoryScope, keep: &[String], by_owner: bool) -> Option<Json> {
    let at = file
        .notes
        .iter()
        .enumerate()
        .filter(|(_, s)| !keep.contains(&s.record.id))
        .filter(|(_, s)| by_owner || s.record.trust != MemoryTrust::Owner)
        .min_by_key(|(_, s)| (s.record.status == MemoryStatus::Active, s.record.uses, s.record.last_used_ms))
        .map(|(at, _)| at)?;
    let stored = file.notes.remove(at);
    Some(audit_line("retire", &stored.record, scope))
}

impl MemoryBank for MemoryStore {
    fn remember<'a>(
        &'a self,
        scope: &'a MemoryScope,
        note: NewMemory,
    ) -> BoxFuture<'a, Result<MemoryRecord, MemoryError>> {
        Box::pin(self.save(scope.clone(), note))
    }

    fn recall<'a>(
        &'a self,
        scope: &'a MemoryScope,
        query: &'a MemoryQuery,
    ) -> BoxFuture<'a, Result<Vec<MemoryHit>, MemoryError>> {
        Box::pin(async move {
            let limits = &self.inner.limits;
            let limit = match query.limit {
                0 => limits.recall_notes,
                n => n.min(limits.recall_notes * 3),
            };
            let mut levels = vec![(scope.agent_level(), MemoryLevel::Agent)];
            if scope.user.is_some() {
                levels.insert(0, (scope.clone(), MemoryLevel::User));
            }
            // Without the embedder recall still works, by keyword: said, not hidden.
            let query_vector = match &self.inner.embedder {
                // The query first: when it fails, notes are not embedded either.
                Some(embedder) if !query.text.trim().is_empty() && !self.embedder_resting() => {
                    match embedder.embed_query(&query.text).await {
                        Ok(vector) if !vector.is_empty() => {
                            for (level_scope, _) in &levels {
                                if let Err(e) = self.embed_missing(level_scope).await {
                                    eprintln!(
                                        "warning: memory of `{}`: some notes have no vector yet: {e}",
                                        level_scope.agent
                                    );
                                    break;
                                }
                            }
                            Some(vector)
                        }
                        failed => {
                            self.rest_embedder();
                            let why = failed.err().map_or_else(|| "an empty vector".to_owned(), |e| e.to_string());
                            eprintln!(
                                "warning: memory of `{}`: recall by keyword only for a minute: {why}",
                                scope.agent
                            );
                            None
                        }
                    }
                }
                _ => None,
            };
            let mut hits: Vec<MemoryHit> = Vec::new();
            for (level_scope, level) in &levels {
                let found = self
                    .search_scope(level_scope, &query.text, query_vector.as_deref(), Some(MemoryStatus::Active))
                    .await?;
                hits.extend(found.into_iter().map(|(record, score)| MemoryHit::note(record, *level, score)));
            }
            hits.sort_by(|a, b| b.score.total_cmp(&a.score));
            if query.fill_recent && hits.len() < limit && !query.text.trim().is_empty() {
                let mut recent: Vec<MemoryHit> = Vec::new();
                for (level_scope, level) in &levels {
                    let found = self.search_scope(level_scope, "", None, Some(MemoryStatus::Active)).await?;
                    // After every match, newest use first.
                    recent
                        .extend(found.into_iter().map(|(record, score)| MemoryHit::note(record, *level, score * 1e-6)));
                }
                recent.sort_by_key(|a| std::cmp::Reverse(a.record.last_used_ms));
                for hit in recent {
                    if !hits.iter().any(|h| h.record.id == hit.record.id) {
                        hits.push(hit);
                    }
                }
            }
            #[allow(unused_mut)]
            let mut fact_notes: HashMap<String, Vec<String>> = HashMap::new();
            #[cfg(feature = "graph")]
            if !query.text.trim().is_empty() {
                let at = query.as_of_ms.unwrap_or_else(|| self.now());
                for (level_scope, level) in &levels {
                    // The graph is derived from the notes: when it cannot be read, recall goes on without it.
                    let facts = match self.graph_hits(level_scope, *level, &query.text, at, limit).await {
                        Ok(facts) => facts,
                        Err(e) => {
                            eprintln!("warning: memory graph of `{}` not read: {}", level_scope.agent, e.kind);
                            Vec::new()
                        }
                    };
                    for (mut fact, notes) in facts {
                        fact_notes.insert(fact.record.id.clone(), notes.clone());
                        // A fact and the note it came from count once: the fact, which
                        // knows its period, takes the place (and the score) of its note.
                        let mut replaced = 0.0_f32;
                        hits.retain(|h| {
                            let source = h.kind == graphrt_core::memory::HitKind::Note && notes.contains(&h.record.id);
                            if source {
                                replaced = replaced.max(h.score);
                            }
                            !source
                        });
                        fact.score = fact.score.max(replaced);
                        hits.push(fact);
                    }
                }
                hits.sort_by(|a, b| b.score.total_cmp(&a.score));
            }
            let mut chars = 0;
            let mut kept = Vec::new();
            for hit in hits {
                let size = hit.record.text.chars().count();
                if kept.len() >= limit || chars + size > limits.recall_chars {
                    break;
                }
                chars += size;
                kept.push(hit);
            }
            if query.touch {
                for (level_scope, level) in levels {
                    // A fact counts as a use of the notes it stands on.
                    let ids: Vec<String> = kept
                        .iter()
                        .filter(|h| h.level == level)
                        .flat_map(|h| {
                            fact_notes.get(&h.record.id).cloned().unwrap_or_else(|| vec![h.record.id.clone()])
                        })
                        .collect();
                    // Recall works without it: the order of eviction is a little less exact.
                    if let Err(e) = self.touch(level_scope, ids).await {
                        eprintln!("warning: memory of `{}`: recalled notes not marked as used: {e}", scope.agent);
                    }
                }
            }
            Ok(kept)
        })
    }

    #[cfg(feature = "graph")]
    fn keeps_graph(&self) -> bool {
        true
    }

    #[cfg(feature = "graph")]
    fn known_entities<'a>(
        &'a self,
        scope: &'a MemoryScope,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<String>, MemoryError>> {
        let scope = scope.clone();
        Box::pin(self.blocking(move |store| Ok(store.load_graph(&scope)?.names(limit))))
    }

    #[cfg(feature = "graph")]
    fn link<'a>(
        &'a self,
        scope: &'a MemoryScope,
        note_id: &'a str,
        facts: graphrt_core::memory::ExtractedFacts,
    ) -> BoxFuture<'a, Result<usize, MemoryError>> {
        let (scope, note_id) = (scope.clone(), note_id.to_owned());
        Box::pin(self.blocking(move |store| {
            let path = store.scope_path(&scope)?;
            let agent_dir = store.agent_dir(scope.tenant.as_deref(), &scope.agent)?;
            let _writing = store.inner.writing.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if !path.exists() {
                return Err(MemoryError::new("memory_not_found", "no such note in this scope"));
            }
            let _lock = lock(&agent_dir)?;
            store.forget_cached(&path);
            let loaded = store.load(&path)?;
            // Only a note that is still there: what was removed meanwhile adds nothing.
            let note = loaded
                .file
                .notes
                .iter()
                .find(|s| s.record.id == note_id)
                .map(|s| s.record.clone())
                .ok_or_else(|| MemoryError::new("memory_not_found", "no such note in this scope"))?;
            let mut graph = store.load_graph(&scope)?;
            let now = store.now();
            let counter = &store.inner.counter;
            let mut id = || new_id(&note_id, now, counter.fetch_add(1, Ordering::Relaxed)).replacen("m_", "g_", 1);
            let (added, mut lines) = graph.link(&note, &facts, now, &mut id);
            if lines.is_empty() {
                return Ok(0);
            }
            for line in &mut lines {
                if let Some(fields) = line.as_object_mut() {
                    fields.insert("tenant".to_owned(), json!(scope.tenant));
                    fields.insert("agent".to_owned(), json!(scope.agent));
                    fields.insert("user_sha256".to_owned(), json!(scope.user.as_deref().map(sha256_hex)));
                    fields.insert("actor".to_owned(), json!("graph"));
                }
            }
            store.append_audit(&agent_dir, &lines, now)?;
            store.write_graph(&scope, &graph)?;
            Ok(added)
        }))
    }

    fn forget<'a>(
        &'a self,
        scope: &'a MemoryScope,
        target: &'a ForgetTarget,
        source: &'a MemorySource,
    ) -> BoxFuture<'a, Result<usize, MemoryError>> {
        let (scope, target, source) = (scope.clone(), target.clone(), source.clone());
        Box::pin(async move {
            self.scope_path(&scope)?;
            self.blocking(move |store| {
                store.change(&scope, |file, _| {
                    let query_words: Vec<String> = match &target {
                        ForgetTarget::Query(text) => crate::search::words(text),
                        ForgetTarget::Id(_) => Vec::new(),
                    };
                    let matches = |record: &MemoryRecord| match &target {
                        // A run never removes the owner's notes: the owner and the user do.
                        _ if source.actor == "agent" && record.trust == MemoryTrust::Owner => false,
                        ForgetTarget::Id(id) => record.id == *id,
                        ForgetTarget::Query(_) => {
                            let words = crate::search::words(&record.text);
                            !query_words.is_empty() && query_words.iter().all(|w| words.contains(w))
                        }
                    };
                    let mut lines = Vec::new();
                    file.notes.retain(|stored| {
                        if matches(&stored.record) {
                            let mut line = audit_line("forget", &stored.record, &scope);
                            with_actor(&mut line, &source);
                            lines.push(line);
                            false
                        } else {
                            true
                        }
                    });
                    Ok((lines.len(), lines))
                })
            })
            .await
        })
    }
}
