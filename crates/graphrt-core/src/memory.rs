//! Memory contract between a memory store and the nodes that remember.
//!
//! An agent saves short notes about a user or about its own work and finds
//! them again in later runs. This module holds only the contract. A host
//! registers a [`Memories`] service (for example the store of the
//! `graphrt-memory` crate) and the `agent` node offers it to the model as the
//! tools `memory/remember`, `memory/recall` and `memory/forget`.
//!
//! Isolation is the store's job, not the caller's: every operation names one
//! exact [`MemoryScope`] and a store never returns or changes a note of any
//! other scope. The scope of a run comes from the node configuration and the
//! verified user, never from what the model passes to a tool.
//!
//! Saved text is data. A store returns it as [`MemoryHit`]s and the node hands
//! it to the model as data, never as instructions. The design and the threat
//! model are in `docs/memory-design.md`.

use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

/// Whose memory an operation touches. Every level is exact: no wildcards.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MemoryScope {
    /// The tenant. `None` is the single tenant of a server without tenants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// The agent the memory belongs to.
    pub agent: String,
    /// The user. `None` addresses the agent's own memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

impl MemoryScope {
    /// The agent's own memory, shared by every run of the agent.
    pub fn agent(tenant: Option<String>, agent: impl Into<String>) -> Self {
        Self { tenant, agent: agent.into(), user: None }
    }

    /// The memory of one user of the agent.
    pub fn user(tenant: Option<String>, agent: impl Into<String>, user: impl Into<String>) -> Self {
        Self { tenant, agent: agent.into(), user: Some(user.into()) }
    }

    /// The same tenant and agent without the user: the agent's own memory.
    pub fn agent_level(&self) -> Self {
        Self { tenant: self.tenant.clone(), agent: self.agent.clone(), user: None }
    }

    /// Check every level: the rule that keeps a scope inside its directory.
    /// Every [`MemoryBank`] calls it before it touches anything.
    ///
    /// @throws MemoryError - `memory_scope` for a name or user id that breaks the rule.
    pub fn validate(&self) -> Result<(), MemoryError> {
        check_name("agent", &self.agent)?;
        if let Some(tenant) = &self.tenant {
            check_name("tenant", tenant)?;
        }
        if let Some(user) = &self.user {
            check_user(user)?;
        }
        Ok(())
    }
}

/// The rule for agent and tenant names, the one lessons use: 1 to 128
/// letters, digits, `.`, `_` or `-`, not starting with `.`.
///
/// @throws MemoryError - `memory_scope` when `name` breaks it.
pub fn check_name(what: &str, name: &str) -> Result<(), MemoryError> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(MemoryError::new(
            "memory_scope",
            format!("{what} `{name}` must be 1 to 128 letters, digits, `.`, `_` or `-`, not starting with `.`"),
        ))
    }
}

/// The rule for user ids: any text a person can read, up to 256 characters.
/// A user id of a scope carries its [`UserSource`] in front (`channel/shop:42`),
/// and the rule then holds for the id after it. A user id never becomes a
/// path: stores hash it.
///
/// @throws MemoryError - `memory_scope` when `user` breaks it.
pub fn check_user(user: &str) -> Result<(), MemoryError> {
    let id = UserSource::split(user).map_or(user, |(_, id)| id);
    let ok = !id.trim().is_empty() && id.chars().count() <= 256 && !id.chars().any(char::is_control);
    if ok { Ok(()) } else { Err(MemoryError::new("memory_scope", "a user id must be 1 to 256 visible characters")) }
}

/// The `sys` variable in which a host says where the user of a run comes from
/// (`sys.user_source`): `user`, `api` or `channel`, see [`UserSource`]. Hosts
/// set it over anything a client sent, and a run without it is an `api` run.
pub const USER_SOURCE_VAR: &str = "user_source";

/// Where the user id of a run comes from. It is part of every user scope, so
/// the same id from two sources (the number `42` on Telegram and the subject
/// `42` of a login) names two people with two memories.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UserSource {
    /// The subject of a token the server or its gateway verified (`--verify-jwt`),
    /// or a chat guest (`guest:<id>`, ids no identity provider may issue).
    Verified,
    /// An id the holder of the API key named, or the host itself (`graphrt run`,
    /// an app that embeds the runtime): the owner speaks for its users.
    Api,
    /// A customer of a messaging channel. The id is `<channel name>:<customer
    /// id>`: the name stands for the bot account (one Telegram bot, one WhatsApp
    /// number, one Slack workspace, one Teams app), whose ids are unique.
    Channel,
}

