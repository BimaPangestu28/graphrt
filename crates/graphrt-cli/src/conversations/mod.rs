//! Conversations kept by the server (`serve --conversations`, cargo feature
//! `conversations`).
//!
//! Without them a chat client sends the earlier turns with every request
//! (`sys.history`), which stays supported unchanged. With them a client creates
//! a conversation once and names it in each run (`conversation_id`):
//!
//! - the stored turns become the run's `sys.history`, the stored conversation
//!   variables its `conversation` namespace, and `sys.conversation_id` its id;
//! - when the run succeeds, the message and the answer are added as a turn and
//!   the conversation variables the run wrote are stored. A failed or aborted
//!   run adds nothing. A paused run adds its turn when a resume succeeds.
//!
//! A conversation belongs to who created it: a verified end user (`sub` of
//! their token, or a chat guest) reaches only their own, and holders of the API
//! key reach all of them. Limits ([`Limits`]) bound what one user can store, and
//! conversations not used for `--conversation-ttl-days` are deleted.
//!
//! The link between a run and its conversation is the run record's
//! `session_id`, so traces group by conversation without more configuration.

mod api;
mod store;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use graphrt_core::store::now_ms;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

pub(crate) use api::{join_run, routes};
pub use store::{ConversationError, ConversationStore, FileConversations, MemoryConversations};

use crate::error::CliError;

/// Bytes of message text kept per conversation. Beyond it the oldest turns are
/// dropped, like beyond `--conversation-max-messages`.
pub const MAX_CONVERSATION_BYTES: usize = 4 * 1024 * 1024;
/// Runs of one conversation that may wait for their answer at once.
const MAX_PENDING_TURNS: usize = 16;
/// Characters of the first message used as the title.
const TITLE_CHARS: usize = 80;

/// Who wrote a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// One message of a conversation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// The run that took the message or gave the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub created_ms: u64,
}

/// A message whose run has not finished yet.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingTurn {
    pub run_id: String,
    pub content: String,
    pub created_ms: u64,
}

/// A stored conversation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    /// The verified end user it belongs to; `None` when an API-key caller created it.
    #[serde(default)]
    pub owner: Option<String>,
    pub workflow: String,
    /// The start input that takes each message; `None` when it is `sys.query`.
    #[serde(default)]
    pub input: Option<String>,
    #[serde(default)]
    pub title: String,
    pub created_ms: u64,
    pub updated_ms: u64,
    #[serde(default)]
    pub messages: Vec<Message>,
    /// The `conversation` variables as the last finished run left them.
    #[serde(default)]
    pub variables: IndexMap<String, serde_json::Value>,
    #[serde(default)]
    pub pending: Vec<PendingTurn>,
}

impl Conversation {
    /// An empty conversation with `workflow`.
    pub fn new(id: &str, owner: Option<String>, workflow: &str, input: Option<String>, now_ms: u64) -> Self {
        Self {
            id: id.to_owned(),
            owner,
            workflow: workflow.to_owned(),
            input,
            title: String::new(),
            created_ms: now_ms,
            updated_ms: now_ms,
            messages: Vec::new(),
            variables: IndexMap::new(),
            pending: Vec::new(),
        }
    }

    /// Whether `user` may use it: its owner, or (`None`) a holder of the API key.
    pub fn is_reachable_by(&self, user: Option<&str>) -> bool {
        user.is_none() || self.owner.as_deref() == user
    }

    /// The stored turns as `sys.history`: `[{role, content}]`, oldest first.
    pub fn history(&self) -> serde_json::Value {
        let role = |role| if role == Role::User { "user" } else { "assistant" };
        self.messages
            .iter()
            .map(|message| serde_json::json!({ "role": role(message.role), "content": message.content }))
            .collect()
    }

    /// Remember the message of a run that has just started.
    fn begin_turn(&mut self, run_id: &str, content: &str, now_ms: u64) {
        if self.title.is_empty() {
            self.title = title_of(content);
        }
        self.pending.push(PendingTurn { run_id: run_id.to_owned(), content: content.to_owned(), created_ms: now_ms });
        if self.pending.len() > MAX_PENDING_TURNS {
            let excess = self.pending.len() - MAX_PENDING_TURNS;
            self.pending.drain(..excess);
        }
        self.updated_ms = now_ms;
    }