impl UserSource {
    const ALL: [Self; 3] = [Self::Verified, Self::Api, Self::Channel];

    /// The word in `sys.user_source` and in front of a scoped user id.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "user",
            Self::Api => "api",
            Self::Channel => "channel",
        }
    }

    /// The source a word names, `None` for any other word.
    pub fn parse(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|source| source.as_str() == word)
    }

    /// The user id of a scope for `id` of this source: `<source>/<id>`.
    pub fn scoped(self, id: &str) -> String {
        format!("{}/{id}", self.as_str())
    }

    /// The source and the id of a scoped user id, `None` for an id without a
    /// source (one stored before ids carried their source).
    pub fn split(user: &str) -> Option<(Self, &str)> {
        let (word, id) = user.split_once('/')?;
        Some((Self::parse(word)?, id))
    }
}

/// The scope user an owner means by `user` (`graphrt memory --user`, `?user=`
/// of `/v1/memory`): a scoped id as it is (`channel/shop:42`, `user/<sub>`),
/// and any other id as one the owner names, as a run started with the API key
/// does (`api/<id>`).
pub fn owner_named_user(user: &str) -> String {
    match UserSource::split(user) {
        Some(_) => user.to_owned(),
        None => UserSource::Api.scoped(user),
    }
}

/// The id a scoped user id was stored under before ids carried their source:
/// the id without it (`shop:42` for `channel/shop:42`). A store moves notes
/// it finds under that id to the scoped one the first time the scoped one is
/// used, and an erase of the scoped id erases them there too.
pub fn legacy_user(user: &str) -> Option<&str> {
    UserSource::split(user).map(|(_, id)| id)
}

/// What a note is about. It decides how long a note lives by default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// Something true about the user or the world ("works at Acme").
    Fact,
    /// How the user wants things done ("invoices as PDF").
    Preference,
    /// Something that happened ("ordered 3 chairs on 2026-11-02").
    Event,
    /// Anything else the agent wants to find again.
    #[default]
    Note,
}

impl MemoryKind {
    /// The word used in documents and tool arguments.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Preference => "preference",
            Self::Event => "event",
            Self::Note => "note",
        }
    }
}

/// Where the text of a note came from, as the writer judged it. The store
/// decides from it whether a note is used at once or waits for review.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryTrust {
    /// Written by the owner (CLI, HTTP API).
    Owner,
    /// Mostly words the user wrote in the run.
    UserStated,
    /// The model's own words, with no outside content in the run.
    ModelInferred,
    /// The model's own words in a run that read outside content (tool
    /// results): they may carry text planted there.
    ModelAfterTools,
    /// Mostly words of a tool result (a web page, a file, an API).
    ToolDerived,
}

impl MemoryTrust {
    /// The word used in records and events.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::UserStated => "user_stated",
            Self::ModelInferred => "model_inferred",
            Self::ModelAfterTools => "model_after_tools",
            Self::ToolDerived => "tool_derived",
        }
    }
}

/// Whether a note is used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    /// Returned by recall.
    Active,
    /// Stored, never returned by recall until the owner (or the user, for
    /// their own notes) approves it.
    Pending,
}

impl MemoryStatus {
    /// The word used in records and events.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Pending => "pending",
        }
    }
}

/// How writes from a run are decided (`long_term_memory.write`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WritePolicy {
    /// Notes in the user's or the model's own words are used at once. Notes
    /// built from outside content wait for review.
    #[default]
    Guarded,
    /// Every note waits for review.
    Review,
    /// Every note is used at once, except text that reads like an instruction.
    Open,
}

/// Who wrote a note and in which run. Kept with the note and in the audit log.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySource {
    /// `agent` for a run, `owner` for the CLI and the HTTP API, `user` for the
    /// verified user's own API.
    pub actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
}

/// A note to save. Build it with [`NewMemory::new`]: fields may be added.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NewMemory {
    pub text: String,
    pub kind: MemoryKind,
    pub tags: Vec<String>,
    /// Where the text came from, as the writer judged it.
    pub trust: MemoryTrust,
    /// How the store decides between active and pending.
    pub policy: WritePolicy,
    pub source: MemorySource,
    /// Values that must never be stored, such as the secrets of the run. The
    /// store masks them before anything else looks at the text.
    pub secrets: Vec<String>,
}