    /// Add the turn of `run_id`, whose run succeeded with `answer`, and keep the
    /// conversation within `limits`. Does nothing when the run has no pending message.
    fn complete_turn(&mut self, run_id: &str, answer: &str, now_ms: u64, limits: &Limits) {
        let Some(position) = self.pending.iter().position(|turn| turn.run_id == run_id) else { return };
        let turn = self.pending.remove(position);
        let run = Some(run_id.to_owned());
        self.messages.push(Message {
            role: Role::User,
            content: turn.content,
            run_id: run.clone(),
            created_ms: turn.created_ms,
        });
        self.messages.push(Message {
            role: Role::Assistant,
            content: answer.to_owned(),
            run_id: run,
            created_ms: now_ms,
        });
        self.updated_ms = now_ms;
        self.trim(limits);
    }

    /// Drop the oldest turns (a message and its answer) beyond the limits.
    /// The newest turn always stays.
    fn trim(&mut self, limits: &Limits) {
        let over = |messages: &[Message]| {
            let bytes: usize = messages.iter().map(|message| message.content.len()).sum();
            (limits.max_messages > 0 && messages.len() > limits.max_messages) || bytes > MAX_CONVERSATION_BYTES
        };
        let mut drop = 0;
        while self.messages.len() - drop > 2 && over(self.messages.get(drop..).unwrap_or_default()) {
            drop += 2;
        }
        self.messages.drain(..drop.min(self.messages.len()));
    }
}

/// The first line of `content`, shortened, as a title.
fn title_of(content: &str) -> String {
    let line = content.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or_default();
    let mut title: String = line.chars().take(TITLE_CHARS).collect();
    if line.chars().count() > TITLE_CHARS {
        title.push('…');
    }
    title
}

/// What one user may store (`--conversation-max-messages`,
/// `--conversations-per-user`, `--conversation-ttl-days`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Messages kept per conversation (0 = unlimited).
    pub max_messages: usize,
    /// Conversations kept per end user (0 = unlimited).
    pub per_user: usize,
    /// Days after which an unused conversation is deleted (0 = never).
    pub ttl_days: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_messages: 200, per_user: 50, ttl_days: 30 }
    }
}

/// The conversations of a server.
#[derive(Debug)]
pub struct Conversations {
    store: Arc<dyn ConversationStore>,
    limits: Limits,
    next_id: AtomicU64,
}

impl Conversations {
    /// Conversations in `store_dir` (the run store directory), or in memory without one.
    ///
    /// @throws CliError (usage) - When the directory cannot be created.
    pub fn open(store_dir: Option<&Path>, limits: Limits) -> Result<Self, CliError> {
        let store: Arc<dyn ConversationStore> = match store_dir {
            Some(dir) => Arc::new(
                FileConversations::open(dir)
                    .map_err(|e| CliError::usage(format!("cannot keep conversations in `{}`: {e}", dir.display())))?,
            ),
            None => Arc::new(MemoryConversations::default()),
        };
        Ok(Self::with_store(store, limits))
    }

    /// Conversations in `store` (tests and embedders).
    pub fn with_store(store: Arc<dyn ConversationStore>, limits: Limits) -> Self {
        Self { store, limits, next_id: AtomicU64::new(1) }
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    pub(crate) fn store(&self) -> &Arc<dyn ConversationStore> {
        &self.store
    }

    /// One line for the startup log.
    pub fn describe(&self, durable: bool) -> String {
        let place = if durable { "in the run store" } else { "in memory (set --store to keep them)" };
        let ttl = match self.limits.ttl_days {
            0 => String::new(),
            days => format!(", deleted after {days} unused day(s)"),
        };
        format!("conversations kept {place} under /v1/conversations{ttl}")
    }

    /// A new id: unique in this process and ordered by creation time.
    fn new_id(&self) -> String {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        format!("conv-{nanos:x}-{n:x}")
    }

    /// Create a conversation with `workflow` for `owner`.
    ///
    /// @throws ConversationError - When the store fails.
    pub async fn create(
        &self,
        owner: Option<String>,
        workflow: &str,
        input: Option<String>,
        title: Option<String>,
    ) -> Result<Conversation, ConversationError> {
        let mut conversation = Conversation::new(&self.new_id(), owner, workflow, input, now_ms());
        conversation.title = title.as_deref().map(title_of).unwrap_or_default();
        self.store.insert(conversation.clone(), self.limits.per_user).await?;
        Ok(conversation)
    }

    /// The turn of run `run_id` in the conversation `id`, whose message is
    /// `content`: remembered until the run ends.
    ///
    /// @returns The conversation before this turn, or `None` when there is no such conversation.
    /// @throws ConversationError - When the store fails.
    pub async fn begin_turn(
        &self,
        id: &str,
        run_id: &str,
        content: &str,
    ) -> Result<Option<Conversation>, ConversationError> {
        let (run_id, content) = (run_id.to_owned(), content.to_owned());
        let now = now_ms();
        let changed = self.store.update(id, Box::new(move |c| c.begin_turn(&run_id, &content, now))).await?;
        Ok(changed)
    }

    /// Delete conversations unused for longer than the retention, hourly.
    pub fn spawn_retention(self: &Arc<Self>) {
        let days = self.limits.ttl_days;
        if days == 0 {
            return;
        }
        let store = Arc::clone(&self.store);
        graphrt_core::task::spawn(async move {
            let ttl_ms = days.saturating_mul(24 * 60 * 60 * 1000);
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60 * 60));
            loop {
                tick.tick().await;
                match store.purge_before(now_ms().saturating_sub(ttl_ms)).await {
                    Ok(0) => {}
                    Ok(n) => eprintln!("conversations: removed {n} unused for {days} day(s)"),
                    Err(e) => eprintln!("warning: conversation cleanup failed: {e}"),
                }
            }
        });
    }
}