impl NewMemory {
    /// A note of kind `note`, without tags, decided by `guarded`.
    pub fn new(text: impl Into<String>, trust: MemoryTrust, source: MemorySource) -> Self {
        Self {
            text: text.into(),
            kind: MemoryKind::Note,
            tags: Vec::new(),
            trust,
            policy: WritePolicy::Guarded,
            source,
            secrets: Vec::new(),
        }
    }

    pub fn kind(mut self, kind: MemoryKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn tags(mut self, tags: Vec<String>) -> Self {
        self.tags = tags;
        self
    }

    pub fn policy(mut self, policy: WritePolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn secrets(mut self, secrets: Vec<String>) -> Self {
        self.secrets = secrets;
        self
    }
}

/// The secrets are never printed.
impl fmt::Debug for NewMemory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewMemory")
            .field("text", &self.text)
            .field("kind", &self.kind)
            .field("tags", &self.tags)
            .field("trust", &self.trust)
            .field("policy", &self.policy)
            .field("source", &self.source)
            .field("secrets", &format_args!("[{} masked]", self.secrets.len()))
            .finish()
    }
}

/// A saved note as a store returns it. Fields may be added.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MemoryRecord {
    pub id: String,
    pub text: String,
    pub kind: MemoryKind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub status: MemoryStatus,
    pub trust: MemoryTrust,
    pub created_ms: u64,
    pub updated_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_ms: Option<u64>,
    #[serde(default)]
    pub last_used_ms: u64,
    #[serde(default)]
    pub uses: u64,
    pub source: MemorySource,
}

impl MemoryRecord {
    /// A note written at `now_ms`, not used yet, without tags or expiry.
    pub fn new(
        id: impl Into<String>,
        text: impl Into<String>,
        kind: MemoryKind,
        status: MemoryStatus,
        trust: MemoryTrust,
        source: MemorySource,
        now_ms: u64,
    ) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            kind,
            tags: Vec::new(),
            status,
            trust,
            created_ms: now_ms,
            updated_ms: now_ms,
            expires_ms: None,
            last_used_ms: now_ms,
            uses: 0,
            source,
        }
    }
}

/// A search over the notes of one scope. Build it with [`MemoryQuery::new`]:
/// fields may be added (a point in time for graph facts, for example).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct MemoryQuery {
    /// Words to look for. Empty returns the most recently used notes.
    pub text: String,
    /// At most this many hits. 0 means the store's default; the store caps it.
    pub limit: usize,
    /// Count the hits as used (recall from a run). Listing by the owner does not.
    pub touch: bool,
    /// When fewer notes match than `limit`, add the most recently used
    /// active notes after them (the recall at the start of a run).
    pub fill_recent: bool,
    /// Graph facts true at this time (milliseconds since the epoch). `None`: now.
    pub as_of_ms: Option<u64>,
}

impl MemoryQuery {
    /// A search for `text` with the store's default limit, not counted as a use.
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into(), ..Self::default() }
    }

    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Count the hits as used.
    pub fn touch(mut self) -> Self {
        self.touch = true;
        self
    }

    /// Add recently used notes after the matches.
    pub fn fill_recent(mut self) -> Self {
        self.fill_recent = true;
        self
    }

    /// Graph facts true at `ms` instead of now.
    pub fn as_of(mut self, ms: u64) -> Self {
        self.as_of_ms = Some(ms);
        self
    }
}

/// Entities and relations a model read out of one note, for the memory graph.
/// What a model writes is untrusted: a store cleans and bounds every field.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ExtractedFacts {
    #[serde(default)]
    pub entities: Vec<ExtractedEntity>,
    #[serde(default)]
    pub relations: Vec<ExtractedRelation>,
}

/// One entity of [`ExtractedFacts`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ExtractedEntity {
    pub name: String,
    /// `person`, `organization`, `place`, `product`, `event` or `other`.
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// One relation of [`ExtractedFacts`]: `source relation target`, for example
/// `Alice works_at Acme`, with the period the text says it is true.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ExtractedRelation {
    pub source: String,
    pub relation: String,
    pub target: String,
    /// `YYYY-MM-DD` when the text says since when.
    #[serde(default)]
    pub valid_from: Option<String>,
    /// `YYYY-MM-DD` when the text says until when.
    #[serde(default)]
    pub valid_to: Option<String>,
    /// The text says this replaces what was true before ("moved to", "now works at").
    #[serde(default)]
    pub replaces: bool,
}

/// What a hit is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HitKind {
    /// A saved note.
    Note,
    /// A fact of the memory graph.
    Fact,
}

/// The level of a scope a note lives at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryLevel {
    /// The notes of one user.
    User,
    /// The agent's own notes, read by every run of the agent.
    Agent,
}

/// One result of [`MemoryBank::recall`]. Fields may be added (the subject
/// and object of a graph fact, for example).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MemoryHit {
    pub kind: HitKind,
    /// The note, or the note a graph fact came from.
    pub record: MemoryRecord,
    /// Which level the note belongs to.
    pub level: MemoryLevel,
    /// Higher is better. Only the order means something.
    pub score: f32,
}

impl MemoryHit {
    /// A note of `level` found with `score`.
    pub fn note(record: MemoryRecord, level: MemoryLevel, score: f32) -> Self {
        Self { kind: HitKind::Note, record, level, score }
    }

    /// A fact of the memory graph, as a record whose text says it with its period.
    pub fn fact(record: MemoryRecord, level: MemoryLevel, score: f32) -> Self {
        Self { kind: HitKind::Fact, record, level, score }
    }
}

/// What [`MemoryBank::forget`] removes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForgetTarget {
    /// The note with this id.
    Id(String),
    /// Every note that matches these words well.
    Query(String),
}

/// A memory operation failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct MemoryError {
    /// Stable error kind, for example `memory_limit` or `memory_store`.
    pub kind: &'static str,
    /// What went wrong, safe to show to the model and the owner.
    pub message: String,
}

impl MemoryError {
    pub fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
}

/// A store of notes, isolated per scope.
pub trait MemoryBank: Send + Sync + fmt::Debug {
    /// Save a note in `scope`, through the store's write gate. A note with
    /// the same meaning as a stored one updates it.
    fn remember<'a>(
        &'a self,
        scope: &'a MemoryScope,
        note: NewMemory,
    ) -> BoxFuture<'a, Result<MemoryRecord, MemoryError>>;

    /// Search the active notes of `scope` and, when `scope` names a user, of
    /// the agent's own memory too. Pending notes are never returned.
    fn recall<'a>(
        &'a self,
        scope: &'a MemoryScope,
        query: &'a MemoryQuery,
    ) -> BoxFuture<'a, Result<Vec<MemoryHit>, MemoryError>>;

    /// Remove notes of exactly `scope`. Returns how many were removed.
    fn forget<'a>(
        &'a self,
        scope: &'a MemoryScope,
        target: &'a ForgetTarget,
        source: &'a MemorySource,
    ) -> BoxFuture<'a, Result<usize, MemoryError>>;

    /// Whether this store keeps a memory graph ([`MemoryBank::link`] works).
    fn keeps_graph(&self) -> bool {
        false
    }

    /// The names of entities the graph of `scope` already knows, for an
    /// extraction to reuse (at most `limit`, the most connected first).
    fn known_entities<'a>(
        &'a self,
        scope: &'a MemoryScope,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<String>, MemoryError>> {
        let _ = (scope, limit);
        Box::pin(async { Ok(Vec::new()) })
    }

    /// Add what was read out of note `note_id` of `scope` to its graph. The
    /// relations take the status and trust of the note: a note waiting for
    /// review gives relations that wait too. Returns how many relations are new.
    fn link<'a>(
        &'a self,
        scope: &'a MemoryScope,
        note_id: &'a str,
        facts: ExtractedFacts,
    ) -> BoxFuture<'a, Result<usize, MemoryError>> {
        let _ = (scope, note_id, facts);
        Box::pin(async { Err(MemoryError::new("memory_graph", "this memory store keeps no graph")) })
    }
}

/// Service registered in [`crate::node::Services`] by the host, per workflow:
/// what a node cannot know about its run. A request never sets it, so a
/// client cannot move a run into another tenant.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryDefaults {
    /// The tenant of every scope of the workflow's runs.
    pub tenant: Option<String>,
    /// The agent name used when `long_term_memory` names none: the workflow's name.
    pub agent: Option<String>,
}

/// Service registered in [`crate::node::Services`]: where nodes find memory.
#[derive(Clone, Debug)]
pub struct Memories(pub Arc<dyn MemoryBank>);

impl Memories {
    /// Wrap a memory store as a service.
    pub fn new(bank: Arc<dyn MemoryBank>) -> Self {
        Self(bank)
    }
}