/// How a run's execution ended, for its conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnEnd {
    /// The run succeeded: its answer becomes a turn.
    Succeeded,
    /// The run paused: the turn waits for the resume.
    Paused,
    /// The run failed or was aborted: the message is forgotten.
    Dropped,
}

/// The link from an executing run to the conversation its record names
/// (`session_id`). Runs that name none of ours cost one lookup when they end.
#[derive(Clone, Debug)]
pub struct Turn {
    conversations: Arc<Conversations>,
    id: String,
}

impl Turn {
    /// The turn of a run whose record names `session_id`.
    pub fn of(conversations: &Arc<Conversations>, session_id: &str) -> Self {
        Self { conversations: Arc::clone(conversations), id: session_id.to_owned() }
    }

    /// Record how the execution of `run_id` ended. `variables` are the
    /// `conversation` variables it wrote. Failures are logged: the run itself
    /// is finished and stored either way.
    pub async fn finish(
        &self,
        run_id: &str,
        end: TurnEnd,
        answer: &str,
        variables: IndexMap<String, serde_json::Value>,
    ) {
        let limits = self.conversations.limits;
        let (run, answer) = (run_id.to_owned(), answer.to_owned());
        let now = now_ms();
        let change = Box::new(move |conversation: &mut Conversation| {
            // Only a run that began a turn here may change the conversation: a
            // `session_id` that merely looks like one of our ids changes nothing.
            if !conversation.pending.iter().any(|turn| turn.run_id == run) {
                return;
            }
            match end {
                TurnEnd::Succeeded => {
                    conversation.variables.extend(variables);
                    conversation.complete_turn(&run, &answer, now, &limits);
                }
                TurnEnd::Paused => conversation.variables.extend(variables),
                TurnEnd::Dropped => conversation.pending.retain(|turn| turn.run_id != run),
            }
        });
        if let Err(e) = self.conversations.store.update(&self.id, change).await {
            eprintln!("warning: run {run_id}: cannot update conversation `{}`: {e}", self.id);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use serde_json::json;

    use super::*;

    fn conversations(limits: Limits) -> Arc<Conversations> {
        Arc::new(Conversations::with_store(Arc::new(MemoryConversations::default()), limits))
    }

    #[tokio::test]
    async fn a_succeeded_run_adds_its_turn_and_variables() {
        let all = conversations(Limits::default());
        let created = all.create(Some("u1".into()), "chat", None, None).await.unwrap();
        all.begin_turn(&created.id, "run-1", "What is a DAG?\nSecond line").await.unwrap().unwrap();
        let turn = Turn::of(&all, &created.id);
        let variables = IndexMap::from([("topic".to_owned(), json!("graphs"))]);
        turn.finish("run-1", TurnEnd::Succeeded, "A graph without cycles.", variables).await;
        let stored = all.store.get(&created.id).await.unwrap().unwrap();
        assert_eq!(stored.title, "What is a DAG?");
        assert_eq!(
            stored.history(),
            json!([
                { "role": "user", "content": "What is a DAG?\nSecond line" },
                { "role": "assistant", "content": "A graph without cycles." },
            ])
        );
        assert_eq!(stored.variables.get("topic"), Some(&json!("graphs")));
        assert!(stored.pending.is_empty());
        assert_eq!(stored.messages[1].run_id.as_deref(), Some("run-1"));
    }

    #[tokio::test]
    async fn a_failed_run_adds_nothing_and_a_paused_one_waits() {
        let all = conversations(Limits::default());
        let id = all.create(None, "chat", None, None).await.unwrap().id;
        all.begin_turn(&id, "run-1", "first").await.unwrap();
        all.begin_turn(&id, "run-2", "second").await.unwrap();
        let turn = Turn::of(&all, &id);
        turn.finish("run-1", TurnEnd::Dropped, "", IndexMap::new()).await;
        turn.finish("run-2", TurnEnd::Paused, "", IndexMap::from([("step".to_owned(), json!(1))])).await;
        let stored = all.store.get(&id).await.unwrap().unwrap();
        assert!(stored.messages.is_empty());
        assert_eq!(stored.pending.len(), 1, "the paused run still waits");
        assert_eq!(stored.variables.get("step"), Some(&json!(1)));
        turn.finish("run-2", TurnEnd::Succeeded, "done", IndexMap::new()).await;
        let stored = all.store.get(&id).await.unwrap().unwrap();
        assert_eq!(stored.messages.len(), 2);
        assert_eq!(stored.messages[0].content, "second");
    }

    #[tokio::test]
    async fn a_run_that_began_no_turn_changes_nothing() {
        let all = conversations(Limits::default());
        let id = all.create(None, "chat", None, None).await.unwrap().id;
        let turn = Turn::of(&all, &id);
        turn.finish("stranger", TurnEnd::Succeeded, "injected", IndexMap::from([("x".to_owned(), json!(1))])).await;
        let stored = all.store.get(&id).await.unwrap().unwrap();
        assert!(stored.messages.is_empty());
        assert!(stored.variables.is_empty());
        // And a turn of an unknown conversation is a no-op, not an error.
        Turn::of(&all, "conv-unknown").finish("r", TurnEnd::Succeeded, "", IndexMap::new()).await;
    }

    #[tokio::test]
    async fn the_oldest_turns_are_dropped_beyond_the_limits() {
        let limits = Limits { max_messages: 4, ..Limits::default() };
        let all = conversations(limits);
        let id = all.create(None, "chat", None, None).await.unwrap().id;
        let turn = Turn::of(&all, &id);
        for n in 1..=3 {
            let run = format!("run-{n}");
            all.begin_turn(&id, &run, &format!("question {n}")).await.unwrap();
            turn.finish(&run, TurnEnd::Succeeded, &format!("answer {n}"), IndexMap::new()).await;
        }
        let stored = all.store.get(&id).await.unwrap().unwrap();
        let texts: Vec<&str> = stored.messages.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(texts, ["question 2", "answer 2", "question 3", "answer 3"]);
    }

    #[test]
    fn one_huge_turn_stays_whole() {
        let mut conversation = Conversation::new("c", None, "chat", None, 1);
        conversation.begin_turn("r", &"x".repeat(MAX_CONVERSATION_BYTES + 1), 2);
        conversation.complete_turn("r", "ok", 3, &Limits::default());
        assert_eq!(conversation.messages.len(), 2, "the newest turn is never dropped");
    }

    #[test]
    fn pending_turns_are_bounded() {
        let mut conversation = Conversation::new("c", None, "chat", None, 1);
        for n in 0..MAX_PENDING_TURNS + 5 {
            conversation.begin_turn(&format!("r{n}"), "hi", 2);
        }
        assert_eq!(conversation.pending.len(), MAX_PENDING_TURNS);
        assert_eq!(conversation.pending[0].run_id, "r5");
    }

    #[test]
    fn owners_reach_their_own_and_the_key_reaches_all() {
        let conversation = Conversation::new("c", Some("u1".into()), "chat", None, 1);
        assert!(conversation.is_reachable_by(Some("u1")));
        assert!(!conversation.is_reachable_by(Some("u2")));
        assert!(conversation.is_reachable_by(None));
        let keyed = Conversation::new("c", None, "chat", None, 1);
        assert!(!keyed.is_reachable_by(Some("u1")), "a user never reaches a conversation of the key");
    }

    #[test]
    fn titles_are_one_short_line() {
        assert_eq!(title_of("\n  Hello there  \nmore"), "Hello there");
        let long = "é".repeat(TITLE_CHARS + 10);
        let title = title_of(&long);
        assert_eq!(title.chars().count(), TITLE_CHARS + 1);
        assert!(title.ends_with('…'));
    }
}