/// The lower case words of `text`: runs of letters and digits, and each CJK
/// character as a word of its own (those scripts have no spaces).
pub fn tokens(text: &str) -> Vec<String> {
    let cjk = |c: char| matches!(u32::from(c), 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF);
    let mut out = Vec::new();
    let mut word = String::new();
    for c in text.chars() {
        if cjk(c) {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            out.push(c.to_string());
        } else if c.is_alphanumeric() {
            word.extend(c.to_lowercase());
        } else if !word.is_empty() {
            out.push(std::mem::take(&mut word));
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

/// The words of `text`, each reduced to a common stem so that "invoices"
/// finds "invoice" and "monthly" finds "month".
pub fn words(text: &str) -> Vec<String> {
    tokens(text).into_iter().map(|word| stem(&word)).collect()
}

/// A light stemmer for English and Indonesian: a few common suffixes only,
/// and never below three characters. It errs on the side of leaving a word
/// alone, so two different words rarely meet.
pub fn stem(word: &str) -> String {
    if !word.is_ascii() || word.len() <= 3 {
        return word.to_owned();
    }
    const SUFFIXES: [&str; 12] = ["ingly", "edly", "ing", "ies", "ied", "ly", "ed", "es", "s", "nya", "kan", "lah"];
    for suffix in SUFFIXES {
        if let Some(base) = word.strip_suffix(suffix) {
            if base.len() < 3 || (suffix == "s" && (base.ends_with('s') || base.ends_with('u') || base.ends_with('i')))
            {
                continue;
            }
            // "es" only after a sibilant (boxes, wishes): "invoices" keeps its "e".
            if suffix == "es" && !["x", "sh", "ch", "ss", "z"].iter().any(|end| base.ends_with(end)) {
                continue;
            }
            return match suffix {
                "ies" | "ied" => format!("{base}y"),
                _ => base.to_owned(),
            };
        }
    }
    word.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scoped_user_id_names_its_source_and_keeps_the_id_whole() {
        assert_eq!(UserSource::Channel.scoped("shop:42"), "channel/shop:42");
        assert_eq!(UserSource::Verified.scoped("a/b"), "user/a/b", "an id may hold slashes");
        assert_eq!(UserSource::split("user/a/b"), Some((UserSource::Verified, "a/b")));
        assert_eq!(UserSource::split("api/42"), Some((UserSource::Api, "42")));
        assert_eq!(UserSource::split("42"), None, "an id stored before sources");
        assert_eq!(UserSource::split("team/42"), None, "an unknown word is part of an old id");
        assert_eq!(legacy_user("channel/shop:42"), Some("shop:42"));
        assert_eq!(legacy_user("shop:42"), None);
        assert_eq!(owner_named_user("alice"), "api/alice", "what the key holder names");
        assert_eq!(owner_named_user("channel/shop:42"), "channel/shop:42");
        assert_eq!(owner_named_user("team/42"), "api/team/42");
        for source in UserSource::ALL {
            assert_eq!(UserSource::parse(source.as_str()), Some(source));
        }
        assert_eq!(UserSource::parse("User"), None);
    }

    #[test]
    fn the_same_id_of_two_sources_is_two_scopes() {
        let ids: std::collections::HashSet<String> =
            UserSource::ALL.into_iter().map(|source| source.scoped("42")).collect();
        assert_eq!(ids.len(), 3);
        assert!(!ids.contains("42"));
    }

    #[test]
    fn the_length_rule_holds_for_the_id_after_the_source() {
        let longest = "x".repeat(256);
        assert!(check_user(&UserSource::Channel.scoped(&longest)).is_ok());
        assert!(check_user(&UserSource::Channel.scoped(&format!("{longest}x"))).is_err());
        assert!(check_user(&longest).is_ok());
        assert!(check_user("api/ ").is_err(), "an empty id after the source");
        assert!(check_user("api/a b").is_ok());
        assert!(check_user("api/a\u{7}b").is_err());
    }

    #[test]
    fn the_stemmer_joins_forms_of_a_word_and_leaves_others_alone() {
        for (a, b) in [
            ("invoices", "invoice"),
            ("monthly", "month"),
            ("orders", "ordered"),
            ("boxes", "box"),
            ("cities", "city"),
            ("pesanannya", "pesanan"),
            ("kirimkan", "kirim"),
        ] {
            assert_eq!(stem(a), stem(b), "{a} {b}");
        }
        for word in ["class", "bus", "this", "pdf", "日本"] {
            assert_eq!(stem(word), word, "{word}");
        }
    }

    #[test]
    fn tokens_split_words_and_cjk() {
        assert_eq!(tokens("Hello, World 42"), ["hello", "world", "42"]);
        assert_eq!(tokens("東京へ行く"), ["東", "京", "へ", "行", "く"]);
    }
}
